//! End-to-end integration tests for `jig auth --login`: the **real** `jig`
//! binary drives a **real** OAuth 2.1 authorization-code flow against the
//! `jig-mock-server`'s fixture authorization server, over actual TCP.
//!
//! There is no browser and no IdP. `--no-browser` makes `jig` print the
//! authorization URL instead of launching anything; the test then plays the role
//! of the user-agent, fetching that URL and following the `302` back to `jig`'s
//! loopback redirect. Everything after that — the PKCE proof, the code
//! redemption, the authenticated `initialize` + `tools/list` — is the product
//! code doing the real thing.
//!
//! The user-agent is a ~40-line HTTP/1.1 client built on `std::net::TcpStream`.
//! Every hop is plain HTTP on `127.0.0.1`, so a full client would be a
//! dependency bought for nothing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Binaries and fixtures
// ---------------------------------------------------------------------------

/// The freshly built `jig` binary under test.
fn jig_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig"))
}

/// The `jig-mock-server` binary: a sibling of `jig` in the target dir.
fn mock_bin() -> PathBuf {
    let mut p = jig_bin();
    p.set_file_name(if cfg!(windows) {
        "jig-mock-server.exe"
    } else {
        "jig-mock-server"
    });
    assert!(
        p.exists(),
        "mock-server binary not found at {} — run with `cargo test --workspace --all-targets`",
        p.display()
    );
    p
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

/// Extract the port from an announcement line carrying `127.0.0.1:<digits>`.
fn parse_announced_port(line: &str) -> Option<u16> {
    let rest = &line[line.find("127.0.0.1:")? + "127.0.0.1:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Spawn the mock in `--http 0 --auth <scenario>` and learn its OS-assigned port.
fn spawn_scenario(scenario: &str) -> (ServerGuard, String) {
    let mut cmd = Command::new(mock_bin());
    cmd.arg("--http")
        .arg("0")
        .arg("--auth")
        .arg(scenario)
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mock server");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
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
                        if let Some(p) = parse_announced_port(&line) {
                            let _ = tx.send(p);
                            sent = true;
                        }
                    }
                }
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("mock server never announced its port within 10s");
    (
        ServerGuard { child },
        format!("http://127.0.0.1:{port}/mcp"),
    )
}

// ---------------------------------------------------------------------------
// The test-side user-agent
// ---------------------------------------------------------------------------

/// One HTTP/1.1 response, distilled to what the redirect chain needs.
struct HttpResponse {
    status: u16,
    location: Option<String>,
}

/// `GET url` over plain HTTP on loopback. Panics with a readable message on any
/// transport failure — in a test, a failed hop is a failed test.
fn http_get(url: &str) -> HttpResponse {
    let rest = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("the test user-agent only speaks plain HTTP, got `{url}`"));
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let mut stream = TcpStream::connect(authority)
        .unwrap_or_else(|e| panic!("could not connect to {authority}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().expect("flush request");

    let mut raw = Vec::new();
    // A `Connection: close` response ends at EOF, so read to the end.
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);

    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in `{status_line}` (full response: {text})"));

    let mut location = None;
    for line in lines {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("location") {
                location = Some(value.trim().to_string());
            }
        }
    }
    HttpResponse { status, location }
}

/// Play the user-agent: fetch the authorization URL and follow its redirect back
/// to `jig`'s loopback listener, exactly as a browser would. Returns the status
/// of the final hop — `200` means `jig`'s callback page rendered.
fn complete_authorization(authorize_url: &str) -> u16 {
    let first = http_get(authorize_url);
    assert_eq!(
        first.status, 302,
        "the authorization endpoint should redirect to the loopback callback"
    );
    let location = first
        .location
        .expect("a 302 from /authorize must carry a Location header");
    http_get(&location).status
}

// ---------------------------------------------------------------------------
// Driving `jig auth --login`
// ---------------------------------------------------------------------------

/// The result of a complete login run.
struct LoginRun {
    stdout: String,
    stderr: String,
    /// Whether the process exited 0.
    success: bool,
    /// The authorization URL `jig` printed, if it got that far.
    authorize_url: Option<String>,
}

