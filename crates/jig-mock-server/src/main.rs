//! A minimal MCP server used as Jig's integration-test fixture and as a handy
//! scratch server. It speaks **both** MCP transports:
//!
//! * **stdio** (default): one JSON-RPC message per line on stdin/stdout.
//! * **Streamable HTTP** (`--http <port>`): the server side of the
//!   `2025-06-18` Streamable HTTP transport, via axum. See [`http`].
//!
//! It implements just enough of MCP `2025-06-18` to exercise a client:
//! `initialize`, `notifications/initialized`, `tools/list`, and `tools/call`.
//! It deliberately advertises *only* the `tools` capability so that a client's
//! graceful handling of unsupported `resources`/`prompts` can be observed.
//!
//! Flags:
//! * `--http <port>` — run the Streamable HTTP server on `127.0.0.1:<port>`
//!   instead of stdio. The MCP endpoint is `/mcp`.
//! * `--sse` — (HTTP mode) answer requests with a `text/event-stream` body,
//!   and push a server notification ahead of the `tools/list` response, rather
//!   than a single `application/json` object.
//! * `--expire-after-initialize` — (HTTP mode) issue a session on `initialize`
//!   but then return HTTP 404 for every post-handshake request, to exercise a
//!   client's session-expiry path.
//! * `--resources-prompts` — (both modes) additionally advertise and serve the
//!   `resources` and `prompts` features (one text resource, one blob resource,
//!   one prompt with an argument) so `resources/read` and `prompts/get` can be
//!   exercised at parity.
//! * `--push-notifications <n>` / `--server-ping` / `--server-sampling` — (HTTP
//!   mode) make the standalone GET stream real: push `n` notifications and/or a
//!   server→client `ping`/`sampling/createMessage` request.
//! * `--giant-json` / `--giant-sse` — (HTTP mode) answer `tools/list` with a
//!   multi-megabyte body to exercise the client's streaming size-cap.
//! * `--auth <scenario>` — (HTTP mode) play an OAuth conformance scenario for
//!   `jig auth`: `well-configured` (401 + proper challenge + full RFC 9728/8414
//!   metadata with S256+DCR), `no-challenge` (bare 401), `no-metadata`
//!   (challenge points at a 404), `no-pkce` (metadata lacks S256), or `open`
//!   (200, no auth). The `login-*` scenarios add a live authorization server
//!   (`/register`, `/authorize`, `/token`) so `jig auth --login` can run the
//!   whole authorization-code flow: `login-happy`, `login-bad-state`,
//!   `login-bad-iss`, `login-no-s256`, `login-token-error`. See
//!   [`http::AuthMode`].
//! * `--sampling-client [--no-sampling] -- <cmd...>` — run as a scripted MCP
//!   *host* instead of a server: spawn `<cmd>`, handshake (advertising the
//!   `sampling` capability unless `--no-sampling`), call one tool, and answer
//!   every `sampling/createMessage` from a script. See [`sampling_client`].
//! * `--pollute-stdout` / `--paginate` — (stdio mode) test fixtures, see below.
//! * `--chaos <mode[,mode...]>` — (stdio mode) the **hostile-server chaos
//!   catalog**: deliberately misbehave in one specific way so Jig's degradation
//!   can be asserted. Repeatable and/or comma-separated. See [`Chaos`].

use std::collections::HashSet;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

mod http;
mod provider;
mod sampling_client;

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Size of the `giant-message` payload (~20 MiB of text in one response).
const GIANT_BYTES: usize = 20 * 1024 * 1024;

/// The hostile-server chaos catalog. Each mode makes the stdio server misbehave
/// in exactly one way — never a panic, never an unbounded hang on the server's
/// part beyond what the mode names — so an integration test can assert Jig
/// produces a specific, actionable error/warning for it.
///
/// Modes (all trigger on the first post-handshake `tools/list`, except the two
/// startup/handshake modes):
///
/// * `binary-garbage` — raw non-UTF-8 bytes on stdout at startup, then serve
///   normally (a robust client survives and flags the pollution).
/// * `immediate-exit` — exit right after spawn, before the handshake.
/// * `mid-session-crash` — answer `initialize`, then exit before the next
///   request's response.
/// * `malformed-json` — a truncated/garbled JSON line for `tools/list`.
/// * `giant-message` — a single ~20 MiB `tools/list` response.
/// * `slow-drip` — write the `tools/list` response one byte at a time.
/// * `wrong-id` — answer `tools/list` with an id that was never requested.
/// * `duplicate-id` — answer `tools/list` twice with the same id.
/// * `no-newline` — a valid `tools/list` response but with no trailing newline,
///   then stay alive (nothing is ever framed, so the request times out).
#[derive(Default, Clone)]
struct Chaos {
    modes: HashSet<String>,
}

