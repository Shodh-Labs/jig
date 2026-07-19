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

use std::path::PathBuf;
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
    let out = run_check("", &[]);
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
    let out = run_check("--pollute-stdout", &[]);
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
    let out = run_check("", &["--min-score", "99"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "score 98 is below --min-score 99, must exit 1"
    );
    // A passing floor exits 0.
    let ok = run_check("", &["--min-score", "80"]);
    assert!(ok.status.success(), "score 98 clears --min-score 80");
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
    assert_eq!(v["rubricVersion"], "rubric-v1");
    assert_eq!(v["contextCost"]["provenance"]["type"], "absolute_bands");
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