/// Run `jig auth --http <url> --login --no-browser <extra…>`, act as the
/// user-agent when the authorization URL appears, and collect the result.
///
/// `drive` controls whether the test completes the authorization at all — the
/// timeout scenario wants it left hanging.
fn run_login(url: &str, extra: &[&str], drive: bool) -> LoginRun {
    let mut child = Command::new(jig_bin())
        .arg("auth")
        .arg("--http")
        .arg(url)
        .arg("--login")
        .arg("--no-browser")
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jig auth --login");

    // stdout is the report and arrives only at the end; drain it on a thread so
    // the child can never block on a full pipe.
    let mut child_stdout = child.stdout.take().expect("piped stdout");
    let stdout_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = child_stdout.read_to_string(&mut s);
        s
    });

    // stderr carries the live progress, including the authorization URL. Watch
    // it line by line and hand the URL back as soon as it appears.
    let child_stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    let stderr_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(child_stderr);
        let mut all = String::new();
        let mut line = String::new();
        let mut sent = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !sent {
                        let trimmed = line.trim();
                        if trimmed.starts_with("http://") && trimmed.contains("/authorize?") {
                            let _ = tx.send(trimmed.to_string());
                            sent = true;
                        }
                    }
                    all.push_str(&line);
                }
            }
        }
        all
    });

    let authorize_url = rx.recv_timeout(Duration::from_secs(20)).ok();
    if drive {
        if let Some(u) = &authorize_url {
            assert_eq!(
                complete_authorization(u),
                200,
                "jig's loopback callback should render its 'you can close this tab' page"
            );
        }
    }

    let status = child.wait().expect("wait for jig");
    LoginRun {
        stdout: stdout_handle.join().expect("join stdout reader"),
        stderr: stderr_handle.join().expect("join stderr reader"),
        success: status.success(),
        authorize_url,
    }
}

