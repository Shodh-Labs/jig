//! A **scripted MCP host** — the test double for `jig serve`'s sampling path.
//!
//! Every other fixture in this crate plays the *server* half of MCP. This one
//! plays the *client* half, because Path 3 of the keyless work inverts the
//! usual direction: `jig serve` is the server, and it asks its host for model
//! completions via `sampling/createMessage`. Testing that needs a host that
//! advertises `capabilities.sampling` and answers those requests — which is
//! what this is.
//!
//! # Usage
//!
//! ```text
//! jig-mock-server --sampling-client [--no-sampling] -- <server command...>
//! ```
//!
//! It spawns the given command as an MCP server over stdio, completes the
//! handshake (advertising `sampling` unless `--no-sampling` is passed), calls
//! one tool, answers every inbound `sampling/createMessage` from a script, and
//! prints the tool's result as one line of JSON on stdout.
//!
//! Configuration comes from the environment so the argument list stays a plain
//! command line:
//!
//! * `JIG_FIXTURE_TOOL` — the tool to call (default `bench_server`).
//! * `JIG_FIXTURE_ARGS` — the tool's arguments, as a JSON object.
//! * `JIG_FIXTURE_SCRIPT` — a JSON array of strings: the assistant text to
//!   return for each successive `sampling/createMessage`, cycled if the run
//!   count exceeds the script length.
//! * `JIG_FIXTURE_MODEL` — the model identity to report. The literal
//!   `none` omits the `model` field entirely, so the "host named no model"
//!   path can be exercised.
//! * `JIG_FIXTURE_REJECT` — when set, every sampling request is refused with
//!   the spec's example error instead of being answered, exercising the
//!   host-refusal path.
//!
//! # Why a subprocess and not a test helper
//!
//! `jig serve` reads *its own* stdin and writes *its own* stdout. The only
//! honest way to drive it is across a real pipe, so the fixture has to be a
//! separate process. Being a binary mode also means an engineer can reproduce a
//! failing sampling run by hand, without writing Rust.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// The protocol revision the fixture host proposes.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Run the scripted host against `command`.
///
/// Exits non-zero (after a stderr diagnostic) on any harness failure, so a test
/// asserting on the printed JSON never mistakes a broken fixture for a broken
/// implementation.
pub fn run(command: &[String], advertise_sampling: bool) {
    let Some((program, args)) = command.split_first() else {
        fail("--sampling-client needs a server command after `--`");
    };

    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => fail(&format!("could not spawn {program}: {e}")),
    };
    let reader = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut host = Host {
        child,
        reader,
        script: script_from_env(),
        model: std::env::var("JIG_FIXTURE_MODEL").unwrap_or_else(|_| "mock-host-model-1".into()),
        reject: std::env::var("JIG_FIXTURE_REJECT").is_ok(),
        sampled: 0,
    };

    let capabilities = if advertise_sampling {
        // The exact declaration MCP 2025-06-18 requires of a sampling client.
        json!({ "sampling": {} })
    } else {
        json!({})
    };
    host.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": capabilities,
            "clientInfo": { "name": "jig-mock-sampling-host", "version": "1" },
        }
    }));
    let init = host.await_response(1);
    if init.get("error").is_some() {
        fail(&format!("initialize failed: {init}"));
    }
    host.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let tool = std::env::var("JIG_FIXTURE_TOOL").unwrap_or_else(|_| "bench_server".into());
    let arguments: Value = std::env::var("JIG_FIXTURE_ARGS")
        .ok()
        .map(|s| serde_json::from_str(&s).unwrap_or_else(|e| fail(&format!("bad ARGS: {e}"))))
        .unwrap_or_else(|| json!({}));
    host.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    }));

    // Pump until the tool call answers, servicing sampling requests on the way.
    let response = host.await_response(2);

    // The single line of stdout a test parses.
    println!(
        "{}",
        serde_json::to_string(&response).unwrap_or_else(|_| "{}".into())
    );

    host.shutdown();
}

/// The scripted host's live state.
struct Host {
    child: Child,
    reader: BufReader<ChildStdout>,
    /// Assistant texts to return, in order, cycled.
    script: Vec<String>,
    /// The model identity to report (`none` = omit the field).
    model: String,
    /// Refuse every sampling request instead of answering it.
    reject: bool,
    /// How many sampling requests have been answered so far.
    sampled: usize,
}

impl Host {
    fn send(&mut self, message: &Value) {
        let stdin = self.child.stdin.as_mut().expect("child stdin");
        if writeln!(stdin, "{message}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            fail("the server closed its stdin");
        }
    }

    /// Read frames until the response with `id` arrives, answering any
    /// server→client request encountered on the way.
    fn await_response(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => fail(&format!("the server closed stdout while awaiting id {id}")),
                Ok(_) => {}
                Err(e) => fail(&format!("read error: {e}")),
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                // A server that pollutes its stdout is exactly what Jig grades
                // others for; say so loudly rather than skipping the line.
                Err(e) => fail(&format!(
                    "the server wrote a non-JSON line ({e}): {trimmed}"
                )),
            };

            if let Some(method) = frame.get("method").and_then(Value::as_str) {
                self.handle_server_request(method, &frame);
                continue;
            }
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    /// Answer a request the server sent us.
    fn handle_server_request(&mut self, method: &str, frame: &Value) {
        let Some(request_id) = frame.get("id").cloned().filter(|v| !v.is_null()) else {
            // A notification; nothing to answer.
            return;
        };
        match method {
            "sampling/createMessage" => {
                let reply = self.sampling_reply(request_id);
                self.send(&reply);
            }
            "ping" => {
                self.send(&json!({ "jsonrpc": "2.0", "id": request_id, "result": {} }));
            }
            other => {
                self.send(&json!({
                    "jsonrpc": "2.0", "id": request_id,
                    "error": { "code": -32601, "message": format!("Method not found: {other}") }
                }));
            }
        }
    }

    /// Build the scripted answer to one `sampling/createMessage`.
    fn sampling_reply(&mut self, id: Value) -> Value {
        if self.reject {
            // The spec's own worked example of a client-side refusal.
            return json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -1, "message": "User rejected sampling request" }
            });
        }
        let text = if self.script.is_empty() {
            "{\"tool\": null, \"answer\": \"no script configured\"}".to_string()
        } else {
            self.script[self.sampled % self.script.len()].clone()
        };
        self.sampled += 1;

        // The 2025-06-18 result shape: a role, ONE content block, the model the
        // host actually used, and a stop reason.
        let mut result = json!({
            "role": "assistant",
            "content": { "type": "text", "text": text },
            "stopReason": "endTurn",
        });
        if self.model != "none" {
            result["model"] = json!(self.model);
        }
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    /// Close stdin (the MCP way to end a stdio session) and reap the child.
    fn shutdown(mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// Parse `JIG_FIXTURE_SCRIPT` into the list of assistant texts.
fn script_from_env() -> Vec<String> {
    let Ok(raw) = std::env::var("JIG_FIXTURE_SCRIPT") else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => other.to_string(),
            })
            .collect(),
        _ => fail("JIG_FIXTURE_SCRIPT must be a JSON array of strings"),
    }
}

/// Report a harness failure and exit non-zero. A test must never confuse a
/// broken fixture with a broken implementation.
fn fail(message: &str) -> ! {
    eprintln!("jig-mock-server --sampling-client: {message}");
    std::process::exit(1);
}
