//! `jig serve` — run **Jig itself** as an MCP server over stdio.
//!
//! Every other Jig verb is something a person types. This one hands the same
//! capabilities to any MCP client or agent: point a host at `jig serve` and it
//! can grade, price, inspect and bench other MCP servers as tool calls, with no
//! shell and no parsing of terminal output.
//!
//! # What it implements
//!
//! The server side of MCP `2025-06-18` over the stdio transport: `initialize`
//! (with version negotiation and client-capability capture),
//! `notifications/initialized`, `ping`, `tools/list`, and `tools/call`. It
//! advertises exactly one capability, `tools`. Any other method gets a
//! JSON-RPC `-32601`, because a diagnostic tool that silently accepts a method
//! it does not implement has no business grading anyone else for the same sin.
//!
//! # Duplex, not half-duplex
//!
//! stdio is bidirectional and MCP uses both directions: a *server* may issue
//! requests back to the client, which is how [`sampling`] borrows the host's
//! model. So the read loop never blocks on a handler. Inbound frames are
//! demultiplexed — requests and notifications are dispatched to spawned tasks,
//! while responses are routed to whichever in-flight server→client request is
//! waiting on that id. Writes are serialized through a single mutex so two
//! concurrent handlers can never interleave half a line onto stdout.
//!
//! # stdout discipline
//!
//! stdout carries newline-delimited JSON-RPC and nothing else — the exact rule
//! `jig check` penalises other servers for breaking. Every diagnostic goes to
//! stderr. This module must not use `println!`.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

mod sampling;
mod tools;

pub(crate) use tools::TOOL_COUNT;

/// The MCP revision this server implements and advertises.
const PROTOCOL_VERSION: &str = jig_core::LATEST_PROTOCOL_VERSION;

/// Revisions this server will negotiate down to if a client proposes one.
///
/// Per the lifecycle spec the client proposes a version and the server answers
/// with one it supports. Jig's own surface is identical across these revisions
/// (`tools/list` + `tools/call`), so agreeing to an older one costs nothing and
/// keeps older hosts working.
const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC: the method does not exist.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: the parameters were unusable.
const INVALID_PARAMS: i64 = -32602;

/// How long a server→client request (i.e. `sampling/createMessage`) may wait
/// for its response before the run is recorded as a failure.
///
/// Generous on purpose: the sampling spec asks hosts to put a human in the loop
/// approving each request, and a person is slower than an API. Bounded on
/// purpose too — a host that never answers must not wedge the server forever.
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// The `instructions` string returned at `initialize`.
///
/// Kept short deliberately: it is counted against this server's own context
/// budget by `jig check`, and a server that lectures the model in its handshake
/// is exactly what Jig exists to flag.
const INSTRUCTIONS: &str = "Jig grades MCP servers. Point these tools at another server — either \
    a stdio command line or an HTTP endpoint URL — to score it, price its token cost, or watch a \
    model choose among its tools. Nothing here needs an API key.";

/// Shared server state: the serialized writer, the capabilities the client
/// advertised at `initialize`, and the table of in-flight server→client
/// requests.
pub(crate) struct ServeState {
    /// The single writer. Held across a whole line + flush so concurrent
    /// handlers cannot interleave output.
    writer: Mutex<tokio::io::Stdout>,
    /// The `capabilities` object from the client's `initialize` params.
    /// Consulted by `bench_server` to decide whether sampling is available.
    client_capabilities: Mutex<Value>,
    /// In-flight server→client requests, keyed by the id we assigned.
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    /// Monotonic id source for server→client requests.
    next_id: AtomicI64,
}

impl ServeState {
    fn new() -> ServeState {
        ServeState {
            writer: Mutex::new(tokio::io::stdout()),
            client_capabilities: Mutex::new(Value::Null),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
        }
    }

    /// Write one newline-delimited JSON-RPC message to stdout and flush it.
    async fn write_message(&self, message: &Value) {
        let mut line = match serde_json::to_string(message) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("jig serve: could not serialize an outbound message: {e}");
                return;
            }
        };
        line.push('\n');
        let mut out = self.writer.lock().await;
        if let Err(e) = out.write_all(line.as_bytes()).await {
            eprintln!("jig serve: stdout write failed: {e}");
            return;
        }
        if let Err(e) = out.flush().await {
            eprintln!("jig serve: stdout flush failed: {e}");
        }
    }

    /// Whether the client advertised the `sampling` capability.
    ///
    /// Per MCP `2025-06-18` *Client Features → Sampling*, a client that
    /// supports sampling **MUST** declare `capabilities.sampling` during
    /// initialization. Its presence is the whole contract; the object's
    /// contents are not specified, so only presence is tested.
    pub(crate) async fn client_supports_sampling(&self) -> bool {
        self.client_capabilities
            .lock()
            .await
            .get("sampling")
            .is_some()
    }

    /// Issue a request to the client and await its response.
    ///
    /// Returns the `result` object, or a human-readable failure: a JSON-RPC
    /// error the client returned (including the spec's "user rejected sampling
    /// request" case), a timeout, or a dropped channel.
    pub(crate) async fn request_client(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;

        let outcome = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("the {method} request was abandoned")),
            Err(_) => Err(format!(
                "the client did not answer {method} within {}s",
                timeout.as_secs()
            )),
        };
        // On timeout the entry is still registered; drop it so a late response
        // is discarded rather than accumulating forever.
        self.pending.lock().await.remove(&id);
        outcome
    }

    /// Route an inbound response to whichever request is waiting on its id.
    async fn deliver_response(&self, id: i64, payload: Result<Value, String>) {
        match self.pending.lock().await.remove(&id) {
            Some(tx) => {
                let _ = tx.send(payload);
            }
            // A response to an id we never issued (or already gave up on).
            // Recorded, not fatal: we are the diagnostic tool here.
            None => eprintln!("jig serve: ignoring a response for unknown request id {id}"),
        }
    }
}

