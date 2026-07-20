//! End-to-end integration tests for `jig serve` — Jig as an MCP server.
//!
//! # The dogfood gate
//!
//! The headline test here runs `jig check` against `jig serve` and refuses a
//! composite below 90. Jig's whole proposition is that its rubric identifies
//! good MCP servers; if the server written by the people who wrote the rubric,
//! with the rubric in front of them, cannot earn an A, then either the server
//! is bad or the rubric is wrong — and both are our problem, not the user's.
//! So it is a hard gate, not a report.
//!
//! The remaining tests drive the protocol directly over stdio: version
//! negotiation, the unknown-method error code, the tool surface, and the
//! keyless failure path of `bench_server` on a host without sampling.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use serde_json::{json, Value};

/// The floor the dogfood test enforces: an A on Jig's own rubric.
const DOGFOOD_MIN_SCORE: u32 = 90;

/// The freshly built `jig` binary under test.
fn jig_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig"))
}

/// The `--stdio` value that launches `jig serve`. The path can contain spaces,
/// so it must be double-quoted for Jig's command splitter.
fn serve_stdio_arg() -> String {
    format!("\"{}\" serve", jig_bin().display())
}

/// Run `jig check` against `jig serve`.
fn check_serve(args: &[&str]) -> Output {
    Command::new(jig_bin())
        .arg("check")
        .arg("--stdio")
        .arg(serve_stdio_arg())
        .arg("--no-report")
        .args(args)
        .output()
        .expect("spawn jig check against jig serve")
}

// ---------------------------------------------------------------------------
// The dogfood gate
// ---------------------------------------------------------------------------

