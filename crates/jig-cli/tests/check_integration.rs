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

/// Redact machine-dependent latency so the human report is snapshot-stable.
fn redact_latency(s: &str) -> String {
    // Replace "<n>ms" with "<ms>".
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for a run of ASCII digits followed by "ms".
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if s[i..].starts_with("ms") {
                out.push_str("<ms>");
                i += 2; // skip "ms"
                continue;
            } else {
                out.push_str(&s[start..i]);
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
    insta::assert_snapshot!("check_e2e_clean", redact_latency(&report));
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
    insta::assert_snapshot!("check_e2e_pollution", redact_latency(&report));
}

// ---------------------------------------------------------------------------
// The CI gate: --min-score above the score exits nonzero.
// ---------------------------------------------------------------------------

#[test]
fn min_score_gate_fails_below_threshold() {
    let out = run_check("", &["--min-score", "99", "--no-report"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "score below --min-score 99 must exit 1"
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
    assert_eq!(v["rubricVersion"], "rubric-v1.1");
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
    assert_eq!(v["rubricVersion"], "rubric-v1.1");
    let _ = std::fs::remove_dir_all(&dir);
}