/// Run `jig serve`: read JSON-RPC from stdin, write JSON-RPC to stdout, until
/// stdin closes.
pub async fn run(timeout_secs: u64, max_message_bytes: u64) -> Result<ExitCode, String> {
    let state = Arc::new(ServeState::new());
    let defaults = tools::Defaults {
        timeout_secs,
        max_message_bytes,
    };

    eprintln!(
        "jig serve: MCP server ready on stdio (protocol {PROTOCOL_VERSION}, {TOOL_COUNT} tools)"
    );

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut handlers = Vec::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // Clean EOF: the client closed the stream, which is how an MCP
            // stdio session ends. Exit 0.
            Ok(None) => break,
            Err(e) => {
                eprintln!("jig serve: stdin read failed: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jig serve: ignoring a line that is not JSON: {e}");
                continue;
            }
        };

        // Demultiplex: a frame carrying a `method` is inbound work; one without
        // is a response to a request *we* issued.
        if message.get("method").is_some() {
            let state = Arc::clone(&state);
            handlers.push(tokio::spawn(async move {
                handle_incoming(&state, message, defaults).await;
            }));
            // Reap finished handlers so a long session does not grow the vec.
            handlers.retain(|h| !h.is_finished());
        } else if let Some(id) = message.get("id").and_then(Value::as_i64) {
            let payload = match (message.get("result"), message.get("error")) {
                (Some(result), _) => Ok(result.clone()),
                (None, Some(error)) => Err(describe_rpc_error(error)),
                (None, None) => {
                    Err("the client sent a response with neither result nor error".to_string())
                }
            };
            state.deliver_response(id, payload).await;
        } else {
            eprintln!("jig serve: ignoring a frame with neither method nor id");
        }
    }

    // Let any in-flight tool call finish writing its response before exiting.
    for h in handlers {
        let _ = h.await;
    }
    Ok(ExitCode::SUCCESS)
}

/// Render a JSON-RPC error object as a single readable sentence.
fn describe_rpc_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    match code {
        Some(c) => format!("{message} (JSON-RPC error {c})"),
        None => message.to_string(),
    }
}

/// Handle one inbound request or notification.
async fn handle_incoming(state: &Arc<ServeState>, message: Value, defaults: tools::Defaults) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // No id means a notification: never answer one, per JSON-RPC 2.0.
    let Some(id) = message.get("id").cloned().filter(|v| !v.is_null()) else {
        if method == "notifications/initialized" {
            eprintln!("jig serve: client initialized");
        }
        return;
    };

    let response = match method.as_str() {
        "initialize" => success(id, initialize_result(state, &params).await),
        "ping" => success(id, json!({})),
        "tools/list" => success(id, json!({ "tools": tools::catalog() })),
        "tools/call" => match tools::call(state, &params, defaults).await {
            Ok(result) => success(id, result),
            Err(tools::CallError::UnknownTool(name)) => error(
                id,
                INVALID_PARAMS,
                &format!("Unknown tool: {name}. Call tools/list for the available tools."),
            ),
            Err(tools::CallError::BadArguments(detail)) => error(id, INVALID_PARAMS, &detail),
        },
        other => error(id, METHOD_NOT_FOUND, &format!("Method not found: {other}")),
    };
    state.write_message(&response).await;
}

/// Build the `initialize` result, capturing the client's capabilities and
/// negotiating a protocol version.
async fn initialize_result(state: &Arc<ServeState>, params: &Value) -> Value {
    let capabilities = params.get("capabilities").cloned().unwrap_or(Value::Null);
    let supports_sampling = capabilities.get("sampling").is_some();
    *state.client_capabilities.lock().await = capabilities;

    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    // Agree to the client's version when we speak it; otherwise answer with
    // ours and let the client decide whether it can proceed.
    let negotiated = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };

    let client_name = params
        .get("clientInfo")
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("<unnamed client>");
    eprintln!(
        "jig serve: initialized by {client_name} (protocol {negotiated}, sampling {})",
        if supports_sampling {
            "available"
        } else {
            "not advertised"
        }
    );

    json!({
        "protocolVersion": negotiated,
        // Exactly one capability. `2025-06-18` allows completions/experimental/
        // logging/prompts/resources/tools; advertising anything we do not serve
        // would be a lie the rubric rightly punishes.
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "jig",
            "title": "Jig — the MCP server workbench",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_capabilities_are_a_subset_of_the_negotiated_revision() {
        // The rubric this server is graded by allows exactly these top-level
        // capability keys on 2025-06-18. Advertising anything else costs
        // points — and would be untrue besides.
        let caps = json!({ "tools": { "listChanged": false } });
        for key in caps.as_object().unwrap().keys() {
            assert!(
                jig_core::capability_offspec_note(key, PROTOCOL_VERSION).is_none(),
                "capability {key} is off-spec for {PROTOCOL_VERSION}"
            );
        }
    }

    #[test]
    fn the_advertised_version_is_one_we_negotiate() {
        assert!(SUPPORTED_VERSIONS.contains(&PROTOCOL_VERSION));
    }

    #[test]
    fn rpc_errors_render_with_their_code() {
        let e = json!({ "code": -1, "message": "User rejected sampling request" });
        assert_eq!(
            describe_rpc_error(&e),
            "User rejected sampling request (JSON-RPC error -1)"
        );
    }
}