/// Redact the ephemeral ports so a snapshot is stable across runs.
fn redact_port(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let needle = "127.0.0.1:";
    let mut rest = s;
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx + needle.len()]);
        rest = &rest[idx + needle.len()..];
        let mut end = 0;
        for (i, c) in rest.char_indices() {
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

/// Redact the fixture AS's serial-numbered client id, which increments across
/// runs within one process.
fn redact_ids(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("jig-dcr-client-") {
        out.push_str(&rest[..i]);
        out.push_str("jig-dcr-client-N");
        rest = &rest[i + "jig-dcr-client-".len()..];
        rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// login-happy: the whole flow, end to end.
// ---------------------------------------------------------------------------

#[test]
fn happy_path_reaches_an_authenticated_tools_list() {
    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(&url, &[], true);

    assert!(
        run.success,
        "a completed login must exit 0\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    let report = &run.stdout;
    assert!(report.contains("AUTHENTICATED"), "{report}");
    // The payoff line: a tool count only a real session can produce.
    assert!(
        report.contains("authenticated session established"),
        "{report}"
    );
    assert!(report.contains("tool(s) visible"), "{report}");
    assert!(report.contains("Authenticated probe"), "{report}");
    // Every step passed; no step failed.
    assert!(!report.contains("FAIL"), "{report}");

    // Each spec-mandated stage actually ran.
    for expected in [
        "Unauthenticated challenge",
        "Protected Resource Metadata",
        "Authorization Server Metadata",
        "PKCE S256",
        "Loopback redirect",
        "Client identity",
        "Authorization request",
        "Authorization response",
        "Token exchange",
        "Authenticated MCP session",
    ] {
        assert!(
            report.contains(expected),
            "missing step `{expected}`:\n{report}"
        );
    }
    // And the citations are real clause references, not decoration.
    for citation in ["RFC 7636 §4.1–§4.2", "RFC 8252 §7.3", "RFC 9207 §2.4"] {
        assert!(
            report.contains(citation),
            "missing citation `{citation}`:\n{report}"
        );
    }
}

#[test]
fn happy_path_registers_dynamically_and_sends_the_spec_parameters() {
    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(&url, &[], true);
    assert!(run.success, "{}", run.stdout);

    // RFC 7591: the AS advertises a registration endpoint, so jig self-registers
    // as a public native client rather than demanding a --client-id.
    assert!(
        run.stdout.contains("registered dynamically"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("token_endpoint_auth_method=none"),
        "{}",
        run.stdout
    );

    // The authorization URL carries every parameter the specs require.
    let authorize = run.authorize_url.expect("jig printed an authorization URL");
    for param in [
        "response_type=code",
        "code_challenge=",
        "code_challenge_method=S256",
        "state=",
        "redirect_uri=",
        "resource=",
    ] {
        assert!(
            authorize.contains(param),
            "authorization URL missing `{param}`: {authorize}"
        );
    }
    // Never a downgrade to `plain`.
    assert!(
        !authorize.contains("code_challenge_method=plain"),
        "{authorize}"
    );
}

#[test]
fn happy_path_human_report_snapshot() {
    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(&url, &[], true);
    assert!(run.success, "{}", run.stdout);
    insta::assert_snapshot!("auth_login_happy", redact_ids(&redact_port(&run.stdout)));
}

#[test]
fn happy_path_json_is_structured_and_carries_no_token_field() {
    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(&url, &["--json"], true);
    assert!(run.success, "{}", run.stdout);

    let v: serde_json::Value = serde_json::from_str(&run.stdout).expect("valid JSON on stdout");
    assert_eq!(v["mode"], "login");
    assert_eq!(v["result"], "authenticated");
    assert_eq!(v["clientRegistered"], true);
    assert_eq!(v["token"]["tokenType"], "Bearer");
    assert_eq!(v["token"]["expiresIn"], 3600);
    assert_eq!(v["token"]["refreshTokenIssued"], true);
    assert!(v["session"]["toolCount"].as_u64().unwrap() > 0);
    assert!(v["steps"].as_array().unwrap().len() >= 10);
    for step in v["steps"].as_array().unwrap() {
        assert_eq!(step["status"], "pass", "{step}");
        assert!(step["citation"].as_str().unwrap().len() > 3, "{step}");
    }
    // The `token` object describes the token without being able to hold one:
    // these four keys and nothing else.
    let token_keys: Vec<&str> = v["token"]
        .as_object()
        .expect("token object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        token_keys,
        vec!["expiresIn", "refreshTokenIssued", "scope", "tokenType"],
        "the token object must carry no credential-shaped field"
    );

    // The captured exchanges do mention the OAuth field *names* — that is what a
    // redacted record looks like — but every one of their values is redacted.
    // (That the token *value* is absent from every channel is proven separately
    // by `the_minted_token_never_appears_in_stdout_stderr_json_or_the_tap`.)
    let flat = v.to_string();
    assert!(!flat.contains("accessToken"), "{flat}");
    assert!(
        flat.contains("<redacted>"),
        "the captured exchanges should show redaction markers: {flat}"
    );
    let token_exchange = v["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["label"] == "POST token")
        .expect("the token exchange is captured");
    let response_body = token_exchange["body"].as_str().unwrap_or_default();
    assert!(
        response_body.contains(r#""access_token":"<redacted>""#),
        "the token response body must be recorded redacted: {response_body}"
    );
    assert!(
        response_body.contains(r#""refresh_token":"<redacted>""#),
        "{response_body}"
    );
}

// ---------------------------------------------------------------------------
// Secrets: the token value itself must not appear anywhere.
// ---------------------------------------------------------------------------

/// The load-bearing secrets test. `--token-out` is the *only* sanctioned way a
/// token reaches disk, so it is also the only way this test can learn what the
/// token actually was — and knowing it, the test can grep every other channel
/// for that exact string and assert its absence.
#[test]
fn the_minted_token_never_appears_in_stdout_stderr_json_or_the_tap() {
    let dir = std::env::temp_dir().join(format!(
        "jig-login-secrets-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let token_file = dir.join("token.json");
    let tap_file = dir.join("tap.jsonl");

    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(
        &url,
        &[
            "--json",
            "--token-out",
            token_file.to_str().unwrap(),
            "--tap",
            tap_file.to_str().unwrap(),
        ],
        true,
    );
    assert!(run.success, "{}\n{}", run.stdout, run.stderr);

    let written = std::fs::read_to_string(&token_file).expect("--token-out wrote the token file");
    let doc: serde_json::Value = serde_json::from_str(&written).expect("token file is JSON");
    let access_token = doc["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();
    let refresh_token = doc["refresh_token"]
        .as_str()
        .expect("refresh_token")
        .to_string();
    assert!(
        access_token.starts_with("jig-access-token-"),
        "the fixture AS issues recognisable tokens: {access_token}"
    );

    let tap = std::fs::read_to_string(&tap_file).expect("--tap wrote the protocol tap");

    // The token exists, and appears in exactly one place: the file the user
    // asked for.
    for (channel, body) in [
        ("stdout (--json)", run.stdout.as_str()),
        ("stderr", run.stderr.as_str()),
        ("the protocol tap", tap.as_str()),
    ] {
        assert!(
            !body.contains(&access_token),
            "the access token leaked into {channel}:\n{body}"
        );
        assert!(
            !body.contains(&refresh_token),
            "the refresh token leaked into {channel}:\n{body}"
        );
    }

    // The tap does record the exchanges — it is redacted, not empty.
    assert!(tap.contains("jig/http_request"), "the tap captured nothing");
    assert!(
        tap.contains("<redacted>"),
        "the tap should show redaction markers where secrets were: {tap}"
    );
    // Specifically: the token request's code and verifier are redacted, and the
    // token response's access_token is redacted.
    assert!(!tap.contains("code_verifier\":\"j"), "{tap}");

    // The stderr warning names the file that now holds a credential.
    assert!(
        run.stderr.contains("WARNING") && run.stderr.contains("token.json"),
        "--token-out must warn, naming the file: {}",
        run.stderr
    );

    // Owner-only where the platform has mode bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_file)
            .expect("stat token file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the token file must be 0600");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Failure scenarios: each produces its own specific error and a nonzero exit.
// ---------------------------------------------------------------------------

#[test]
fn bad_state_is_rejected_as_csrf() {
    let (_srv, url) = spawn_scenario("login-bad-state");
    let run = run_login(&url, &[], true);
    assert!(
        !run.success,
        "a state mismatch must exit nonzero: {}",
        run.stdout
    );
    assert!(run.stdout.contains("FAILED at step"), "{}", run.stdout);
    assert!(
        run.stdout
            .contains("`state` in the authorization response does not match"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("OAuth 2.1 §7.12"), "{}", run.stdout);
    // The flow stopped: no token, no session.
    assert!(
        !run.stdout.contains("Authenticated probe"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("Token exchange"), "{}", run.stdout);
}

#[test]
fn bad_iss_is_rejected_as_an_as_mixup() {
    let (_srv, url) = spawn_scenario("login-bad-iss");
    let run = run_login(&url, &[], true);
    assert!(
        !run.success,
        "an iss mismatch must exit nonzero: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("mixup.example.net"),
        "the offending issuer should be named: {}",
        run.stdout
    );
    assert!(run.stdout.contains("RFC 9207 §2.4"), "{}", run.stdout);
    assert!(
        run.stdout.contains("mix-up"),
        "the error should say what the mismatch means: {}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("Authenticated probe"),
        "{}",
        run.stdout
    );
}

#[test]
fn no_s256_refuses_to_start_the_flow() {
    let (_srv, url) = spawn_scenario("login-no-s256");
    // Nothing to drive: jig should refuse before it ever prints a URL.
    let run = run_login(&url, &[], false);
    assert!(
        !run.success,
        "a plain-only AS must exit nonzero: {}",
        run.stdout
    );
    assert!(
        run.authorize_url.is_none(),
        "jig must not open an authorization request it cannot secure: {:?}",
        run.authorize_url
    );
    assert!(
        run.stdout.contains("does not include `S256`"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("refuses to fall back to `plain`"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("RFC 7636 §4.2"), "{}", run.stdout);
}

#[test]
fn token_error_is_surfaced_verbatim() {
    let (_srv, url) = spawn_scenario("login-token-error");
    let run = run_login(&url, &[], true);
    assert!(
        !run.success,
        "a token-endpoint error must exit nonzero: {}",
        run.stdout
    );
    // The authorization half succeeded — the failure is specifically at step 9.
    assert!(run.stdout.contains("Token exchange"), "{}", run.stdout);
    // The AS's own words, not a paraphrase.
    assert!(
        run.stdout.contains("`error=invalid_grant`"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("this authorization code was issued to a different client"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("Authenticated probe"),
        "{}",
        run.stdout
    );
}

#[test]
fn failure_report_snapshot() {
    let (_srv, url) = spawn_scenario("login-bad-iss");
    let run = run_login(&url, &[], true);
    assert!(!run.success);
    insta::assert_snapshot!("auth_login_bad_iss", redact_ids(&redact_port(&run.stdout)));
}

// ---------------------------------------------------------------------------
// Client identity: the no-DCR path.
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_client_id_skips_dynamic_registration() {
    let (_srv, url) = spawn_scenario("login-happy");
    let run = run_login(&url, &["--client-id", "my-preregistered-client"], true);
    assert!(run.success, "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("using the supplied --client-id"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("registered dynamically"),
        "an explicit --client-id must suppress DCR: {}",
        run.stdout
    );
    // And it reached the end.
    assert!(run.stdout.contains("AUTHENTICATED"), "{}", run.stdout);
}

// ---------------------------------------------------------------------------
// Servers with no OAuth surface at all.
// ---------------------------------------------------------------------------

#[test]
fn an_open_server_says_there_is_nothing_to_log_into() {
    let (_srv, url) = spawn_scenario("open");
    let run = run_login(&url, &[], false);
    assert!(!run.success, "{}", run.stdout);
    assert!(
        run.stdout.contains("requires no authorization"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Drop `--login`"),
        "the error should say what to do instead: {}",
        run.stdout
    );
}

#[test]
fn a_server_without_metadata_cannot_be_logged_into() {
    let (_srv, url) = spawn_scenario("no-metadata");
    let run = run_login(&url, &[], false);
    assert!(!run.success, "{}", run.stdout);
    assert!(
        run.stdout.contains("Protected Resource Metadata"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("RFC 9728"), "{}", run.stdout);
}

// ---------------------------------------------------------------------------
// No regression: `jig auth` without `--login` is unchanged.
// ---------------------------------------------------------------------------

#[test]
fn without_login_the_command_still_only_probes() {
    let (_srv, url) = spawn_scenario("login-happy");
    let out = Command::new(jig_bin())
        .arg("auth")
        .arg("--http")
        .arg(&url)
        .output()
        .expect("spawn jig auth");
    let report = String::from_utf8_lossy(&out.stdout);
    // The conformance table, not a flow trace.
    assert!(report.contains("Unauthenticated challenge"), "{report}");
    assert!(!report.contains("Flow trace"), "{report}");
    assert!(!report.contains("Token exchange"), "{report}");
    assert!(
        report.contains("probed the discoverable auth surface only"),
        "the honest framing stays, and points at --login: {report}"
    );
}

#[test]
fn login_flags_require_login() {
    // `--client-id` without `--login` is a usage error, not a silent no-op.
    let out = Command::new(jig_bin())
        .arg("auth")
        .arg("--http")
        .arg("http://127.0.0.1:1/mcp")
        .arg("--client-id")
        .arg("x")
        .output()
        .expect("spawn jig auth");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--login"), "{err}");
}

#[test]
fn login_rejects_a_stdio_target() {
    let out = Command::new(jig_bin())
        .arg("auth")
        .arg("--stdio")
        .arg("some-server")
        .arg("--login")
        .output()
        .expect("spawn jig auth --stdio --login");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("HTTP transports") || err.contains("--http"),
        "{err}"
    );
}
