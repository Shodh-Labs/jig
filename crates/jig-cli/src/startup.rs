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

/// How long the credential probe waits for the child to exit or answer before
/// declaring a hang.
///
/// Deliberately short. The probe runs only *after* a connect has already failed
/// with the caller's own (much longer) timeout, so the server has demonstrably
/// had its chance; this window only has to distinguish "died and we are reaping
/// it" from "is going to sit here forever". The census's two hanging servers
/// hang indefinitely, not for four seconds.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// How many stderr lines the probe retains — matched to the transport's own
/// ring (`STDERR_TAIL_LINES`) so the two surfaces see the same evidence.
const STDERR_TAIL_LINES: usize = 20;

/// Maximum characters kept per retained stderr line, so one runaway line cannot
/// dominate the buffer.
const STDERR_LINE_MAX: usize = 1024;

/// Run the `npx` pre-warm pass for `program`/`args`, returning how long it took.
///
/// Returns `None` — meaning "install is `n/a`" — when the command is not
/// `npx`-shaped, when no package can be identified, or when the pre-warm pass
/// itself fails. A failed pre-warm is deliberately *not* an error: the real
/// launch is about to happen anyway and will report the true problem, and a
/// timing refinement must never be able to fail a check that would otherwise
/// pass.
pub(crate) async fn prewarm(program: &str, args: &[String]) -> Option<Duration> {
    let package = boot::npx_package(program, args)?;
    let t0 = Instant::now();
    // Resolve through the transport's own helper: on Windows `npx` is an
    // `npx.cmd` shim that `CreateProcess` will not find by bare name, and a
    // pre-warm that silently fails to spawn would report `install n/a` while
    // quietly folding the entire download back into `boot`.
    let status = Command::new(jig_core::transport::resolve_program(program))
        .args(boot::prewarm_args(&package))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()?;
    status.success().then(|| t0.elapsed())
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

    // Drain stderr concurrently: a child that fills its stderr pipe while we
    // wait on exit would deadlock, which is precisely the hang we are trying to
    // measure and would misattribute it to the server.
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut line = line;
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
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if v.get("result").is_some() {
                        return true;
                    }
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

    let (exit_code, hung) = match tokio::time::timeout(PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        // Waiting itself failed — we know nothing about how it ended.
        Ok(Err(_)) => (None, false),
        Err(_) => {
            let _ = child.start_kill();
            (None, true)
        }
    };
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
