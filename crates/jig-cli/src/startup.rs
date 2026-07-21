//! Live measurement for the two `rubric-v1.3` startup rules: the install/boot
//! split (SOP 25, [`jig_core::boot`]) and credential-failure UX (SOP 26,
//! [`jig_core::credential`]).
//!
//! Both grading rules are pure functions living in `jig-core`. This module is
//! the small, deliberately-dumb I/O layer that gathers their inputs: it runs a
//! pre-warm pass, and — when a stdio server refuses to start — re-launches it
//! once under observation to find out *how* it refused. Keeping the process
//! work here means the scoring engine stays pure and snapshot-lockable.

use std::process::Stdio;
use std::time::{Duration, Instant};

use jig_core::{boot, StartupObservation};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// How long the credential probe waits **after the child's first byte of
/// output** for it to exit or answer, before declaring a hang.
///
/// Deliberately short. The probe runs only *after* a connect has already failed
/// with the caller's own (much longer) timeout, so the server has demonstrably
/// had its chance; this window only has to distinguish "died and we are reaping
/// it" from "is going to sit here forever". The census's two hanging servers
/// hang indefinitely, not for four seconds.
///
/// # Why the window starts at first output, not at spawn (`rubric-v1.4`)
///
/// Under `rubric-v1.3` this window was measured **from spawn**, which made the
/// most severe verdict in the rubric — [`Verdict::Hung`](jig_core::credential::Verdict::Hung), High severity,
/// robustness 0, *"never exited and never answered"* — reachable by a **cold
/// npm cache**. The `npx` shim alone costs ~2.6s before the server's own code
/// runs (`rubric-v1.3`, SOP 25, measured), leaving well under 1.4s of the old
/// 4s window for the server to speak. In the 50-server fleet run:
///
/// | Server | Exit | Time from spawn | `v1.3` verdict | Correct verdict |
/// |:-------|-----:|----------------:|:---------------|:----------------|
/// | `server-slack` | 1, named `SLACK_BOT_TOKEN` | 3.83s / 3.86s | **Hung** | PASS |
/// | `server-gitlab` | 1, named the variable | 5.19s / 7.84s | **Hung** | PASS |
///
/// Both are the rubric's own PASS shape, and both were recorded as the worst
/// verdict it can reach. It was also **non-deterministic**: `server-gitlab`
/// flipped to PASS on a warm npm cache, so the same server graded differently on
/// the same machine depending on whether someone had run it that week. 13 of the
/// 24 fleet failures were affected.
///
/// Measuring from the first byte the child writes subtracts the launcher cost
/// without having to model it: whatever `npx` spent resolving and spawning is,
/// by construction, over by the time the server's own process emits anything.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// The absolute ceiling on the whole probe, measured from spawn.
///
/// [`PROBE_TIMEOUT`] cannot bound a child that never writes a single byte, so
/// this backstop does. It is set well above the worst launcher cost the fleet
/// run measured (`server-gitlab`, 7.84s from spawn to exit on a cold cache) plus
/// a full [`PROBE_TIMEOUT`], so a cold cache alone can never consume it — which
/// is the property `rubric-v1.4` is buying. A child that is still silent after
/// 30 seconds is hanging by any definition a user would accept.
const PROBE_HARD_CAP: Duration = Duration::from_secs(30);

/// How many stderr lines the probe retains — matched to the transport's own
/// ring (`STDERR_TAIL_LINES`) so the two surfaces see the same evidence.
const STDERR_TAIL_LINES: usize = 20;

/// Maximum characters kept per retained stderr line, so one runaway line cannot
/// dominate the buffer.
const STDERR_LINE_MAX: usize = 1024;

/// Run the `npx` pre-warm pass for `program`/`args`, returning **(install,
/// launcher floor)**.
///
/// The pass runs **twice** (`rubric-v1.4`). The first run resolves and downloads
/// the package tree and is timed as *install*. The second run does exactly the
/// same thing against a cache that is now warm — `npx --yes --package <pkg> --
/// node -e ""`, a null program through the identical `npx` path — and is timed
/// as the **launcher floor**: the cost of getting *anything at all* started
/// through this shim on this machine, right now.
///
/// That second number is the one `rubric-v1.3` said it could not defensibly
/// produce. Its changelog declined to subtract the ~2.6s shim overhead because
/// *"measuring it would require timing a null server through the same path on
/// every run"*. This is that measurement. It costs one extra warm-cache spawn on
/// `npx` targets only, and it is what lets robustness vary between servers
/// instead of scoring exactly 80 for all of them — see
/// [`jig_core::boot::Timing::server_boot`].
///
/// Either half is `None` when the command is not `npx`-shaped, when no package
/// can be identified, or when that pass failed. A failed pass is deliberately
/// *not* an error: the real launch is about to happen anyway and will report the
/// true problem, and a timing refinement must never be able to fail a check that
/// would otherwise pass. A missing launcher floor simply means nothing is
/// subtracted, which is the `rubric-v1.3` behaviour and the safe direction.
pub(crate) async fn prewarm(
    program: &str,
    args: &[String],
) -> (Option<Duration>, Option<Duration>) {
    let Some(package) = boot::npx_package(program, args) else {
        return (None, None);
    };
    let install = timed_prewarm_pass(program, &package).await;
    if install.is_none() {
        // The install pass failed, so the cache is not warm and a second pass
        // would time a download rather than a launcher. Measuring nothing is
        // better than reporting a floor that is really an install.
        return (None, None);
    }
    let launcher = timed_prewarm_pass(program, &package).await;
    (install, launcher)
}

