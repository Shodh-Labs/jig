//! The server side of the MCP **Streamable HTTP** transport (`2025-06-18`),
//! implemented with axum as a test fixture for Jig's `HttpTransport`.
//!
//! Single MCP endpoint at `/mcp` supporting POST (client messages), GET
//! (returns 405 — no standalone server stream offered), and DELETE (session
//! termination). It reuses the same JSON-RPC handlers as the stdio server, so
//! the two transports expose an identical MCP surface.
//!
//! Fixture behaviours, all driven by flags parsed in `main`:
//! * JSON response mode (default): each request answered with one
//!   `application/json` object.
//! * SSE response mode (`--sse`): each request answered with a
//!   `text/event-stream` body; the `tools/list` response is preceded by a
//!   pushed `notifications/message` so a client's notification capture can be
//!   asserted.
//! * Session issuance + enforcement: `initialize` issues an `Mcp-Session-Id`;
//!   every later request must echo it or receive HTTP 404.
//! * `--expire-after-initialize`: issue the session, then 404 every
//!   post-handshake request — the client's session-expiry path.

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

/// Shared server state.
struct AppState {
    /// Respond with SSE streams (`--sse`) rather than single JSON objects.
    sse: bool,
    /// Issue a session on initialize, then 404 every later request (`--expire`).
    expire: bool,
    /// The currently-issued session id (set on initialize, cleared on DELETE).
    session: Mutex<Option<String>>,
}

/// Run the HTTP server on `127.0.0.1:<port>` until the process is killed. Builds
/// its own Tokio runtime so `main` can stay synchronous for the stdio path.
pub fn serve(port: u16, sse: bool, expire: bool) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");
    rt.block_on(async move {
        let state = Arc::new(AppState {
            sse,
            expire,
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
             (sse={sse}, expire={expire})"
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

    // A notification (no id): per spec, 202 Accepted with an empty body.
    if id.is_none() {
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap();
    }
    let id = id.unwrap_or(Value::Null);

    // initialize: issue a fresh session and return the InitializeResult with the
    // Mcp-Session-Id header.
    if method == "initialize" {
        let session = new_session_id();
        *locked(&state.session) = Some(session.clone());
        let msg = crate::handle_initialize(id);
        return respond(&state, vec![msg], Some(&session));
    }

    // Every post-handshake request must carry the issued session id.
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = locked(&state.session).clone();
    let session_ok = known.is_some() && known == provided;
    if state.expire || !session_ok {
        // 404: expired (--expire), unknown, or missing session id.
        return text_status(StatusCode::NOT_FOUND, "session not found");
    }

    let messages = match method {
        "tools/list" => {
            let response = crate::handle_tools_list(id);
            if state.sse {
                // Push a server notification ahead of the response so the client
                // records-and-ignores it exactly as it would over stdio.
                vec![pushed_notification(), response]
            } else {
                vec![response]
            }
        }
        "tools/call" => vec![crate::handle_tools_call(id, req.get("params"))],
        other => vec![crate::error_response(
            id,
            -32601,
            &format!("Method not found: {other}"),
        )],
    };
    respond(&state, messages, None)
}

/// GET `/mcp`: we do not offer a standalone server->client SSE stream, so per
/// spec we return 405 Method Not Allowed.
async fn handle_get() -> Response {
    text_status(
        StatusCode::METHOD_NOT_ALLOWED,
        "this server does not offer a standalone SSE stream",
    )
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

    if state.sse {
        let mut body = String::new();
        for m in &messages {
            body.push_str("event: message\ndata: ");
            body.push_str(&serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string()));
            body.push_str("\n\n");
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

/// The server notification pushed ahead of a `tools/list` response in SSE mode.
fn pushed_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": "info", "data": "hello from the SSE stream" }
    })
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
