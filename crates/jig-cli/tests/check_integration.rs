//! End-to-end integration tests for `jig check`: spawn the real `jig` binary
//! against the real `jig-mock-server` and assert on the rendered report card,
//! exit codes, and machine outputs.
//!
//! The `jig` binary path comes from Cargo as `CARGO_BIN_EXE_jig` (this crate
//! defines it). The mock-server binary is its sibling in the same target
//! directory (built by `cargo test --workspace --all-targets`).
//!
//! Latency is machine-dependent, so the human-report snapshots redact any
//! `<n>ms` token; every other byte of the report is deterministic and locked.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The freshly built `jig` binary under test.
fn jig_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig"))
}

/// The `jig-mock-server` binary: a sibling of `jig` in the target dir.
fn mock_bin() -> PathBuf {
    let mut p = jig_bin();
    let name = if cfg!(windows) {
        "jig-mock-server.exe"
    } else {
        "jig-mock-server"
    };
    p.set_file_name(name);
    assert!(
        p.exists(),
        "mock-server binary not found at {} — run with `cargo test --workspace --all-targets`",
        p.display()
    );
    p
}

/// The `--stdio` value that launches the mock: the (space-containing) path must
/// be double-quoted so Jig's command splitter keeps it a single token.
fn stdio_arg(extra: &str) -> String {
    let path = mock_bin();
    if extra.is_empty() {
        format!("\"{}\"", path.display())
    } else {
        format!("\"{}\" {extra}", path.display())
    }
}

/// Run `jig check` with the given trailing args against the mock (optionally
/// passing `extra` flags to the mock itself).
fn run_check(mock_extra: &str, args: &[&str]) -> Output {
    Command::new(jig_bin())
        .arg("check")
        .arg("--stdio")
        .arg(stdio_arg(mock_extra))
        .args(args)
        .output()
        .expect("spawn jig check")
}

/// Run `jig check` against the mock with `dir` as the working directory, so the
/// default report is written into an isolated temp dir rather than the source
/// tree.
fn run_check_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(jig_bin())
        .arg("check")
        .arg("--stdio")
        .arg(stdio_arg(""))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn jig check")
}