impl Chaos {
    /// Collect every `--chaos <value>` occurrence, splitting comma lists, into a
    /// set of mode names.
    fn parse(args: &[String]) -> Chaos {
        let mut modes = HashSet::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--chaos" {
                if let Some(v) = it.next() {
                    for m in v.split(',') {
                        let m = m.trim();
                        if !m.is_empty() {
                            modes.insert(m.to_string());
                        }
                    }
                }
            }
        }
        Chaos { modes }
    }

    fn has(&self, mode: &str) -> bool {
        self.modes.contains(mode)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Mock model-provider mode: `--provider <port>` (the `jig bench` test double).
    if let Some(port) = flag_port(&args, "--provider") {
        provider::serve(port);
        return;
    }

    // Scripted MCP *host* mode: `--sampling-client [--no-sampling] -- <cmd...>`.
    // Plays the client half of MCP so `jig serve`'s `sampling/createMessage`
    // path has something to talk to. See [`sampling_client`].
    if args.iter().any(|a| a == "--sampling-client") {
        let command: Vec<String> = match args.iter().position(|a| a == "--") {
            Some(i) => args[i + 1..].to_vec(),
            None => Vec::new(),
        };
        let advertise = !args.iter().any(|a| a == "--no-sampling");
        sampling_client::run(&command, advertise);
        return;
    }

    // HTTP mode: `--http <port>`.
    if let Some(port) = http_port(&args) {
        let cfg = http::HttpConfig {
            sse: args.iter().any(|a| a == "--sse"),
            expire: args.iter().any(|a| a == "--expire-after-initialize"),
            resources_prompts: args.iter().any(|a| a == "--resources-prompts"),
            push_notifications: flag_usize(&args, "--push-notifications").unwrap_or(0),
            server_ping: args.iter().any(|a| a == "--server-ping"),
            server_sampling: args.iter().any(|a| a == "--server-sampling"),
            giant_json: args.iter().any(|a| a == "--giant-json"),
            giant_sse: args.iter().any(|a| a == "--giant-sse"),
            auth: http::AuthMode::parse(&args),
        };
        http::serve(port, cfg);
        return;
    }

    let chaos = Chaos::parse(&args);

    // `immediate-exit`: die right after spawn, before the handshake even starts.
    // The client must report a clear "server exited during handshake" error that
    // includes the exit code and this stderr line.
    if chaos.has("immediate-exit") {
        eprintln!("jig-mock-server: chaos immediate-exit — exiting before handshake");
        std::process::exit(3);
    }

    run_stdio(chaos);
}

/// Parse `--http <port>` from the argument list, if present.
fn http_port(args: &[String]) -> Option<u16> {
    flag_port(args, "--http")
}

/// Parse `<flag> <port>` from the argument list, if present.
fn flag_port(args: &[String], flag: &str) -> Option<u16> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).and_then(|s| s.parse::<u16>().ok())
}

/// Parse `<flag> <n>` (a usize) from the argument list, if present.
fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).and_then(|s| s.parse::<usize>().ok())
}

