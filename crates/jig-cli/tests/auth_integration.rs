//! End-to-end integration tests for `jig auth`: run the real `jig` binary
//! against the real `jig-mock-server` in `--http --auth <scenario>` mode, over
//! actual TCP, and assert on the rendered conformance table, the findings, the
//! JSON output, and exit codes.
//!
//! The `jig` binary path comes from Cargo as `CARGO_BIN_EXE_jig` (this crate
//! defines it). The mock-server binary is its sibling in the same target
//! directory (built by `cargo test --workspace --all-targets`).

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::Duration;

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

/// Grab an ephemeral free port by binding to port 0 and releasing it.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Kills the child mock server when dropped, so no fixture process leaks.
struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the mock in `--http --auth <scenario>` on a free port, wait until it
/// accepts TCP, and return the guard plus the MCP URL.
fn spawn_auth(scenario: &str) -> (ServerGuard, String) {
    let port = free_port();
    let child = Command::new(mock_bin())
        .arg("--http")
        .arg(port.to_string())
        .arg("--auth")
        .arg(scenario)
        .spawn()
        .expect("spawn mock http server");
    let guard = ServerGuard { child };

    let mut ready = false;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "mock http server never started listening on {port}");

    (guard, format!("http://127.0.0.1:{port}/mcp"))
}