/// `jig serve` must score at least an A on Jig's own report card.
#[test]
fn jig_serve_earns_an_a_on_jigs_own_rubric() {
    let out = check_serve(&["--json"]);
    assert!(
        out.status.success(),
        "jig check failed against jig serve:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("check --json emits valid JSON");

    let composite = doc["composite"].as_u64().expect("a composite score") as u32;
    assert!(
        composite >= DOGFOOD_MIN_SCORE,
        "jig serve scored {composite}, below the {DOGFOOD_MIN_SCORE} floor. Either the server \
         regressed or the rubric did — fix whichever is wrong.\nReport:\n{}",
        serde_json::to_string_pretty(&doc).unwrap_or_default()
    );
    assert_eq!(doc["grade"], "A", "a score of {composite} must grade A");
    assert_eq!(doc["server"]["name"], "jig");

    // No single dimension may be quietly carried by the others: each scored
    // dimension has to stand on its own.
    for dim in doc["dimensions"].as_array().expect("dimensions") {
        if let Some(score) = dim["score"].as_u64() {
            assert!(
                score >= 60,
                "dimension {} scored {score}",
                dim["dimension"].as_str().unwrap_or("?")
            );
        }
    }
}

/// The `--min-score` CI gate is the mechanism a user would apply to us, so
/// prove it passes at the same floor rather than only asserting on the JSON.
#[test]
fn jig_serve_passes_the_min_score_gate() {
    let floor = DOGFOOD_MIN_SCORE.to_string();
    let out = check_serve(&["--min-score", &floor]);
    assert!(
        out.status.success(),
        "jig serve did not clear --min-score {floor}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Whatever else it does, `jig serve` must not write a byte of non-JSON-RPC to
/// stdout — the single most common way an MCP server breaks, and the thing
/// `jig check` docks the most points for.
#[test]
fn jig_serve_never_pollutes_stdout() {
    let out = check_serve(&["--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("non-protocol line"),
        "jig serve polluted its own stdout:\n{stderr}"
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let protocol = doc["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["dimension"] == "protocol")
        .expect("a protocol dimension");
    assert_eq!(protocol["score"], 100);
}

// ---------------------------------------------------------------------------
// A hand-driven MCP client
// ---------------------------------------------------------------------------

/// A `jig serve` child process driven by writing JSON-RPC lines to its stdin
/// and reading them back from its stdout.
struct Session {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Session {
    /// Start `jig serve` and complete the handshake, advertising `capabilities`.
    fn start(capabilities: Value) -> Session {
        let mut child = Command::new(jig_bin())
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn jig serve");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Session { child, reader };

        let init = session.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": { "name": "jig-serve-integration-test", "version": "1" },
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        session.notify("notifications/initialized", json!({}));
        session
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{message}").expect("write a request");
        stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Send a request and read frames until the response with that id arrives.
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("read a frame");
            assert!(n > 0, "jig serve closed stdout while awaiting id {id}");
            let frame: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("jig serve wrote a non-JSON line ({e}): {line:?}"));
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    /// Call a tool and return its `result` object.
    fn call_tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )["result"]
            .clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing stdin is the MCP way to end a stdio session; the server must
        // exit on EOF rather than needing a kill.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn tools_list_matches_the_documented_surface() {
    let mut s = Session::start(json!({}));
    let tools = s.request(2, "tools/list", json!({}))["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .clone();

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "check_server",
            "budget_server",
            "context_server",
            "inspect_server",
            "bench_server",
            "list_local_servers",
        ]
    );
    for tool in &tools {
        assert!(tool["title"].is_string(), "{tool} has no title");
        assert!(tool["description"].is_string(), "{tool} has no description");
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
}

#[test]
fn an_unknown_method_gets_the_spec_error_code() {
    let mut s = Session::start(json!({}));
    let resp = s.request(2, "definitely/not/a/method", json!({}));
    assert_eq!(resp["error"]["code"], -32601);
    assert!(resp.get("result").is_none());
}

#[test]
fn ping_is_answered_with_an_empty_result() {
    let mut s = Session::start(json!({}));
    let resp = s.request(2, "ping", json!({}));
    assert_eq!(resp["result"], json!({}));
}

#[test]
fn an_older_protocol_revision_is_negotiated_down() {
    let mut child = Command::new(jig_bin())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn jig serve");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            })
        )
        .expect("write initialize");
        stdin.flush().expect("flush");
    }
    let mut line = String::new();
    reader.read_line(&mut line).expect("read initialize result");
    let frame: Value = serde_json::from_str(line.trim()).expect("valid JSON");
    assert_eq!(frame["result"]["protocolVersion"], "2024-11-05");
    drop(child.stdin.take());
    let _ = child.wait();
}

#[test]
fn unknown_tool_names_are_rejected_with_invalid_params() {
    let mut s = Session::start(json!({}));
    let resp = s.request(
        2,
        "tools/call",
        json!({ "name": "no_such_tool", "arguments": {} }),
    );
    assert_eq!(resp["error"]["code"], -32602);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown tool"));
}

// ---------------------------------------------------------------------------
// Keyless behaviour
// ---------------------------------------------------------------------------

/// A host that does not advertise sampling must get a clear, actionable
/// failure — never a silent degradation into an empty or fabricated result.
#[test]
fn bench_server_without_sampling_fails_loudly_and_names_the_alternatives() {
    let mut s = Session::start(json!({}));
    let result = s.call_tool(
        2,
        "bench_server",
        json!({ "stdio": "does-not-matter", "task": "book a table" }),
    );

    assert_eq!(
        result["isError"], true,
        "a host without sampling must be an error, not an empty success"
    );
    let text = result["content"][0]["text"].as_str().expect("a message");
    assert!(text.contains("does not support MCP sampling"), "{text}");
    // All three escape routes, by name.
    assert!(text.contains("host that supports sampling"), "{text}");
    assert!(text.contains("--base-url"), "{text}");
    assert!(text.contains("--no-auth"), "{text}");
    assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
    // And the reassurance that the keyless tools still work.
    assert!(text.contains("check_server"), "{text}");
    // Nothing that looks like a result was invented.
    assert!(result.get("structuredContent").is_none(), "{result}");
}

/// `list_local_servers` must never emit an environment variable *value*. This
/// is the one tool that reads local configuration, so it is the one place a
/// token could escape through an MCP call.
#[test]
fn list_local_servers_redacts_environment_values() {
    let mut s = Session::start(json!({}));
    let result = s.call_tool(2, "list_local_servers", json!({}));
    assert_eq!(result["isError"], false);

    let structured = &result["structuredContent"];
    assert!(structured["servers"].is_array(), "{structured}");
    // Whatever this machine happens to have configured, every env value in the
    // document is the redaction marker rather than a real secret.
    for server in structured["servers"].as_array().unwrap() {
        if let Some(env) = server["env"].as_object() {
            for (key, value) in env {
                assert_eq!(
                    value.as_str(),
                    Some(jig_core::REDACTED),
                    "env value for {key} was not redacted"
                );
            }
        }
    }
}

/// The keyless tools must actually work with no credential in the environment
/// at all — that is the entire claim being made.
#[test]
fn the_keyless_tools_work_with_no_credentials_present() {
    let out = Command::new(jig_bin())
        .arg("check")
        .arg("--stdio")
        .arg(serve_stdio_arg())
        .arg("--no-report")
        .arg("--json")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("JIG_BENCH_API_KEY")
        .output()
        .expect("spawn jig check");
    assert!(
        out.status.success(),
        "check_server's own path needs no key:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