/// The original stdio server loop: read one JSON-RPC message per line from
/// stdin, write one per line to stdout. Nothing but MCP messages is written to
/// stdout; diagnostics go to stderr.
fn run_stdio(chaos: Chaos) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Test fixture: with `--pollute-stdout`, emit a plain-text line to stdout
    // *before* any protocol traffic — exactly what a misconfigured logger or a
    // stray `console.log` does, corrupting the newline-delimited framing. A
    // robust client must still complete the handshake and flag the noise.
    if std::env::args().any(|a| a == "--pollute-stdout") {
        let _ = writeln!(
            out,
            "[startup] mock server listening (this line is NOT JSON-RPC)"
        );
        let _ = out.flush();
    }

    // Chaos `binary-garbage`: raw non-UTF-8 bytes on stdout before any protocol
    // traffic. A robust client must decode lossily, flag the pollution, and
    // still complete the handshake — it must not abort the whole stream.
    if chaos.has("binary-garbage") {
        // Invalid UTF-8 (lone continuation / reserved bytes), newline-terminated
        // so it lands as a single polluting "line".
        let _ = out.write_all(&[0xFF, 0xFE, 0x00, 0x80, 0xC0, b'\n']);
        let _ = out.flush();
    }

    // Test fixture: with `--paginate`, `tools/list` returns exactly one tool per
    // page and a `nextCursor` until the list is exhausted, so a client's cursor
    // following can be exercised. Off by default so the simple path stays simple.
    let paginate = std::env::args().any(|a| a == "--paginate");

    // Test fixture: with `--resources-prompts`, advertise and serve the
    // `resources` and `prompts` features so `resources/read` and `prompts/get`
    // can be exercised over stdio at parity with HTTP. Off by default so the
    // "only tools advertised" fixture behaviour is preserved.
    let resources_prompts = std::env::args().any(|a| a == "--resources-prompts");

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("jig-mock-server: stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jig-mock-server: ignoring non-JSON line: {e}");
                continue;
            }
        };

        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();

        // Notifications (no id) require no response.
        if id.is_none() {
            if method == "notifications/initialized" {
                eprintln!("jig-mock-server: client initialized");
            }
            continue;
        }
        let id = id.unwrap_or(Value::Null);

        // Test fixture: a request the server deliberately *accepts but never
        // answers*, so a client's request-timeout path can be exercised
        // deterministically. A real hung server looks exactly like this.
        if method == "test/hang" {
            eprintln!("jig-mock-server: received test/hang; intentionally not responding");
            continue;
        }

        // Chaos that targets the first post-handshake operation (`tools/list`).
        // `initialize` always succeeds so the handshake itself is never the
        // variable under test (except for the two handshake-phase modes).
        if method == "tools/list" && apply_tools_list_chaos(&mut out, &chaos, &id) {
            // The chaos path fully handled (or intentionally abandoned) this
            // request; move on without the normal response.
            continue;
        }

        let response = match method {
            "initialize" => handle_initialize(id, resources_prompts),
            "tools/list" if paginate => handle_tools_list_paginated(id, request.get("params")),
            "tools/list" => handle_tools_list(id),
            "tools/call" => handle_tools_call(id, request.get("params")),
            "resources/list" if resources_prompts => handle_resources_list(id),
            "resources/read" if resources_prompts => {
                handle_resources_read(id, request.get("params"))
            }
            "prompts/list" if resources_prompts => handle_prompts_list(id),
            "prompts/get" if resources_prompts => handle_prompts_get(id, request.get("params")),
            other => error_response(id, -32601, &format!("Method not found: {other}")),
        };

        if let Err(e) = write_message(&mut out, &response) {
            eprintln!("jig-mock-server: stdout write error: {e}");
            break;
        }
    }
}