/// Run `jig auth` against `url` with the given trailing args.
fn run_auth(url: &str, args: &[&str]) -> Output {
    Command::new(jig_bin())
        .arg("auth")
        .arg("--http")
        .arg(url)
        .args(args)
        .output()
        .expect("spawn jig auth")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Redact the ephemeral port so a snapshot is stable across runs.
fn redact_port(s: &str) -> String {
    // Replace "127.0.0.1:<digits>" with "127.0.0.1:PORT".
    let mut out = String::with_capacity(s.len());
    let needle = "127.0.0.1:";
    let mut rest = s;
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx + needle.len()]);
        rest = &rest[idx + needle.len()..];
        let mut chars = rest.char_indices();
        let mut end = 0;
        for (i, c) in chars.by_ref() {
            if c.is_ascii_digit() {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        out.push_str("PORT");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// well-configured: a fully conformant surface.
// ---------------------------------------------------------------------------

#[test]
fn well_configured_is_conformant_and_snapshots() {
    let (_srv, url) = spawn_auth("well-configured");
    let out = run_auth(&url, &[]);
    assert!(out.status.success(), "conformant surface exits 0");
    let report = stdout(&out);
    assert!(
        report.contains("CONFORMANT"),
        "expected a conformant verdict: {report}"
    );
    // Every graded probe passed.
    assert!(
        report.contains("PASS") && !report.contains("FAIL"),
        "{report}"
    );
    assert!(report.contains("PKCE `S256`"));
    assert!(report.contains("audience binding"));
    insta::assert_snapshot!("auth_e2e_well_configured", redact_port(&report));
}

#[test]
fn well_configured_json_is_structured_and_redacted() {
    let (_srv, url) = spawn_auth("well-configured");
    let out = run_auth(&url, &["--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["verdict"], "conformant");
    assert_eq!(v["authRequired"], true);
    assert_eq!(v["mcpAuthSpecRevision"], "2025-06-18");
    assert!(v["summary"]["fail"].as_u64().unwrap() == 0);
    // The unauthenticated challenge exchange is captured.
    let exchanges = v["exchanges"].as_array().unwrap();
    assert!(exchanges.iter().any(|e| e["status"] == 401));
    // Every finding carries a spec citation.
    for f in v["findings"].as_array().unwrap() {
        assert!(f["citation"].as_str().unwrap().len() > 3);
    }
}

// ---------------------------------------------------------------------------
// no-challenge: a bare 401 with no WWW-Authenticate.
// ---------------------------------------------------------------------------

#[test]
fn no_challenge_flags_missing_www_authenticate() {
    let (_srv, url) = spawn_auth("no-challenge");
    let out = run_auth(&url, &[]);
    let report = stdout(&out);
    assert!(
        report.contains("no `WWW-Authenticate`"),
        "must flag the missing challenge header: {report}"
    );
    // The 401 itself is still a pass; the challenge grading fails.
    assert!(report.contains("HTTP 401"));
    insta::assert_snapshot!("auth_e2e_no_challenge", redact_port(&report));
}

// ---------------------------------------------------------------------------
// no-metadata: a challenge that points nowhere (404).
// ---------------------------------------------------------------------------

#[test]
fn no_metadata_flags_unreachable_resource_metadata() {
    let (_srv, url) = spawn_auth("no-metadata");
    let out = run_auth(&url, &[]);
    let report = stdout(&out);
    assert!(
        report.contains("resource_metadata"),
        "the challenge advertised a metadata URL: {report}"
    );
    assert!(
        report.contains("did not return usable RFC 9728 metadata"),
        "must flag the dangling metadata URL: {report}"
    );
}

// ---------------------------------------------------------------------------
// no-pkce: full metadata, but the AS omits S256.
// ---------------------------------------------------------------------------

#[test]
fn no_pkce_flags_missing_s256() {
    let (_srv, url) = spawn_auth("no-pkce");
    let out = run_auth(&url, &[]);
    let report = stdout(&out);
    assert!(
        report.contains("does not include the REQUIRED `S256`"),
        "must flag the missing PKCE method: {report}"
    );
    // The rest of the chain still resolves (partial conformance).
    assert!(report.contains("PARTIALLY CONFORMANT"), "{report}");
    insta::assert_snapshot!("auth_e2e_no_pkce", redact_port(&report));
}

// ---------------------------------------------------------------------------
// open: a 200 to the bare probe — informational "no auth".
// ---------------------------------------------------------------------------

#[test]
fn open_server_reports_no_auth() {
    let (_srv, url) = spawn_auth("open");
    let out = run_auth(&url, &[]);
    assert!(out.status.success(), "an open server is not a failure");
    let report = stdout(&out);
    assert!(
        report.contains("NO AUTH") && report.contains("requires no authentication"),
        "{report}"
    );
}

// ---------------------------------------------------------------------------
// Header passthrough: a supplied token succeeds where the bare probe got 401.
// ---------------------------------------------------------------------------

#[test]
fn header_passthrough_succeeds_with_a_token() {
    let (_srv, url) = spawn_auth("well-configured");
    let out = run_auth(
        &url,
        &["--header", "Authorization: Bearer test-token-value"],
    );
    let report = stdout(&out);
    assert!(
        report.contains("Header passthrough"),
        "the passthrough probe must run when a token is supplied: {report}"
    );
    assert!(
        report.contains("returned HTTP 200 where the bare probe got 401"),
        "the supplied token should be accepted: {report}"
    );
    // The token must never appear in the output (redaction).
    assert!(
        !report.contains("test-token-value"),
        "the token must be redacted from all output: {report}"
    );
}

// ---------------------------------------------------------------------------
// `jig check` surfaces a compact auth section for HTTP targets.
// ---------------------------------------------------------------------------

#[test]
fn check_surfaces_a_compact_auth_section_for_http() {
    // An open server: the MCP handshake succeeds (so `check` produces a report
    // card), and the trailing informational auth section probes the surface and
    // reports "no auth".
    let (_srv, url) = spawn_auth("open");
    let out = Command::new(jig_bin())
        .arg("check")
        .arg("--http")
        .arg(&url)
        .output()
        .expect("spawn jig check --http");
    let report = stdout(&out);
    assert!(
        report.contains("jig check"),
        "the report card should render: {report}"
    );
    assert!(
        report.contains("Auth (informational — not scored into the grade)"),
        "check must surface a compact auth section for an HTTP target: {report}"
    );
    assert!(
        report.contains("NO AUTH"),
        "the open server's auth verdict should be surfaced: {report}"
    );
}

// ---------------------------------------------------------------------------
// stdio target: a clear "HTTP-only" error, not a probe.
// ---------------------------------------------------------------------------

#[test]
fn stdio_target_is_rejected_with_a_clear_message() {
    let out = Command::new(jig_bin())
        .arg("auth")
        .arg("--stdio")
        .arg("some-server")
        .output()
        .expect("spawn jig auth --stdio");
    assert!(!out.status.success(), "stdio auth must fail");
    let err = stderr(&out);
    assert!(
        err.contains("HTTP transport") || err.contains("--http"),
        "expected an HTTP-only message: {err}"
    );
}