/// One pre-warm pass, timed. `None` when it could not be run or did not succeed.
async fn timed_prewarm_pass(program: &str, package: &str) -> Option<Duration> {
    let t0 = Instant::now();
    // Resolve through the transport's own helper: on Windows `npx` is an
    // `npx.cmd` shim that `CreateProcess` will not find by bare name, and a
    // pre-warm that silently fails to spawn would report `install n/a` while
    // quietly folding the entire download back into `boot`.
    let status = Command::new(jig_core::transport::resolve_program(program))
        .args(boot::prewarm_args(package))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()?;
    status.success().then(|| t0.elapsed())
}

/// Whether a line of the child's stdout is a JSON-RPC **response** — a
/// well-formed JSON object carrying a `result` member. A server that produces
/// one answered `initialize` and therefore did not fail to start, so this is the
/// guard that keeps the credential-UX rule from grading a server that was fine.
///
/// Anything that is not parseable JSON, or is JSON without a `result` key (an
/// `error` reply, a log line the server wrote as JSON, a bare array), is *not*
/// an answer. A `result` of `null` still counts: the key's presence is the
/// signal, matching the JSON-RPC response shape rather than its payload.
fn is_initialize_answer(line: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(line),
        Ok(value) if value.get("result").is_some()
    )
}

/// Append one stderr line to the retained tail: truncate it to
/// [`STDERR_LINE_MAX`] characters so a single runaway line cannot dominate the
/// buffer, then keep only the last [`STDERR_TAIL_LINES`] lines. Truncation is by
/// character, never by byte, so a multi-byte line is never split mid-codepoint.
fn retain_stderr_line(lines: &mut Vec<String>, mut line: String) {
    line.truncate(
        line.char_indices()
            .nth(STDERR_LINE_MAX)
            .map_or(line.len(), |(i, _)| i),
    );
    lines.push(line);
    if lines.len() > STDERR_TAIL_LINES {
        lines.remove(0);
    }
}

/// The probe's **sliding** deadline (`rubric-v1.4`). Before the child's first
/// byte the only bound is [`PROBE_HARD_CAP`] measured from spawn; once it has
/// spoken the bound becomes [`PROBE_TIMEOUT`] measured from that instant, so the
/// launcher's cost is subtracted by construction rather than estimated.
fn probe_deadline(first_output_at: Option<Instant>, spawned_at: Instant) -> Instant {
    match first_output_at {
        Some(t) => t + PROBE_TIMEOUT,
        None => spawned_at + PROBE_HARD_CAP,
    }
}

