//! The server side of the MCP **Streamable HTTP** transport (`2025-06-18`),
//! implemented with axum as a test fixture for Jig's `HttpTransport`.
//!
//! Single MCP endpoint at `/mcp` supporting POST (client messages), GET
//! (returns 405 by default — no standalone server stream — or, under the
//! push flags, a server→client SSE stream), and DELETE (session termination).
//! It reuses the same JSON-RPC handlers as the stdio server, so the two
//! transports expose an identical MCP surface.
//!
//! Fixture behaviours, all driven by [`HttpConfig`] parsed in `main`:
//! * JSON response mode (default): each request answered with one
//!   `application/json` object.
//! * SSE response mode (`--sse`): each request answered with a
//!   `text/event-stream` body; the `tools/list` response is preceded by a
//!   pushed `notifications/message` so a client's notification capture can be
//!   asserted.
//! * `--resources-prompts`: advertise and serve `resources` and `prompts`.
//! * Session issuance + enforcement: `initialize` issues an `Mcp-Session-Id`;
//!   every later request must echo it or receive HTTP 404.
//! * `--expire-after-initialize`: issue the session, then 404 every
//!   post-handshake request — the client's session-expiry path.
//! * `--push-notifications <n>` / `--server-ping` / `--server-sampling`: make
//!   the standalone GET stream real — push `n` notifications and/or a
//!   server→client `ping`/`sampling/createMessage` request, so a client's
//!   GET-stream handling (capture + reply policy) can be asserted.
//! * `--giant-json` / `--giant-sse`: answer `tools/list` with a multi-megabyte
//!   body (as one JSON object, or one giant SSE event) to exercise the client's
//!   streaming size-cap enforcement.
//!
//! Any JSON-RPC *response* the client POSTs back (its reply to a server→client
//! request) is answered `202 Accepted` and echoed to stderr as
//! `observed-reply: <json>`, so an integration test can confirm the reply
//! actually arrived at the server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

/// Header carrying the session id, both directions.
const SESSION_HEADER: &str = "Mcp-Session-Id";
/// The single MCP endpoint path.
const MCP_ENDPOINT: &str = "/mcp";

/// Flags controlling the HTTP fixture's behaviour, parsed once in `main`.
#[derive(Clone, Copy, Default)]
pub struct HttpConfig {
    /// Respond with SSE streams (`--sse`) rather than single JSON objects.
    pub sse: bool,
    /// Issue a session on initialize, then 404 every later request (`--expire`).
    pub expire: bool,
    /// Advertise and serve `resources` and `prompts` (`--resources-prompts`).
    pub resources_prompts: bool,
    /// Push this many notifications on the standalone GET stream.
    pub push_notifications: usize,
    /// Push a server→client `ping` request on the GET stream.
    pub server_ping: bool,
    /// Push a server→client `sampling/createMessage` request on the GET stream.
    pub server_sampling: bool,
    /// Answer `tools/list` with a multi-megabyte single JSON object.
    pub giant_json: bool,
    /// Answer `tools/list` with a single multi-megabyte SSE event.
    pub giant_sse: bool,
}

/// Shared server state.
struct AppState {
    cfg: HttpConfig,
    /// The currently-issued session id (set on initialize, cleared on DELETE).
    session: Mutex<Option<String>>,
}

/// Run the HTTP server on `127.0.0.1:<port>` until the process is killed. Builds
/// its own Tokio runtime so `main` can stay synchronous for the stdio path.
pub fn serve(port: u16, cfg: HttpConfig) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");
    rt.block_on(async move {
        let state = Arc::new(AppState {
            cfg,
            session: Mutex::new(None),
        });
        let app = Router::new()
            .route(
                MCP_ENDPOINT,
                post(handle_post).get(handle_get).delete(handle_delete),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("failed to bind HTTP port");
        eprintln!(
            "jig-mock-server: HTTP MCP endpoint on http://127.0.0.1:{port}{MCP_ENDPOINT} \
             (sse={}, expire={})",
            cfg.sse, cfg.expire
        );
        axum::serve(listener, app).await.expect("HTTP server error");
    });
}

/// Lock helper tolerant of poisoning (a test fixture must not cascade-panic).
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// POST `/mcp`: the client sending a JSON-RPC message.
async fn handle_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return text_status(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}")),
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // A JSON-RPC *response* the client POSTs back (its reply to a server→client
    // request): it carries `result`/`error` and no `method`. Per spec: answer
    // 202 Accepted with no body. Echo it to stderr so a test can confirm the
    // reply arrived.
    if method.is_empty() && (req.get("result").is_some() || req.get("error").is_some()) {
        eprintln!(
            "jig-mock-server: observed-reply: {}",
            serde_json::to_string(&req).unwrap_or_default()
        );
        return accepted();
    }

    // A notification (no id): per spec, 202 Accepted with an empty body.
    if id.is_none() {
        return accepted();
    }
    let id = id.unwrap_or(Value::Null);

    // initialize: issue a fresh session and return the InitializeResult with the
    // Mcp-Session-Id header.
    if method == "initialize" {
        let session = new_session_id();
        *locked(&state.session) = Some(session.clone());
        let msg = crate::handle_initialize(id, state.cfg.resources_prompts);
        return respond(&state, vec![msg], Some(&session));
    }

    // Every post-handshake request must carry the issued session id.
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = locked(&state.session).clone();
    let session_ok = known.is_some() && known == provided;
    if state.cfg.expire || !session_ok {
        // 404: expired (--expire), unknown, or missing session id.
        return text_status(StatusCode::NOT_FOUND, "session not found");
    }

    let messages = match method {
        "tools/list" => return tools_list_response(&state, id),
        "tools/call" => vec![crate::handle_tools_call(id, req.get("params"))],
        "resources/list" if state.cfg.resources_prompts => {
            vec![crate::handle_resources_list(id)]
        }
        "resources/read" if state.cfg.resources_prompts => {
            vec![crate::handle_resources_read(id, req.get("params"))]
        }
        "prompts/list" if state.cfg.resources_prompts => vec![crate::handle_prompts_list(id)],
        "prompts/get" if state.cfg.resources_prompts => {
            vec![crate::handle_prompts_get(id, req.get("params"))]
        }
        other => vec![crate::error_response(
            id,
            -32601,
            &format!("Method not found: {other}"),
        )],
    };
    respond(&state, messages, None)
}

