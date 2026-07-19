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
//! * `--pollute-stdout` / `--paginate` — (stdio mode) test fixtures, see below.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

mod http;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // HTTP mode: `--http <port>`.
    if let Some(port) = http_port(&args) {
        let sse = args.iter().any(|a| a == "--sse");
        let expire = args.iter().any(|a| a == "--expire-after-initialize");
        http::serve(port, sse, expire);
        return;
    }

    run_stdio();
}

/// Parse `--http <port>` from the argument list, if present.
fn http_port(args: &[String]) -> Option<u16> {
    let idx = args.iter().position(|a| a == "--http")?;
    args.get(idx + 1).and_then(|s| s.parse::<u16>().ok())
}

/// The original stdio server loop: read one JSON-RPC message per line from
/// stdin, write one per line to stdout. Nothing but MCP messages is written to
/// stdout; diagnostics go to stderr.
fn run_stdio() {
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

    // Test fixture: with `--paginate`, `tools/list` returns exactly one tool per
    // page and a `nextCursor` until the list is exhausted, so a client's cursor
    // following can be exercised. Off by default so the simple path stays simple.
    let paginate = std::env::args().any(|a| a == "--paginate");

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

        let response = match method {
            "initialize" => handle_initialize(id),
            "tools/list" if paginate => handle_tools_list_paginated(id, request.get("params")),
            "tools/list" => handle_tools_list(id),
            "tools/call" => handle_tools_call(id, request.get("params")),
            other => error_response(id, -32601, &format!("Method not found: {other}")),
        };

        if let Err(e) = write_message(&mut out, &response) {
            eprintln!("jig-mock-server: stdout write error: {e}");
            break;
        }
    }
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

fn handle_initialize(id: Value) -> Value {
    success_response(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            // Only tools are advertised — resources/prompts are intentionally absent.
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "jig-mock-server",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "A toy MCP server for exercising Jig."
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