/// Apply any `tools/list`-targeting chaos mode. Returns `true` if the request
/// was handled here (the caller should skip the normal response), `false` if no
/// chaos applied and the normal handler should run.
///
/// Never panics; the whole point is to misbehave *cleanly* so Jig's reaction is
/// the thing under test.
fn apply_tools_list_chaos(out: &mut impl Write, chaos: &Chaos, id: &Value) -> bool {
    // `mid-session-crash`: the handshake succeeded, but the server dies before
    // answering the first real request. Exit code + stderr must reach the client.
    if chaos.has("mid-session-crash") {
        eprintln!("jig-mock-server: chaos mid-session-crash — exiting before tools/list response");
        let _ = out.flush();
        std::process::exit(7);
    }

    // `malformed-json`: a truncated JSON-RPC line (valid framing — it ends in a
    // newline — but unparseable), so the client records stdout pollution and the
    // request times out with the method named.
    if chaos.has("malformed-json") {
        let garbled = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[");
        let _ = writeln!(out, "{garbled}");
        let _ = out.flush();
        return true;
    }

    // `wrong-id`: a well-formed response carrying an id that was never
    // requested. The client cannot correlate it (records it, routes nothing) and
    // the real request times out.
    if chaos.has("wrong-id") {
        let resp = handle_tools_list(json!(987654321_i64));
        let _ = write_message(out, &resp);
        return true;
    }

    // `duplicate-id`: answer the same request twice. The first wins and completes
    // the call; the second is surplus and lands in the tap.
    if chaos.has("duplicate-id") {
        let resp = handle_tools_list(id.clone());
        let _ = write_message(out, &resp);
        let _ = write_message(out, &resp);
        return true;
    }

    // `giant-message`: a single ~20 MiB response. Under the default 64 MiB cap
    // the client must handle it; under a lower `--max-message-bytes` it must fail
    // with a clear size error.
    if chaos.has("giant-message") {
        let resp = handle_tools_list_giant(id.clone());
        let _ = write_message(out, &resp);
        return true;
    }

    // `slow-drip`: the correct response, but written one byte at a time, each
    // flushed separately so the client's reader observes many partial reads. A
    // small delay is inserted periodically (not every byte: on Windows the
    // ~15 ms timer granularity would otherwise make an 800-byte response take
    // tens of seconds). The client must reassemble it and complete under a
    // generous timeout.
    if chaos.has("slow-drip") {
        let resp = handle_tools_list(id.clone());
        let mut text = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        for (i, b) in text.as_bytes().iter().enumerate() {
            let _ = out.write_all(&[*b]);
            let _ = out.flush();
            if i % 16 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        return true;
    }

    // `no-newline`: a valid JSON response but with no trailing newline, then stay
    // alive so no EOF frames it either. Nothing is ever delivered, so the request
    // times out and the tap shows no inbound response.
    if chaos.has("no-newline") {
        let resp = handle_tools_list(id.clone());
        let text = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
        let _ = out.write_all(text.as_bytes()); // deliberately no '\n'
        let _ = out.flush();
        eprintln!("jig-mock-server: chaos no-newline — response written without newline; holding");
        // Block until the client gives up and closes stdin (EOF ends the wait).
        let mut sink = String::new();
        while io::stdin().lock().read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
        std::process::exit(0);
    }

    false
}

/// Write one newline-delimited JSON-RPC message and flush.
fn write_message(out: &mut impl Write, message: &Value) -> io::Result<()> {
    let text = serde_json::to_string(message).unwrap_or_else(|_| "{}".to_string());
    out.write_all(text.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Build the `initialize` result. By default advertises only `tools` (so a
/// client's graceful handling of unsupported `resources`/`prompts` can be
/// observed); with `resources_prompts` it additionally advertises and serves
/// `resources` and `prompts`, for parity testing of `resources/read` and
/// `prompts/get`.
fn handle_initialize(id: Value, resources_prompts: bool) -> Value {
    let capabilities = if resources_prompts {
        json!({ "tools": {}, "resources": {}, "prompts": {} })
    } else {
        // Only tools are advertised — resources/prompts are intentionally absent.
        json!({ "tools": {} })
    };
    // Test hook: when `JIG_MOCK_INSTRUCTIONS` is present in the environment, echo
    // its value back as the `instructions` string. This lets an integration test
    // prove that environment variables (e.g. from a discovered `--server` config
    // entry) actually reach the spawned child process.
    let instructions = std::env::var("JIG_MOCK_INSTRUCTIONS")
        .unwrap_or_else(|_| "A toy MCP server for exercising Jig.".to_string());
    success_response(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": capabilities,
            "serverInfo": {
                "name": "jig-mock-server",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": instructions
        }),
    )
}

/// The single text resource this server serves under `--resources-prompts`.
const TEXT_RESOURCE_URI: &str = "mock://text/hello";
/// The single blob resource this server serves under `--resources-prompts`.
const BLOB_RESOURCE_URI: &str = "mock://blob/logo";
/// A tiny valid 1×1 PNG, base64-encoded — a realistic `blob` payload.
const BLOB_RESOURCE_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGP4DwABBAEAHnGpJQAAAABJRU5ErkJggg==";

/// `resources/list`: one text resource and one blob resource.
fn handle_resources_list(id: Value) -> Value {
    success_response(
        id,
        json!({
            "resources": [
                {
                    "uri": TEXT_RESOURCE_URI,
                    "name": "hello",
                    "description": "A plain-text greeting resource.",
                    "mimeType": "text/plain"
                },
                {
                    "uri": BLOB_RESOURCE_URI,
                    "name": "logo",
                    "description": "A tiny binary (PNG) resource.",
                    "mimeType": "image/png"
                }
            ]
        }),
    )
}

/// `resources/read`: return the contents of the requested URI (text or blob),
/// or a `-32002 resource not found` error for an unknown URI.
fn handle_resources_read(id: Value, params: Option<&Value>) -> Value {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match uri {
        TEXT_RESOURCE_URI => success_response(
            id,
            json!({
                "contents": [
                    {
                        "uri": TEXT_RESOURCE_URI,
                        "mimeType": "text/plain",
                        "text": "Hello from a jig mock text resource.\nSecond line."
                    }
                ]
            }),
        ),
        BLOB_RESOURCE_URI => success_response(
            id,
            json!({
                "contents": [
                    {
                        "uri": BLOB_RESOURCE_URI,
                        "mimeType": "image/png",
                        "blob": BLOB_RESOURCE_B64
                    }
                ]
            }),
        ),
        other => error_response(id, -32002, &format!("Resource not found: {other}")),
    }
}

/// `prompts/list`: one prompt (`greet`) taking a required `name` argument.
fn handle_prompts_list(id: Value) -> Value {
    success_response(
        id,
        json!({
            "prompts": [
                {
                    "name": "greet",
                    "description": "Greet someone by name.",
                    "arguments": [
                        { "name": "name", "description": "Who to greet.", "required": true }
                    ]
                }
            ]
        }),
    )
}

/// `prompts/get`: expand the `greet` prompt with the supplied `name` argument,
/// or a `-32602 invalid params` error for an unknown prompt name.
fn handle_prompts_get(id: Value, params: Option<&Value>) -> Value {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if name != "greet" {
        return error_response(id, -32602, &format!("Unknown prompt: {name}"));
    }
    let who = params
        .and_then(|p| p.get("arguments"))
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("friend");
    success_response(
        id,
        json!({
            "description": "A friendly greeting.",
            "messages": [
                {
                    "role": "user",
                    "content": { "type": "text", "text": format!("Please greet {who} warmly.") }
                }
            ]
        }),
    )
}

/// Three tools with deliberately varied schemas:
/// * `echo` — a single required string (simple).
/// * `make_reservation` — a nested object plus an enum (structured).
/// * `always_fails` — takes nothing and reports an error when called.
fn handle_tools_list(id: Value) -> Value {
    success_response(
        id,
        json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "Echo the provided text straight back.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to echo." }
                        },
                        "required": ["text"]
                    }
                },
                {
                    "name": "make_reservation",
                    "description": "Book a table. Demonstrates a nested object argument and an enum.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "party": {
                                "type": "object",
                                "properties": {
                                    "size": { "type": "integer", "minimum": 1 },
                                    "seating": {
                                        "type": "string",
                                        "enum": ["indoor", "outdoor", "bar"]
                                    }
                                },
                                "required": ["size"]
                            },
                            "date": { "type": "string", "description": "ISO-8601 date." }
                        },
                        "required": ["party", "date"]
                    }
                },
                {
                    "name": "always_fails",
                    "description": "A tool that always reports an error, for testing error paths.",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
    )
}