/// A fresh, unique temp directory for a report-writing test.
fn temp_cwd(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("jig-check-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&p).expect("create temp cwd");
    p
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The process's stderr as text. The `rubric-v1.3` credential-UX verdicts are
/// emitted on the error path, so they land here rather than on stdout.
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Redact machine-dependent latency so the human report is snapshot-stable.
///
/// Two shapes are machine-dependent: `<n>ms` (list latency, and the boot
/// sub-score line) and `<n.n>s` — the install/boot split added in
/// `rubric-v1.3`, whose seconds figure depends on process-spawn speed.
fn redact_latency(s: &str) -> String {
    // Replace "<n>ms" with "<ms>" and "<n.n>s" with "<s>".
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for a run of ASCII digits (optionally with one decimal point)
        // followed by "ms" or "s".
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut decimal_end = i;
            if i < bytes.len() && bytes[i] == b'.' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    decimal_end = j;
                }
            }
            if s[i..].starts_with("ms") {
                out.push_str("<ms>");
                i += 2; // skip "ms"
                continue;
            }
            // A decimal-seconds figure, e.g. "0.5s" in the install/boot line.
            if decimal_end > i && s[decimal_end..].starts_with('s') {
                out.push_str("<s>");
                i = decimal_end + 1;
                continue;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Redact the machine-dependent path to the mock binary. The report states the
/// invocation it measured, and for these tests that is an absolute path into
/// the target directory — stable in meaning, not in bytes.
fn redact_mock_path(s: &str) -> String {
    s.replace(&mock_bin().display().to_string(), "<mock>")
}

/// Both redactions, in the order the snapshots need: the path first (it may
/// contain digits that the latency pass would otherwise chew on).
fn redact(s: &str) -> String {
    redact_latency(&redact_mock_path(s))
}

// ---------------------------------------------------------------------------
// Clean server: a high score, exit 0, human report snapshot.
// ---------------------------------------------------------------------------

#[test]
fn clean_server_scores_high_and_exits_zero() {
    let out = run_check("", &["--no-report"]);
    assert!(out.status.success(), "clean check should exit 0");
    let report = stdout(&out);
    assert!(report.contains("grade A"), "expected grade A: {report}");
    assert!(report.contains("Protocol compliance  100"));
    insta::assert_snapshot!("check_e2e_clean", redact(&report));
}

// ---------------------------------------------------------------------------
// Issue #6, option 1: the report states the invocation it measured, so a reader
// can tell the score describes that command line and not the package.
// ---------------------------------------------------------------------------

#[test]
fn the_human_report_states_the_invocation_that_was_actually_run() {
    let out = run_check("--pollute-stdout", &["--no-report"]);
    let report = stdout(&out);
    let expected = format!("measured: {} --pollute-stdout", mock_bin().display());
    assert!(
        report.contains(&expected),
        "the header must state the exact invocation ({expected}):\n{report}"
    );
}

#[test]
fn the_json_report_carries_the_invocation_that_was_actually_run() {
    let out = run_check("--pollute-stdout", &["--json", "--no-report"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON report");
    assert_eq!(
        v["invocation"],
        serde_json::Value::String(format!("{} --pollute-stdout", mock_bin().display()))
    );
}

#[test]
fn the_html_report_carries_the_invocation_that_was_actually_run() {
    let dir = temp_cwd("invocation-html");
    let out = run_check_in(&dir, &["--no-prewarm"]);
    assert!(out.status.success());
    let html = std::fs::read_to_string(dir.join("jig-report-jig-mock-server.html"))
        .expect("the report file is written in human mode");
    assert!(
        html.contains("measured: <span class=\"mono\">"),
        "the HTML header must name what was measured"
    );
    assert!(
        html.contains(&jig_cli_html_escape(&mock_bin().display().to_string())),
        "the HTML header must carry the invocation itself:\n{html}"
    );
}

/// The five-character HTML escape the report applies, mirrored here so the
/// assertion compares like with like (a Windows path contains no escapable
/// characters, but a developer checkout path may).
fn jig_cli_html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ---------------------------------------------------------------------------
// Pollution fixture: the protocol deduction + finding appear.
// ---------------------------------------------------------------------------

#[test]
fn stdout_pollution_deducts_protocol_and_names_the_finding() {
    let out = run_check("--pollute-stdout", &["--no-report"]);
    // Still succeeds (no --min-score gate); the deduction is in the report.
    assert!(out.status.success());
    let report = stdout(&out);
    assert!(
        report.contains("Protocol compliance   85"),
        "protocol should drop to 85 for one polluting line: {report}"
    );
    assert!(
        report.contains("non-protocol line(s) on stdout"),
        "the pollution finding must appear: {report}"
    );
    insta::assert_snapshot!("check_e2e_pollution", redact(&report));
}

// ---------------------------------------------------------------------------
// The CI gate: --min-score above the score exits nonzero.
// ---------------------------------------------------------------------------

#[test]
fn min_score_gate_fails_below_threshold() {
    // 101 is unreachable by construction, so this asserts the *gate*, not a
    // particular mock-server score. Under `rubric-v1.1` the threshold was 99 and
    // the mock scored 97; `rubric-v1.2` lifts the mock to exactly 99 (lighter
    // rate weights on the near-universal title/annotation classes, plus
    // small-surface shrinkage over its 3 tools), which would have made a
    // threshold of 99 pass and silently stop testing the gate at all.
    let out = run_check("", &["--min-score", "101", "--no-report"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "score below --min-score 101 must exit 1"
    );
    // A passing floor exits 0.
    let ok = run_check("", &["--min-score", "80", "--no-report"]);
    assert!(ok.status.success(), "the clean score clears --min-score 80");
}

// ---------------------------------------------------------------------------
// --badge emits shields.io endpoint JSON.
// ---------------------------------------------------------------------------

#[test]
fn badge_emits_shields_endpoint_json() {
    let out = run_check("", &["--badge"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("badge is JSON");
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["label"], "jig score");
    assert_eq!(v["color"], "brightgreen");
    assert!(v["message"].as_str().unwrap().parse::<u32>().unwrap() >= 90);
}

// ---------------------------------------------------------------------------
// --json carries the full structured report.
// ---------------------------------------------------------------------------

#[test]
fn json_output_has_dimensions_weights_and_rubric_version() {
    let out = run_check("", &["--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON report");
    assert_eq!(v["rubricVersion"], "rubric-v1.5");
    // The bundled census now engages by default (M7 #4), so context cost is
    // scored against the ecosystem and labelled bundled.
    assert_eq!(v["contextCost"]["provenance"]["type"], "percentile");
    assert_eq!(v["contextCost"]["provenance"]["bundled"], true);
    let dims = v["dimensions"].as_array().unwrap();
    assert_eq!(dims.len(), 5);
    let weights: u64 = dims.iter().map(|d| d["weight"].as_u64().unwrap()).sum();
    assert_eq!(weights, 100, "dimension weights must sum to 100");
    // Every dimension carries a score and a label.
    for d in dims {
        assert!(d["label"].is_string());
        assert!(d["weight"].is_u64());
    }
}

// ---------------------------------------------------------------------------
// Percentiles dataset: bundled by default, `none` opts out, a bad path errors.
// ---------------------------------------------------------------------------

#[test]
fn percentiles_none_forces_absolute_bands() {
    let out = run_check("", &["--json", "--percentiles", "none"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(
        v["contextCost"]["provenance"]["type"], "absolute_bands",
        "`--percentiles none` must opt out of the bundled census"
    );
}

#[test]
fn explicit_missing_percentiles_file_is_an_error() {
    let out = run_check("", &["--percentiles", "no/such/file.json", "--no-report"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing explicit file must fail"
    );
}

// ---------------------------------------------------------------------------
// The HTML report: default-on in human mode, --no-report, --report <file>.
// ---------------------------------------------------------------------------

#[test]
fn human_mode_writes_report_by_default_and_announces_it() {
    let dir = temp_cwd("default");
    let out = run_check_in(&dir, &[]);
    assert!(out.status.success());
    let path = dir.join("jig-report-jig-mock-server.html");
    assert!(path.exists(), "human mode must write the report by default");
    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.contains("<title>Jig Report Card"));
    assert!(html.contains("MCP server report card"));
    assert!(
        stdout(&out).contains("report: ./jig-report-jig-mock-server.html"),
        "the report path must be announced on stdout: {}",
        stdout(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_report_suppresses_the_file() {
    let dir = temp_cwd("suppress");
    let out = run_check_in(&dir, &["--no-report"]);
    assert!(out.status.success());
    assert!(
        !dir.join("jig-report-jig-mock-server.html").exists(),
        "--no-report must not write a file"
    );
    assert!(!stdout(&out).contains("report:"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn report_flag_sets_an_explicit_path() {
    let dir = temp_cwd("explicit");
    let custom = dir.join("card.html");
    let out = run_check_in(&dir, &["--report", custom.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(custom.exists(), "--report <file> must write to that path");
    assert!(
        !dir.join("jig-report-jig-mock-server.html").exists(),
        "--report must not also write the default file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_mode_writes_no_report_unless_asked() {
    let dir = temp_cwd("json-default");
    let out = run_check_in(&dir, &["--json"]);
    assert!(out.status.success());
    assert!(
        !dir.join("jig-report-jig-mock-server.html").exists(),
        "machine mode must not write a report without --report"
    );
    // stdout is still clean JSON.
    serde_json::from_str::<serde_json::Value>(&stdout(&out)).expect("json stays valid");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_mode_with_report_flag_writes_file_and_keeps_json_clean() {
    let dir = temp_cwd("json-report");
    let custom = dir.join("card.html");
    let out = run_check_in(&dir, &["--json", "--report", custom.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(
        custom.exists(),
        "--report writes a file even in --json mode"
    );
    // The announcement goes to stderr, so stdout is still parseable JSON.
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("stdout stays valid JSON");
    assert_eq!(v["rubricVersion"], "rubric-v1.5");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// rubric-v1.3: tool poisoning (SOP 12)
// ---------------------------------------------------------------------------

/// The `--poisoned` fixture carries one instance of every shape the lint
/// detects. End-to-end over a real handshake, all five must surface.
#[test]
fn poisoned_tool_set_is_detected_end_to_end() {
    let out = run_check("--poisoned", &["--json", "--no-prewarm"]);
    assert!(out.status.success(), "a poisoned server still scores");
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON report");
    let findings = v["injection"].as_array().expect("injection array");
    let text = serde_json::to_string(findings).expect("serializable");

    for shape in [
        "ignore all previous instructions", // model-directed imperative
        "role/instruction tags",            // fake conversation turn
        "zero-width",                       // hidden characters
        "bidirectional",                    // Trojan Source
        "outbound-transfer verb",           // exfiltration shape
        "readOnlyHint",                     // annotation contradiction
        "named as a read",                  // name/behaviour mismatch
    ] {
        assert!(text.contains(shape), "missing detection: {shape}\n{text}");
    }

    // Every one of them cites its evidence and carries a fix.
    for f in findings {
        assert_eq!(f["dimension"], "injection");
        assert!(!f["fix"].as_str().expect("fix").is_empty());
        assert_eq!(f["points"], 0.0, "injection findings never score");
    }
}

/// Reported, never scored: the poisoned server's grade is driven entirely by the
/// five rubric dimensions. This is the guarantee that lets the lint ship in
/// report-only posture without silently re-grading the ecosystem.
#[test]
fn poisoning_does_not_move_the_grade() {
    let out = run_check("--poisoned", &["--json", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(!v["injection"].as_array().expect("array").is_empty());
    assert_eq!(v["protocolCap"], serde_json::Value::Null);
    // Protocol is clean, so the composite is a plain weighted mean and the
    // server still grades well despite being flagrantly poisoned.
    assert_eq!(v["dimensions"][0]["score"], 100);
}

/// A clean server produces an empty injection list — the false-positive bar,
/// enforced end-to-end rather than only in the unit corpus.
#[test]
fn a_clean_server_reports_no_poisoning() {
    let out = run_check("", &["--json", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(v["injection"].as_array().expect("array").is_empty());
}

/// The poisoning section is visible in the human report and reaches "Top fixes"
/// — pinned, so a large tool surface can never bury it.
#[test]
fn poisoning_surfaces_in_the_human_report_and_top_fixes() {
    let out = run_check("--poisoned", &["--no-report", "--no-prewarm"]);
    let text = stdout(&out);
    assert!(text.contains("Tool poisoning (unscored)"), "{text}");
    let top = text.split("Top fixes").nth(1).expect("a Top fixes section");
    assert!(
        top.contains("[injection]"),
        "not pinned into Top fixes:\n{top}"
    );
}

// ---------------------------------------------------------------------------
// rubric-v1.3: credential-failure UX (SOP 26)
// ---------------------------------------------------------------------------

/// The four rows of the verdict matrix, end-to-end against a real process that
/// really fails in each shape.
#[test]
fn credential_failure_modes_are_graded() {
    // (mock flag, expected verdict fragment, expected fix fragment)
    let cases: &[(&str, &str, &str)] = &[
        ("names-var", "credential UX: PASS", "no action needed"),
        (
            "no-var",
            "named no environment variable",
            "name the missing environment variable",
        ),
        (
            "exits-zero",
            "exited 0 after failing to start",
            "exit with a non-zero status",
        ),
    ];
    for (mode, verdict, fix) in cases {
        let out = run_check(
            &format!("--credential-failure {mode}"),
            &["--no-report", "--no-prewarm", "--timeout", "5"],
        );
        assert!(!out.status.success(), "{mode} must still fail the check");
        let err = stderr(&out);
        assert!(err.contains(verdict), "{mode}: missing verdict in:\n{err}");
        assert!(err.contains(fix), "{mode}: missing fix in:\n{err}");
        // Every penalizing verdict cites the SOP it comes from.
        if *mode != "names-var" {
            assert!(err.contains("SOP 26"), "{mode}: uncited:\n{err}");
        }
    }
}

/// The PASS case names the variable it read out of the child's stderr, so the
/// user is told exactly which key to set.
#[test]
fn a_passing_credential_failure_names_the_variable() {
    let out = run_check(
        "--credential-failure names-var",
        &["--no-report", "--no-prewarm", "--timeout", "5"],
    );
    let err = stderr(&out);
    assert!(err.contains("MOCK_API_KEY"), "variable not named:\n{err}");
}

/// A server that hangs on a missing credential is graded HIGH rather than
/// merely timing out — the census's 2 hanging servers were previously
/// indistinguishable from any other startup failure.
#[test]
fn a_hanging_server_is_graded_as_a_hang() {
    let out = run_check(
        "--credential-failure hangs",
        &["--no-report", "--no-prewarm", "--timeout", "3"],
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("credential UX: HUNG"), "{err}");
    assert!(err.contains("never block on a missing credential"), "{err}");
}

// ---------------------------------------------------------------------------
// rubric-v1.3: install-vs-boot timing (SOP 25)
// ---------------------------------------------------------------------------

/// A non-`npx` command has nothing to install, so install is reported as `n/a`
/// and boot carries the whole launch. Only boot is ever scored.
#[test]
fn a_non_npx_command_reports_install_as_not_applicable() {
    let out = run_check("", &["--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["timing"]["installSeconds"], serde_json::Value::Null);
    assert_eq!(v["timing"]["scored"], "serverBoot");
    assert!(
        v["timing"]["bootSeconds"].as_f64().expect("boot measured") >= 0.0,
        "boot must be measured for a server that started"
    );
    // `rubric-v1.4`: no `npx` shim to time, so no floor is measured and nothing
    // is subtracted — the scored figure is the raw launch.
    assert_eq!(v["timing"]["launcherSeconds"], serde_json::Value::Null);
    assert_eq!(v["timing"]["serverBootSeconds"], v["timing"]["bootSeconds"]);
    assert_eq!(v["timing"]["prewarmSkipped"], false);
}

/// `--no-prewarm` is recorded, so "we did not look" is never rendered as
/// "there was nothing to look at".
#[test]
fn no_prewarm_is_recorded_distinctly() {
    let out = run_check("", &["--json", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["timing"]["prewarmSkipped"], true);
    assert_eq!(v["timing"]["installSeconds"], serde_json::Value::Null);
}

/// The human report states the split on its own line, so the graded number
/// (boot) can never be confused with the cold-start figure that is not graded.
#[test]
fn the_human_report_states_the_install_boot_split() {
    let out = run_check("", &["--no-report", "--no-prewarm"]);
    let text = stdout(&out);
    assert!(text.contains("install skipped"), "{text}");
    assert!(text.contains("boot "), "{text}");
}

// ---------------------------------------------------------------------------
// rubric-v1.3: the protocol-compliance ceiling
// ---------------------------------------------------------------------------

/// A server that pollutes stdout breaks its own framing, and must not read "A"
/// however clean the rest of it is. End-to-end over the real pollution fixture.
#[test]
fn stdout_pollution_ceilings_the_composite() {
    let out = run_check("--pollute-stdout", &["--json", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let cap = &v["protocolCap"];
    assert!(
        !cap.is_null(),
        "the ceiling must bind on a polluting server"
    );
    assert_eq!(cap["cap"], 85.0);
    assert_eq!(cap["highPoints"], 15.0);
    // The report states the ceiling *and* its cause.
    let explanation = cap["explanation"].as_str().expect("explanation");
    assert!(explanation.contains("capped at 85"), "{explanation}");
    assert!(explanation.contains("non-protocol line"), "{explanation}");
    // And the composite really was lowered to it.
    assert!(v["composite"].as_f64().expect("composite") <= 85.0);
    assert_eq!(v["grade"], "B");
}

/// A clean server never sees the ceiling — the ramp is inert where the
/// overwhelming majority of servers sit.
#[test]
fn a_clean_server_has_no_protocol_ceiling() {
    let out = run_check("", &["--json", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["protocolCap"], serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// `--judge`: the opt-in description judge (jig-core::judge)
//
// Every test here drives the scripted mock provider — no live model call ever
// runs in CI. The scenarios cover the four things a real provider does to us:
// answer correctly, answer in prose, answer about only some tools, and fail.
// ---------------------------------------------------------------------------

/// Spawn the mock model provider on an OS-assigned port, returning a guard that
/// kills it on drop plus the port it announced on stderr.
fn spawn_provider() -> (ProviderGuard, u16) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let mut cmd = Command::new(mock_bin());
    cmd.arg("--provider").arg("0").stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mock provider");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut sent = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !sent {
                        if let Some(i) = line.find("127.0.0.1:") {
                            let digits: String = line[i + "127.0.0.1:".len()..]
                                .chars()
                                .take_while(char::is_ascii_digit)
                                .collect();
                            if let Ok(port) = digits.parse::<u16>() {
                                let _ = tx.send(port);
                                sent = true;
                            }
                        }
                    }
                }
            }
        }
    });
    let port = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("mock provider never announced its port within 10s");
    (ProviderGuard { child }, port)
}

struct ProviderGuard {
    child: std::process::Child,
}
impl Drop for ProviderGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `jig check --judge` against the mock MCP server, pointing the judge at
/// `scenario` on the mock provider. Keyless, so no key touches the test.
fn run_judged(port: u16, scenario: &str, extra: &[&str]) -> Output {
    let base = format!("http://127.0.0.1:{port}/{scenario}");
    let mut args: Vec<&str> = vec![
        "--no-report",
        "--no-prewarm",
        "--judge",
        "--base-url",
        &base,
        "--no-auth",
        "--api-model",
        "mock-judge",
    ];
    args.extend_from_slice(extra);
    run_check("", &args)
}

/// **The honesty guarantee, asserted end-to-end.** The deterministic document
/// must be identical with and without `--judge` against the same server: same
/// composite, same dimension scores, same grade, same findings. The only
/// difference permitted anywhere in the output is the added `judged` key.
#[test]
fn judge_does_not_move_a_single_byte_of_the_deterministic_report() {
    let (_guard, port) = spawn_provider();

    let plain = run_check("", &["--json", "--no-report", "--no-prewarm"]);
    let judged = run_judged(port, "judge_ok", &["--json"]);
    assert!(plain.status.success() && judged.status.success());

    let mut plain_doc: serde_json::Value =
        serde_json::from_str(&stdout(&plain)).expect("valid JSON");
    let mut judged_doc: serde_json::Value =
        serde_json::from_str(&stdout(&judged)).expect("valid JSON");

    // The judged run really did judge.
    let judged_key = judged_doc
        .as_object_mut()
        .expect("object")
        .remove("judged")
        .expect("a --judge run must carry a `judged` key");
    assert_eq!(judged_key["available"], true);
    assert_eq!(judged_key["scored"], false);

    // Wall-clock timings, and the latency woven into the robustness summary,
    // are the only legitimately machine-dependent bytes; blank them in both.
    for doc in [&mut plain_doc, &mut judged_doc] {
        let obj = doc.as_object_mut().expect("object");
        obj.insert("timing".to_string(), serde_json::Value::Null);
        if let Some(dims) = obj.get_mut("dimensions").and_then(|d| d.as_array_mut()) {
            for d in dims {
                if let Some(m) = d.as_object_mut() {
                    m.remove("summary");
                }
            }
        }
    }

    assert_eq!(
        serde_json::to_string(&plain_doc).unwrap(),
        serde_json::to_string(&judged_doc).unwrap(),
        "--judge must not alter one byte of the deterministic report"
    );

    // And the badge — which is the composite and nothing else — is identical.
    let badge_plain = run_check("", &["--badge", "--no-report", "--no-prewarm"]);
    let badge_judged = run_judged(port, "judge_ok", &["--badge"]);
    assert_eq!(stdout(&badge_plain), stdout(&badge_judged));
}

/// Without `--judge` there is no `judged` key at all — the flag is genuinely
/// opt-in, not merely defaulted off.
#[test]
fn without_the_flag_there_is_no_judged_key() {
    let out = run_check("", &["--json", "--no-report", "--no-prewarm"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(
        v.get("judged").is_none(),
        "a default check must not carry a `judged` key"
    );
    assert!(!stdout(&out).contains("Description judge"));
}

/// A well-formed judgement renders every verdict and pins its provenance: the
/// prompt version, the temperature, and the model **the provider reported**.
#[test]
fn a_well_formed_judgement_renders_and_records_its_provenance() {
    let (_guard, port) = spawn_provider();
    let out = run_judged(port, "judge_ok", &[]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("Description judge (opt-in · never scored)"),
        "{text}"
    );
    assert!(text.contains("prompt judge-prompt-v1"), "{text}");
    assert!(text.contains("temperature 0"), "{text}");
    // The model id as the provider reported it, not the one we asked for.
    assert!(text.contains("model mock-judge-1"), "{text}");
    assert!(text.contains("(keyless)"), "{text}");
    assert!(text.contains("outside rubric-v1.5"), "{text}");

    let json = stdout(&run_judged(port, "judge_ok", &["--json"]));
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let j = &v["judged"];
    assert_eq!(j["promptVersion"], "judge-prompt-v1");
    assert_eq!(j["reportedModel"], "mock-judge-1");
    assert_eq!(j["requestedModel"], "mock-judge");
    assert_eq!(j["temperature"], 0.0);
    assert_eq!(j["keyless"], true);
    // The exact prompt text is emitted, so the measurement is reproducible.
    assert!(j["systemPrompt"]
        .as_str()
        .expect("systemPrompt")
        .contains("states_purpose"));
    assert!(j["renderedRequest"]["messages"].is_array());
    assert_eq!(j["summary"]["toolsJudged"], 3);
    assert_eq!(j["tools"][0]["tool"], "echo");
    assert_eq!(j["tools"][0]["states_purpose"]["verdict"], "yes");
    assert_eq!(j["tools"][0]["distinguishes_siblings"]["verdict"], "no");
}

/// Prose instead of JSON is recorded as `unparseable` — never guessed at, never
/// a panic — and the check still exits 0.
#[test]
fn prose_instead_of_json_is_unparseable_and_check_still_succeeds() {
    let (_guard, port) = spawn_provider();
    let out = run_judged(port, "judge_prose", &[]);
    assert!(out.status.success(), "a confused model must not fail check");
    assert!(stdout(&out).contains("not the requested structure"));

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&run_judged(port, "judge_prose", &["--json"])))
            .expect("valid JSON");
    assert_eq!(v["judged"]["summary"]["toolsUnparseable"], 3);
    assert_eq!(v["judged"]["summary"]["toolsJudged"], 0);
    assert_eq!(v["judged"]["tools"][0]["verdict"], "unparseable");
    // The score is untouched by the model's confusion.
    assert!(v["composite"].as_f64().expect("composite") > 90.0);
}

/// A reply covering only some tools leaves the rest `not_judged`. A partial
/// answer is partial data, never backfilled and never an error.
#[test]
fn a_partial_reply_leaves_the_uncovered_tools_not_judged() {
    let (_guard, port) = spawn_provider();
    let out = run_judged(port, "judge_partial", &[]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("not judged"));

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&run_judged(port, "judge_partial", &["--json"])))
            .expect("valid JSON");
    assert_eq!(v["judged"]["summary"]["toolsJudged"], 1);
    assert_eq!(v["judged"]["summary"]["toolsNotJudged"], 2);
    let tools = v["judged"]["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["verdict"], "judged");
    assert_eq!(tools[1]["verdict"], "not_judged");
}

/// A provider 500 (after the shared bounded retry) leaves the judge
/// unavailable: one line saying so, the full deterministic report, exit 0.
#[test]
fn a_provider_500_leaves_the_judge_unavailable_and_the_check_intact() {
    let (_guard, port) = spawn_provider();
    let out = run_judged(port, "error_500", &[]);
    assert!(
        out.status.success(),
        "an unavailable judge must never fail a check"
    );
    let text = stdout(&out);
    assert!(text.contains("Description judge: unavailable"), "{text}");
    assert!(text.contains("500"), "{text}");
    assert!(text.contains("is unaffected"), "{text}");
    // The deterministic report is all still there.
    assert!(text.contains("grade A"));
    assert!(text.contains("Protocol compliance"));

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&run_judged(port, "error_500", &["--json"])))
            .expect("valid JSON");
    assert_eq!(v["judged"]["available"], false);
    assert_eq!(v["judged"]["scored"], false);
    assert!(v["judged"]["reason"]
        .as_str()
        .expect("reason")
        .contains("500"));
}

/// The `--min-score` gate is decided by the composite alone. A judged run, an
/// unavailable-judge run and a plain run gate identically.
#[test]
fn the_min_score_gate_ignores_the_judge_entirely() {
    let (_guard, port) = spawn_provider();
    // A floor above any achievable score fails in all three modes.
    let plain = run_check("", &["--min-score", "101", "--no-report", "--no-prewarm"]);
    let judged = run_judged(port, "judge_ok", &["--min-score", "101"]);
    let unavailable = run_judged(port, "error_500", &["--min-score", "101"]);
    assert_eq!(plain.status.code(), Some(1));
    assert_eq!(judged.status.code(), Some(1));
    assert_eq!(unavailable.status.code(), Some(1));
    // And a floor below it passes.
    assert!(run_judged(port, "judge_ok", &["--min-score", "1"])
        .status
        .success());
}

/// `--no-auth` without `--base-url` is rejected: disabling the key requirement
/// is a mode for an endpoint you host, not for a vendor API.
#[test]
fn no_auth_without_a_base_url_is_rejected() {
    let out = run_check("", &["--judge", "--no-auth", "--no-report", "--no-prewarm"]);
    assert!(!out.status.success());
}