/// Build the `tools/list` response, honouring the giant-body fixtures.
fn tools_list_response(state: &AppState, id: Value) -> Response {
    // A giant single JSON object (streaming size-cap fixture), regardless of
    // the SSE flag.
    if state.cfg.giant_json {
        let msg = crate::handle_tools_list_giant(id);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&msg).unwrap_or_else(|_| "null".to_string()),
            ))
            .unwrap();
    }
    // A single giant SSE event (streaming per-event size-cap fixture).
    if state.cfg.giant_sse {
        let msg = crate::handle_tools_list_giant(id);
        let body = sse_event(&msg);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }

    let response = crate::handle_tools_list(id);
    let messages = if state.cfg.sse {
        // Push a server notification ahead of the response so the client
        // records-and-ignores it exactly as it would over stdio.
        vec![pushed_notification(0), response]
    } else {
        vec![response]
    };
    respond(state, messages, None)
}

/// GET `/mcp`: the standalone server→client stream.
///
/// By default we offer no such stream, so per spec we return 405. Under the
/// push flags we open a real `text/event-stream`, push the configured
/// notifications and/or server→client requests, and then close it (the server
/// MAY close the stream at any time). The client captures every message and
/// replies to the requests.
async fn handle_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let cfg = state.cfg;
    let pushes_anything = cfg.push_notifications > 0 || cfg.server_ping || cfg.server_sampling;
    if !pushes_anything {
        return text_status(
            StatusCode::METHOD_NOT_ALLOWED,
            "this server does not offer a standalone SSE stream",
        );
    }

    // The stream requires a valid session, just like POST requests.
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = locked(&state.session).clone();
    if known.is_none() || known != provided {
        return text_status(StatusCode::NOT_FOUND, "session not found");
    }

    let mut body = String::new();
    for i in 0..cfg.push_notifications {
        body.push_str(&sse_event(&pushed_notification(i)));
    }
    if cfg.server_ping {
        body.push_str(&sse_event(&json!({
            "jsonrpc": "2.0",
            "id": server_request_id(),
            "method": "ping"
        })));
    }
    if cfg.server_sampling {
        body.push_str(&sse_event(&json!({
            "jsonrpc": "2.0",
            "id": server_request_id(),
            "method": "sampling/createMessage",
            "params": {
                "messages": [
                    { "role": "user", "content": { "type": "text", "text": "hello?" } }
                ],
                "maxTokens": 16
            }
        })));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

/// DELETE `/mcp`: explicit session termination. Clears the session if the id
/// matches; 404 otherwise.
async fn handle_delete(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut guard = locked(&state.session);
    if guard.is_some() && *guard == provided {
        *guard = None;
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    } else {
        text_status(StatusCode::NOT_FOUND, "session not found")
    }
}

/// Render `messages` as either a single JSON object or an SSE stream, attaching
/// the session id header when provided.
fn respond(state: &AppState, messages: Vec<Value>, session: Option<&str>) -> Response {
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(s) = session {
        builder = builder.header(SESSION_HEADER, s);
    }

    if state.cfg.sse {
        let mut body = String::new();
        for m in &messages {
            body.push_str(&sse_event(m));
        }
        builder
            .header("Content-Type", "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    } else {
        // A single-object JSON reply carries only the response (the last
        // message); pushed notifications require SSE and are omitted here.
        let last = messages.last().cloned().unwrap_or(Value::Null);
        builder
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&last).unwrap_or_else(|_| "null".to_string()),
            ))
            .unwrap()
    }
}

/// Serialize one JSON-RPC message as an SSE `event: message` frame.
fn sse_event(message: &Value) -> String {
    format!(
        "event: message\ndata: {}\n\n",
        serde_json::to_string(message).unwrap_or_else(|_| "{}".to_string())
    )
}

/// A 202 Accepted with an empty body (the spec reply to a client notification
/// or response).
fn accepted() -> Response {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(Body::empty())
        .unwrap()
}

/// A server notification, numbered so multiple pushes are distinguishable.
fn pushed_notification(n: usize) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": "info", "data": format!("push #{n} from the SSE stream") }
    })
}

/// A unique id for a server→client request pushed on the GET stream.
fn server_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("srv-req-{n}")
}

/// Generate a unique, visible-ASCII session id (spec: 0x21-0x7E).
fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("jig-sess-{nanos:x}-{n:x}")
}

/// A plain-text response with a given status.
fn text_status(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .unwrap()
}