/// `giant-message` variant of `tools/list`: a single tool whose description is
/// ~20 MiB, producing one multi-megabyte response line. Valid JSON throughout —
/// the size is the only hostile property.
fn handle_tools_list_giant(id: Value) -> Value {
    let giant = "A".repeat(GIANT_BYTES);
    success_response(
        id,
        json!({
            "tools": [
                {
                    "name": "giant",
                    "description": giant,
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
    )
}

/// Paginated variant of `tools/list` (enabled by `--paginate`): one tool per
/// page, walking an opaque cursor. Exercises a client's `nextCursor` following.
fn handle_tools_list_paginated(id: Value, params: Option<&Value>) -> Value {
    // Reuse the canonical tool set, then hand it out one entry at a time.
    let all = handle_tools_list(Value::Null);
    let tools = all["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let cursor = params
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // The cursor is simply the next index, encoded as "page-<n>"; absent = 0.
    let index: usize = cursor
        .strip_prefix("page-")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    let mut result = json!({ "tools": tools.get(index).cloned().into_iter().collect::<Vec<_>>() });
    if index + 1 < tools.len() {
        result["nextCursor"] = json!(format!("page-{}", index + 1));
    }
    success_response(id, result)
}

fn handle_tools_call(id: Value, params: Option<&Value>) -> Value {
    let params = params.cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "echo" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            tool_text_result(id, &format!("echo: {text}"), false)
        }
        "make_reservation" => {
            let size = args
                .get("party")
                .and_then(|p| p.get("size"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let seating = args
                .get("party")
                .and_then(|p| p.get("seating"))
                .and_then(Value::as_str)
                .unwrap_or("indoor");
            let date = args.get("date").and_then(Value::as_str).unwrap_or("?");
            tool_text_result(
                id,
                &format!("Reserved a {seating} table for {size} on {date}."),
                false,
            )
        }
        "always_fails" => tool_text_result(id, "This tool always fails, by design.", true),
        other => error_response(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// Build a `tools/call` result carrying a single text content block.
fn tool_text_result(id: Value, text: &str, is_error: bool) -> Value {
    success_response(
        id,
        json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": is_error
        }),
    )
}