/// Re-launch a stdio server that failed to connect, and observe **how** it
/// failed: did it exit, with what status, and did its stderr name an
/// environment variable?
///
/// This costs one extra process spawn, and only on a server that has already
/// failed — the path where the user is stuck and the diagnosis is worth most.
/// The child is sent a well-formed `initialize` request so that a server which
/// *would* have started is not misdiagnosed as a hang: if it answers, the caller
/// sees a normal exchange rather than a timeout.
///
/// Returns `None` when nothing can honestly be said — the program could not be
/// spawned at all, or the server *did* answer `initialize` and therefore did not
/// fail to start. `None` means "not observed", never "observed and fine".
pub(crate) async fn probe_credential_failure(
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> Option<StartupObservation> {
    let mut command = Command::new(jig_core::transport::resolve_program(program));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        command.env(k, v);
    }

    let Ok(mut child) = command.spawn() else {
        // The program does not exist or is not executable. That is a launcher
        // problem, not a credential-UX problem, so report nothing observed
        // rather than inventing a verdict about a process that never ran.
        return None;
    };

    // The instant the child first says anything on either pipe. Until it fires,
    // everything elapsing is launcher cost (the `npx` shim resolving and
    // spawning), which is not the server's and must not be charged to it. Both
    // reader tasks hold a sender; the first to speak wins and the rest are
    // no-ops. See [`PROBE_TIMEOUT`].
    let spawned_at = Instant::now();
    let (first_output_tx, mut first_output_rx) = tokio::sync::watch::channel(false);
    let stderr_first = first_output_tx.clone();

    // Drain stderr concurrently: a child that fills its stderr pipe while we
    // wait on exit would deadlock, which is precisely the hang we are trying to
    // measure and would misattribute it to the server.
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = stderr_first.send(true);
                retain_stderr_line(&mut lines, line);
            }
            lines
        })
    });

    // Watch stdout for an `initialize` response. This is the guard that keeps
    // the rule honest: a server that *answers* did not fail to start, whatever
    // went wrong afterwards, and grading it on credential UX would be a
    // fabricated finding. Only a server that never answers is graded here.
    let answered = child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = first_output_tx.send(true);
                if is_initialize_answer(&line) {
                    return true;
                }
            }
            false
        })
    });

    // Give the server something legitimate to answer, so a slow-but-working
    // server cannot be mistaken for one that hangs on a missing credential.
    // `stdin` is held open for the whole probe rather than dropped: closing it
    // would send EOF, and a correct server exits 0 on EOF — which this rule
    // would then have to read as "exited zero after a failed start". Holding it
    // open makes a hang mean a genuine hang.
    let mut stdin = child.stdin.take();
    if let Some(pipe) = stdin.as_mut() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": jig_core::LATEST_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "jig", "version": env!("CARGO_PKG_VERSION") },
            }
        });
        let _ = pipe.write_all(format!("{request}\n").as_bytes()).await;
        let _ = pipe.flush().await;
    }

    // Wait on a **sliding** deadline (`rubric-v1.4`). Before the child's first
    // byte the only bound is `PROBE_HARD_CAP` from spawn; once it speaks, the
    // deadline becomes `PROBE_TIMEOUT` from that instant. The launcher's cost is
    // therefore subtracted by construction rather than estimated, and a cold npm
    // cache can no longer manufacture a `Hung` verdict. See [`PROBE_TIMEOUT`].
    let (exit_code, hung) = {
        let wait = child.wait();
        tokio::pin!(wait);
        let mut first_output_at: Option<Instant> = None;
        // Whether the first-output watch can still deliver. Once every sender is
        // gone the branch would be permanently ready, which would spin the
        // select; disabling it leaves the hard cap as the only bound, which is
        // the right answer for a child that produced no output at all.
        let mut watching = true;
        loop {
            let deadline = probe_deadline(first_output_at, spawned_at);
            tokio::select! {
                // Biased so an exit that lands in the same tick as the deadline
                // is read as an exit. A server that exited is never a hang.
                biased;
                result = &mut wait => break match result {
                    Ok(status) => (status.code(), false),
                    // Waiting itself failed — we know nothing about how it ended.
                    Err(_) => (None, false),
                },
                changed = first_output_rx.changed(), if watching => {
                    match changed {
                        Ok(()) if *first_output_rx.borrow() => {
                            first_output_at = Some(Instant::now());
                            watching = false;
                        }
                        Ok(()) => {}
                        Err(_) => watching = false,
                    }
                }
                () = tokio::time::sleep_until(deadline.into()) => break (None, true),
            }
        }
    };
    if hung {
        let _ = child.start_kill();
    }
    drop(stdin);

    // A server that answered `initialize` started fine. Report nothing observed
    // rather than a verdict this rule is not entitled to reach.
    if let Some(task) = answered {
        if task.await.unwrap_or(false) {
            return None;
        }
    }

    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    };

    Some(StartupObservation {
        exit_code,
        hung,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_initialize_answer: the "did not fail to start" guard ------------

    /// A JSON-RPC response carries a `result` member, and only that shape counts
    /// as an answer — the presence of the key, not its payload.
    #[test]
    fn a_json_object_with_a_result_member_is_an_answer() {
        assert!(is_initialize_answer(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#
        ));
        // `result: null` is still a response — the key's presence is the signal.
        assert!(is_initialize_answer(r#"{"id":1,"result":null}"#));
    }

    /// The guard must not fire on anything that is not a `result`-bearing object,
    /// or a slow-but-working server could never be told apart from a hang and an
    /// `error` reply would be misread as a successful start.
    #[test]
    fn non_result_output_is_never_mistaken_for_an_answer() {
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no"}}"#, // an error reply
            r#"{"jsonrpc":"2.0","method":"log","params":{}}"#,                    // a notification
            r#"{"result_code": 7}"#,   // a near-miss key, not `result`
            "[1,2,3]",                 // valid JSON, but not an object
            "\"result\"",              // the word as a bare JSON string
            "GITHUB_TOKEN is not set", // plain stderr-style text
            "",                        // empty line
            "{ not json",              // unparseable
        ] {
            assert!(!is_initialize_answer(line), "wrongly an answer: {line:?}");
        }
    }

    // ---- retain_stderr_line: truncation + the ring buffer -------------------

    #[test]
    fn a_short_stderr_line_is_retained_verbatim() {
        let mut lines = Vec::new();
        retain_stderr_line(&mut lines, "MOCK_API_KEY is not set".to_string());
        assert_eq!(lines, vec!["MOCK_API_KEY is not set".to_string()]);
    }

    /// A runaway line is capped at `STDERR_LINE_MAX` characters so it cannot
    /// dominate the buffer — and the cap is by character, so a multi-byte line is
    /// never split mid-codepoint (a byte-wise truncate would panic).
    #[test]
    fn an_overlong_stderr_line_is_truncated_by_character_not_byte() {
        let mut lines = Vec::new();
        // Each `€` is three bytes, so a byte-wise cap at 1024 would land inside a
        // codepoint; a char-wise cap keeps exactly STDERR_LINE_MAX characters.
        retain_stderr_line(&mut lines, "€".repeat(STDERR_LINE_MAX + 50));
        assert_eq!(lines[0].chars().count(), STDERR_LINE_MAX);

        // A line at the boundary is left exactly as-is.
        let mut lines = Vec::new();
        let exact = "a".repeat(STDERR_LINE_MAX);
        retain_stderr_line(&mut lines, exact.clone());
        assert_eq!(lines[0], exact);
    }

    /// The buffer is a ring of the last `STDERR_TAIL_LINES`: the newest lines are
    /// kept and the oldest fall off, so the retained tail matches the transport's
    /// own ring rather than growing without bound.
    #[test]
    fn the_stderr_ring_keeps_only_the_newest_lines() {
        let mut lines = Vec::new();
        for i in 0..(STDERR_TAIL_LINES + 5) {
            retain_stderr_line(&mut lines, i.to_string());
        }
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        // The oldest five (0..5) were evicted; the tail is 5..=24, oldest first.
        assert_eq!(lines.first().unwrap(), "5");
        assert_eq!(lines.last().unwrap(), &(STDERR_TAIL_LINES + 4).to_string());
    }

    // ---- probe_deadline: the rubric-v1.4 sliding window ---------------------

    /// Before the child speaks, the only bound is the hard cap from spawn; once
    /// it speaks the bound slides to `PROBE_TIMEOUT` from *that* instant, which is
    /// how the launcher's cost is subtracted rather than estimated.
    #[test]
    fn the_deadline_slides_from_the_hard_cap_to_a_window_after_first_output() {
        let spawned = Instant::now();
        // No output yet: bounded only by the hard cap, measured from spawn.
        assert_eq!(probe_deadline(None, spawned), spawned + PROBE_HARD_CAP);

        // Once output arrives, the deadline is PROBE_TIMEOUT after that instant
        // and no longer depends on the spawn time at all.
        let first_output = spawned + Duration::from_secs(9);
        assert_eq!(
            probe_deadline(Some(first_output), spawned),
            first_output + PROBE_TIMEOUT
        );
        // A different spawn time cannot move the post-output deadline.
        assert_eq!(
            probe_deadline(Some(first_output), spawned + Duration::from_secs(3)),
            first_output + PROBE_TIMEOUT
        );
    }

    // ---- the process-spawning entry points ----------------------------------

    /// A program that cannot even be spawned is a launcher problem, not a
    /// credential-UX one: the probe reports **nothing observed** (`None`) rather
    /// than inventing a verdict about a process that never ran.
    #[tokio::test]
    async fn probing_a_program_that_cannot_spawn_observes_nothing() {
        let observation =
            probe_credential_failure("jig-nonexistent-program-zzz", &["--any".to_string()], &[])
                .await;
        assert!(
            observation.is_none(),
            "a program that never ran must not be graded: {observation:?}"
        );
    }

    /// A non-`npx` command has nothing to pre-warm, so `prewarm` short-circuits
    /// to `(None, None)` *without spawning anything* — the guard that keeps the
    /// timing refinement off every non-npx target.
    #[tokio::test]
    async fn prewarm_short_circuits_for_a_non_npx_command() {
        let (install, launcher) = prewarm("node", &["server.js".to_string()]).await;
        assert_eq!(install, None);
        assert_eq!(launcher, None);
    }
}
