//! JSON-RPC 2.0 over the MCP **Streamable HTTP** transport (spec `2025-06-18`).
//!
//! Unlike stdio — a single long-lived byte stream with a background reader that
//! routes responses by id — Streamable HTTP is request/response at the HTTP
//! layer: every client-originated JSON-RPC message is its own HTTP `POST` to the
//! single MCP endpoint. The server answers each POST either with a single
//! `application/json` object *or* with a `text/event-stream` (SSE) body carrying
//! one or more messages and ending with the response to the request. This module
//! handles both.
//!
//! Session, protocol-version, and content-negotiation mechanics implemented
//! against the spec, not from memory:
//!
//! * Every POST sends `Accept: application/json, text/event-stream`.
//! * The `Mcp-Session-Id` returned on the `initialize` response is captured and
//!   echoed on every subsequent request. An HTTP `404` carrying our session id
//!   means the session expired — we surface a clear, actionable error rather
//!   than silently re-initializing (jig is a diagnostic tool: it tells the
//!   truth).
//! * After `initialize`, the negotiated `MCP-Protocol-Version` header rides on
//!   every request.
//! * Server notifications that arrive on an SSE stream are recorded to the tap
//!   and ignored at the routing layer — the same policy as stdio.
//! * Notifications we send get `202 Accepted` with an empty body.
//! * On shutdown we send an HTTP `DELETE` with the session id, tolerating `405`
//!   (server does not support explicit termination) or any other failure.
//!
//! The whole request/response exchange happens inside one `async fn`; there is
//! no background task, because HTTP gives us framing and correlation for free.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::error::{JigError, Result};
use crate::tap::{Direction, ProtocolTap};

/// Header carrying the MCP session id, both directions.
const SESSION_HEADER: &str = "Mcp-Session-Id";
/// Header carrying the negotiated protocol version on post-initialize requests.
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";
/// The `Accept` value every POST must send per spec: the client must support
/// both a single JSON reply and an SSE stream.
const ACCEPT_VALUE: &str = "application/json, text/event-stream";
/// The `Accept` value for the standalone GET stream (spec: the client MUST list
/// `text/event-stream`).
const ACCEPT_SSE: &str = "text/event-stream";
/// Bytes of an error response body to include in a diagnostic message.
const BODY_SNIPPET_LEN: usize = 512;

/// Summary of a standalone GET-stream listening session (see
/// [`HttpTransport::listen`]). Every pushed message and every reply Jig sent is
/// also recorded in the [`ProtocolTap`]; this struct is the at-a-glance tally
/// the CLI reports.
#[derive(Debug, Clone, Default)]
pub struct ListenSummary {
    /// Whether the server opened an SSE stream (`true`) or declined with HTTP
    /// 405 (`false`, spec-permitted — the server offers no standalone stream).
    pub opened: bool,
    /// The HTTP status the server returned to the GET request.
    pub status: u16,
    /// Count of server→client notifications observed on the stream.
    pub notifications: usize,
    /// Count of server→client `ping` requests, each answered with an empty
    /// result per spec.
    pub pings: usize,
    /// Count of other server→client requests (e.g. `sampling/createMessage`,
    /// `roots/list`), each answered with a JSON-RPC `-32601 method not found`
    /// because Jig advertises no client capabilities.
    pub other_requests: usize,
    /// How long the stream was kept open.
    pub duration: Duration,
}

/// A live JSON-RPC-over-Streamable-HTTP connection to a remote MCP server.
pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
    /// Extra headers supplied by the caller (e.g. `Authorization: Bearer ...`).
    extra_headers: Vec<(String, String)>,
    /// Session id issued by the server at initialize, echoed on later requests.
    session_id: Mutex<Option<String>>,
    /// Negotiated protocol version, sent as `MCP-Protocol-Version` after init.
    protocol_version: Mutex<Option<String>>,
    tap: ProtocolTap,
    next_id: AtomicI64,
    /// Per-request timeout. `None` waits indefinitely.
    request_timeout: Option<Duration>,
    /// Maximum size, in bytes, of a single inbound response body. `None`
    /// disables the cap. A larger body fails with [`JigError::MessageTooLarge`].
    max_message_bytes: Option<usize>,
    /// Whether [`HttpTransport::listen`] is permitted (the `--listen` opt-in).
    /// Default OFF: a diagnostic tool opens the standalone server stream only
    /// when explicitly asked.
    listen_enabled: bool,
}

impl HttpTransport {
    /// Build a transport pointed at `endpoint` (the MCP endpoint URL). No
    /// network I/O happens here; the connection is established lazily on the
    /// first request (the `initialize` handshake).
    ///
    /// `extra_headers` are attached to every request — this is how auth
    /// (`Authorization: Bearer ...`) reaches servers that require it.
    pub fn connect(
        endpoint: &str,
        extra_headers: Vec<(String, String)>,
        tap: ProtocolTap,
        request_timeout: Option<Duration>,
        max_message_bytes: Option<usize>,
        listen_enabled: bool,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| JigError::transport(format!("failed to build HTTP client: {e}")))?;
        Ok(HttpTransport {
            client,
            endpoint: endpoint.to_string(),
            extra_headers,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            tap,
            next_id: AtomicI64::new(1),
            request_timeout,
            max_message_bytes,
            listen_enabled,
        })
    }

    /// Access the shared protocol tap for this connection.
    pub fn tap(&self) -> &ProtocolTap {
        &self.tap
    }

    /// Send a JSON-RPC request and await its correlated response `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.tap.record(Direction::Outbound, message.clone());

        let resp = self.post(&message, method).await?;

        // Capture a session id the moment it appears (the initialize response
        // carries it). Do this before the status checks so a later 404 can be
        // correctly diagnosed as expiry of the session we already hold.
        self.capture_session_id(&resp);

        let resp = self.ensure_success(resp, method).await?;

        let content_type = header_value(resp.headers().get(CONTENT_TYPE));
        let messages = self.read_body(resp, &content_type, method).await?;

        // Record every inbound message (notifications and the response alike),
        // then pick out the response correlated to our id. Notifications are
        // recorded for the tap but ignored at the routing layer — identical to
        // the stdio policy.
        let mut response: Option<Value> = None;
        for msg in messages {
            self.tap.record(Direction::Inbound, msg.clone());
            if response.is_none() && is_response_to(&msg, id) {
                response = Some(msg);
            }
        }

        let response = response.ok_or_else(|| {
            JigError::protocol(format!(
                "server response for '{method}' contained no JSON-RPC response with id {id}"
            ))
        })?;

        // After the handshake, remember the negotiated protocol version so the
        // spec-mandated `MCP-Protocol-Version` header rides on every later
        // request. The initialize request itself must not carry it (we do not
        // yet know the negotiated value).
        if method == "initialize" {
            if let Some(pv) = response
                .get("result")
                .and_then(|r| r.get("protocolVersion"))
                .and_then(Value::as_str)
            {
                *lock(&self.protocol_version) = Some(pv.to_string());
            }
        }

        crate::transport::parse_response(response)
    }

    /// Send a JSON-RPC notification (no id). Per spec the server replies `202
    /// Accepted` with an empty body; any 2xx is treated as success.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.tap.record(Direction::Outbound, message.clone());

        let resp = self.post(&message, method).await?;
        self.capture_session_id(&resp);
        // Any 2xx (typically 202 Accepted, empty body) is success. Body, if any,
        // is drained and ignored: a notification has no response.
        let _ = self.ensure_success(resp, method).await?;
        Ok(())
    }

    /// Gracefully terminate the session: send HTTP `DELETE` with the session id
    /// per spec. Best-effort — a `405` (server does not allow client
    /// termination) or any transport failure is tolerated, since shutdown must
    /// not itself fail.
    pub async fn shutdown(self) -> Result<()> {
        let session = lock(&self.session_id).clone();
        if let Some(session) = session {
            let mut rb = self
                .client
                .delete(&self.endpoint)
                .header(SESSION_HEADER, &session);
            if let Some(pv) = lock(&self.protocol_version).clone() {
                rb = rb.header(PROTOCOL_HEADER, pv);
            }
            for (k, v) in &self.extra_headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            if let Some(dur) = self.request_timeout {
                rb = rb.timeout(dur);
            }
            // Ignore the outcome entirely: 200, 204, 405, connection error —
            // all acceptable at shutdown.
            let _ = rb.send().await;
        }
        Ok(())
    }

    /// POST a single JSON-RPC message to the endpoint with the standard MCP
    /// headers (Accept, session id, protocol version, caller extras) and the
    /// per-request timeout applied.
    async fn post(&self, message: &Value, method: &str) -> Result<reqwest::Response> {
        let body = serde_json::to_string(message)?;
        let mut rb = self
            .client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, ACCEPT_VALUE)
            .body(body);

        if let Some(session) = lock(&self.session_id).clone() {
            rb = rb.header(SESSION_HEADER, session);
        }
        if let Some(pv) = lock(&self.protocol_version).clone() {
            rb = rb.header(PROTOCOL_HEADER, pv);
        }
        for (k, v) in &self.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(dur) = self.request_timeout {
            rb = rb.timeout(dur);
        }

        rb.send().await.map_err(|e| self.map_send_error(e, method))
    }

    /// Store the session id from a response's `Mcp-Session-Id` header, if present.
    fn capture_session_id(&self, resp: &reqwest::Response) {
        if let Some(value) = resp.headers().get(SESSION_HEADER) {
            if let Ok(s) = value.to_str() {
                if !s.is_empty() {
                    *lock(&self.session_id) = Some(s.to_string());
                }
            }
        }
    }

    /// Validate the HTTP status, returning the response untouched on any 2xx and
    /// a distinct, actionable error otherwise. The body is consumed only on the
    /// error paths (for the diagnostic snippet), so ownership is threaded through.
    async fn ensure_success(
        &self,
        resp: reqwest::Response,
        method: &str,
    ) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        // A 404 while we hold a session id is the spec's session-expiry signal.
        // Do not silently re-initialize — jig is a diagnostic tool; tell the
        // truth so the operator understands the state transition.
        if status == StatusCode::NOT_FOUND {
            if let Some(session) = lock(&self.session_id).clone() {
                return Err(JigError::transport(format!(
                    "MCP session expired: the server returned HTTP 404 for '{method}' with \
                     session id '{session}'. Sessions are not silently re-established — \
                     reconnect to start a fresh session."
                )));
            }
        }
        let body = resp.text().await.unwrap_or_default();
        Err(non_2xx_error(status, &body, method))
    }

    /// Read a *successful* response body into the JSON-RPC messages it carries,
    /// dispatching on content type. The body is **streamed** (via
    /// [`reqwest::Response::chunk`]) rather than buffered whole, so the size cap
    /// aborts the moment it is exceeded instead of after a hostile server has
    /// already made Jig hold the entire payload: per-body for JSON, per-event
    /// for SSE.
    async fn read_body(
        &self,
        mut resp: reqwest::Response,
        content_type: &str,
        method: &str,
    ) -> Result<Vec<Value>> {
        if content_type.contains("text/event-stream") {
            let mut reader = SseByteReader::new(self.max_message_bytes);
            let mut out = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| self.map_send_error(e, method))?
            {
                reader.feed(&chunk, &mut out)?;
            }
            reader.finish(&mut out);
            // A body that declared itself an event-stream but carried no
            // parseable events is malformed framing (mirrors the batch parser).
            if out.is_empty() && reader.saw_nonspace {
                return Err(JigError::protocol(format!(
                    "invalid SSE framing in response to '{method}': body was declared \
                     text/event-stream but carried no parseable events"
                )));
            }
            Ok(out)
        } else {
            // Treat everything else (application/json, or a server that omits
            // the header) as a single JSON object, accumulated under the cap.
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| self.map_send_error(e, method))?
            {
                if let Some(limit) = self.max_message_bytes {
                    if buf.len() + chunk.len() > limit {
                        return Err(JigError::MessageTooLarge { limit });
                    }
                }
                buf.extend_from_slice(&chunk);
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                return Ok(Vec::new());
            }
            let text = String::from_utf8_lossy(&buf);
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                JigError::protocol(format!(
                    "response to '{method}' was not valid JSON ({e}): {}",
                    snippet(&text)
                ))
            })?;
            Ok(vec![value])
        }
    }

    /// Open the standalone **GET** SSE stream and process server-initiated
    /// traffic for `duration`, then return a [`ListenSummary`]. Everything —
    /// pushed notifications, server→client requests, and Jig's replies to
    /// them — is recorded in the tap.
    ///
    /// Spec (`2025-06-18`, "Listening for Messages from the Server"): the client
    /// MAY issue a GET with `Accept: text/event-stream`; the server MUST answer
    /// with `text/event-stream` or HTTP 405. A 405 is not an error — it just
    /// means the server offers no standalone stream — so it is reported in the
    /// summary, never surfaced as a failure.
    ///
    /// Reply policy for server→client **requests** (v1): Jig advertises no
    /// client capabilities, so it honestly answers every such request with
    /// JSON-RPC `-32601 method not found` — **except** `ping`, which the spec
    /// says any receiver answers with an empty result.
    pub async fn listen(&self, duration: Duration) -> Result<ListenSummary> {
        if !self.listen_enabled {
            return Err(JigError::transport(
                "GET-stream listening was not enabled (set ClientOptions.listen / pass --listen)",
            ));
        }
        let start = Instant::now();

        let mut rb = self.client.get(&self.endpoint).header(ACCEPT, ACCEPT_SSE);
        if let Some(session) = lock(&self.session_id).clone() {
            rb = rb.header(SESSION_HEADER, session);
        }
        if let Some(pv) = lock(&self.protocol_version).clone() {
            rb = rb.header(PROTOCOL_HEADER, pv);
        }
        for (k, v) in &self.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        // No per-request timeout on the stream body: it is long-lived by design
        // and bounded instead by `duration` below.
        let resp = rb.send().await.map_err(|e| self.map_send_error(e, "GET"))?;
        let status = resp.status();

        // 405 Method Not Allowed: spec-permitted "no standalone stream offered".
        if status == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(ListenSummary {
                opened: false,
                status: status.as_u16(),
                duration: start.elapsed(),
                ..Default::default()
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(non_2xx_error(status, &body, "GET"));
        }

        let mut summary = ListenSummary {
            opened: true,
            status: status.as_u16(),
            ..Default::default()
        };
        let mut reader = SseByteReader::new(self.max_message_bytes);
        let mut resp = resp;
        let deadline = tokio::time::Instant::now() + duration;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            match tokio::time::timeout(remaining, resp.chunk()).await {
                // Duration elapsed: stop listening (the normal exit).
                Err(_elapsed) => break,
                // Server closed the stream (it MAY at any time).
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => {
                    let mut events = Vec::new();
                    reader.feed(&chunk, &mut events)?;
                    for msg in events {
                        self.handle_pushed(msg, &mut summary).await;
                    }
                }
                Ok(Err(e)) => return Err(self.map_send_error(e, "GET")),
            }
        }

        summary.duration = start.elapsed();
        Ok(summary)
    }

    /// Handle one message pushed on the GET stream: record it inbound, and — if
    /// it is a server→client request — send the policy reply (empty result for
    /// `ping`, else `-32601`) and record that outbound too.
    async fn handle_pushed(&self, msg: Value, summary: &mut ListenSummary) {
        self.tap.record(Direction::Inbound, msg.clone());

        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // A server→client request: it has both a method and a non-null id.
            (Some(m), Some(id)) if !id.is_null() => {
                let reply = if m == "ping" {
                    summary.pings += 1;
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                } else {
                    summary.other_requests += 1;
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!(
                                "method not found: '{m}' — jig advertises no client \
                                 capabilities and does not implement server-initiated requests"
                            )
                        }
                    })
                };
                self.tap.record(Direction::Outbound, reply.clone());
                // Best-effort: a failure to deliver the reply must not abort the
                // whole listen session; it is already recorded in the tap.
                let _ = self.post_reply(&reply).await;
            }
            // A notification (method, no id).
            (Some(_), None) => summary.notifications += 1,
            (Some(_), Some(id)) if id.is_null() => summary.notifications += 1,
            // A stray response or a non-message: recorded, not acted on.
            _ => {}
        }
    }

    /// POST a JSON-RPC *response* (Jig's reply to a server→client request) back
    /// to the MCP endpoint. Per spec the server answers `202 Accepted`; the body
    /// is drained and ignored.
    async fn post_reply(&self, message: &Value) -> Result<()> {
        let resp = self.post(message, "server-request-reply").await?;
        self.capture_session_id(&resp);
        Ok(())
    }

    /// Map a `reqwest` send error into jig's taxonomy with an actionable message.
    fn map_send_error(&self, e: reqwest::Error, method: &str) -> JigError {
        if e.is_timeout() {
            if let Some(dur) = self.request_timeout {
                return JigError::Timeout {
                    method: method.to_string(),
                    elapsed: dur,
                };
            }
        }
        if e.is_connect() {
            return JigError::transport(format!(
                "could not connect to {} for '{method}' — is the server running and the URL \
                 correct? ({e})",
                self.endpoint
            ));
        }
        JigError::transport(format!("HTTP request for '{method}' failed: {e}"))
    }
}

/// Lock helper that recovers from poisoning instead of panicking (a poisoned
/// tap/session mutex must never take down a diagnostic session).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Build the non-2xx error, including status and a bounded body snippet.
fn non_2xx_error(status: StatusCode, body: &str, method: &str) -> JigError {
    let snip = snippet(body);
    if snip.is_empty() {
        JigError::transport(format!(
            "server returned HTTP {status} for '{method}' (empty body)"
        ))
    } else {
        JigError::transport(format!(
            "server returned HTTP {status} for '{method}': {snip}"
        ))
    }
}

/// A bounded, single-line snippet of a response body for diagnostics.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    let mut s: String = trimmed.chars().take(BODY_SNIPPET_LEN).collect();
    if trimmed.chars().count() > BODY_SNIPPET_LEN {
        s.push('…');
    }
    s.replace('\n', " ")
}

/// Extract a header value as a lowercase-friendly string, or empty if absent.
fn header_value(h: Option<&reqwest::header::HeaderValue>) -> String {
    h.and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether `msg` is the JSON-RPC response correlated to request `id` (carries a
/// matching id *and* a `result` or `error`). Notifications and unrelated
/// messages return false.
fn is_response_to(msg: &Value, id: i64) -> bool {
    msg.get("id").and_then(Value::as_i64) == Some(id)
        && (msg.get("result").is_some() || msg.get("error").is_some())
}

/// Convert one dispatched SSE event's data payload into a JSON-RPC [`Value`].
///
/// A payload that is not valid JSON is preserved verbatim as a JSON string
/// (mirroring the stdio reader's truth-telling about malformed input) so it
/// still lands in the tap rather than being silently dropped.
fn sse_payload_to_value(payload: String) -> Value {
    match serde_json::from_str::<Value>(&payload) {
        Ok(v) => v,
        Err(_) => Value::String(payload),
    }
}

/// Incremental SSE **event framer** (WHATWG framing), fed one logical line at a
/// time. This is the single source of truth for SSE framing, shared by the
/// batch [`parse_sse`] entrypoint and by the streaming [`SseByteReader`] used
/// for capped response bodies and the standalone GET stream.
///
/// Framing rules: events are separated by blank lines; a `data:` field appends
/// to the event's data buffer (multiple `data:` lines join with `\n`); lines
/// beginning with `:` are comments; other fields (`event:`, `id:`, `retry:`)
/// are ignored. Each dispatched event's buffer is one JSON-RPC message.
#[derive(Default)]
struct SseFramer {
    data: String,
    have_data: bool,
}

impl SseFramer {
    /// Feed one logical line (its trailing `\n` and any `\r` already stripped).
    /// Returns `Some(payload)` when a blank line dispatches a pending event.
    fn push_line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            // Blank line dispatches the pending event, if any.
            if self.have_data {
                self.have_data = false;
                return Some(std::mem::take(&mut self.data));
            }
            return None;
        }
        if line.starts_with(':') {
            return None; // Comment line — ignore.
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };
        if field == "data" {
            if self.have_data {
                self.data.push('\n');
            }
            self.data.push_str(value);
            self.have_data = true;
        }
        // Other fields (event/id/retry) are not needed by jig.
        None
    }

    /// Flush a trailing event not terminated by a blank line (at EOF).
    fn finish(&mut self) -> Option<String> {
        if self.have_data {
            self.have_data = false;
            Some(std::mem::take(&mut self.data))
        } else {
            None
        }
    }

    /// Bytes currently buffered for the in-progress event (for the size cap).
    fn buffered_len(&self) -> usize {
        self.data.len()
    }
}

/// Parse an SSE body into the JSON-RPC messages carried by its `data:` fields.
///
/// The batch entrypoint (used for a fully-buffered body, and by the property /
/// fuzz harnesses). Framing is delegated to the shared `SseFramer`; a payload
/// that is not valid JSON is preserved verbatim. If the stream claims to be an
/// event-stream but yields no events from a non-empty body, that is flagged as
/// invalid framing.
///
/// Total over arbitrary input: any string yields either a `Vec` of messages or a
/// typed [`JigError::Protocol`] — never a panic.
pub fn parse_sse(text: &str, method: &str) -> Result<Vec<Value>> {
    let mut framer = SseFramer::default();
    let mut messages = Vec::new();
    let mut saw_event = false;

    for raw_line in text.split('\n') {
        // Tolerate CRLF line endings.
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some(payload) = framer.push_line(line) {
            saw_event = true;
            messages.push(sse_payload_to_value(payload));
        }
    }
    if let Some(payload) = framer.finish() {
        saw_event = true;
        messages.push(sse_payload_to_value(payload));
    }

    if !saw_event && !text.trim().is_empty() {
        return Err(JigError::protocol(format!(
            "invalid SSE framing in response to '{method}': body was declared \
             text/event-stream but carried no parseable events: {}",
            snippet(text)
        )));
    }

    Ok(messages)
}

/// Streaming SSE reader over a chunked HTTP body: bytes in, JSON-RPC messages
/// out, with an incremental per-event size cap.
///
/// Bytes arrive in arbitrary chunks (they do not respect line or event
/// boundaries), so this buffers a partial line, splits on `\n`, and feeds each
/// completed line to an [`SseFramer`]. The size cap is enforced against the
/// in-progress event (buffered data plus the current partial line), so a single
/// hostile giant event aborts with [`JigError::MessageTooLarge`] the moment it
/// crosses the cap — without buffering the whole body first.
struct SseByteReader {
    line: Vec<u8>,
    framer: SseFramer,
    max_bytes: Option<usize>,
    /// Whether any non-whitespace byte was seen (drives the framing-error check
    /// on a POST response body; irrelevant to the keep-alive-only GET stream).
    saw_nonspace: bool,
}

impl SseByteReader {
    fn new(max_bytes: Option<usize>) -> Self {
        SseByteReader {
            line: Vec::new(),
            framer: SseFramer::default(),
            max_bytes,
            saw_nonspace: false,
        }
    }

    /// Feed a chunk of body bytes, pushing any completed messages onto `out`.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Value>) -> Result<()> {
        for &b in bytes {
            if !b.is_ascii_whitespace() {
                self.saw_nonspace = true;
            }
            if b == b'\n' {
                self.dispatch_line(out);
            } else {
                self.line.push(b);
            }
            self.check_cap()?;
        }
        Ok(())
    }

    /// Take the buffered partial line, strip a trailing `\r`, decode lossily,
    /// and feed it to the framer.
    fn dispatch_line(&mut self, out: &mut Vec<Value>) {
        let mut raw = std::mem::take(&mut self.line);
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        let line = String::from_utf8_lossy(&raw);
        if let Some(payload) = self.framer.push_line(&line) {
            out.push(sse_payload_to_value(payload));
        }
    }

    /// Enforce the per-event byte cap against the in-progress event.
    fn check_cap(&self) -> Result<()> {
        if let Some(limit) = self.max_bytes {
            if self.framer.buffered_len() + self.line.len() > limit {
                return Err(JigError::MessageTooLarge { limit });
            }
        }
        Ok(())
    }

    /// Flush any trailing partial line and unterminated event at EOF.
    fn finish(&mut self, out: &mut Vec<Value>) {
        if !self.line.is_empty() {
            self.dispatch_line(out);
        }
        if let Some(payload) = self.framer.finish() {
            out.push(sse_payload_to_value(payload));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_event() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["result"]["ok"], true);
    }

    #[test]
    fn parses_notification_then_response_in_order() {
        let body =
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\
                    \n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["method"], "notifications/message");
        assert!(is_response_to(&msgs[1], 7));
        assert!(!is_response_to(&msgs[0], 7));
    }

    #[test]
    fn joins_multiline_data_fields() {
        let body = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":3,\"result\":{}}\n\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(is_response_to(&msgs[0], 3));
    }

    #[test]
    fn crlf_line_endings_are_tolerated() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn non_json_data_is_preserved_verbatim() {
        let body = "data: not json at all\n\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], Value::String("not json at all".to_string()));
    }

    #[test]
    fn empty_stream_is_ok_not_framing_error() {
        let msgs = parse_sse("", "x").unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn non_empty_body_without_events_is_framing_error() {
        // A body that is not SSE at all (no data: fields, not blank).
        let err = parse_sse("garbage line without a field colon meaning", "x").unwrap_err();
        assert!(matches!(err, JigError::Protocol(_)));
    }

    #[test]
    fn comments_are_ignored() {
        let body = ": this is a keep-alive comment\ndata: {\"id\":1,\"result\":{}}\n\n";
        let msgs = parse_sse(body, "x").unwrap();
        assert_eq!(msgs.len(), 1);
    }

    // ---- SseByteReader: the streaming, capped, incremental reader -----------

    /// Feed a body one byte at a time — the worst-case chunking — and prove the
    /// streaming reader reassembles exactly the same events as the batch parser.
    #[test]
    fn stream_reader_reassembles_events_across_arbitrary_chunks() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        let mut reader = SseByteReader::new(None);
        let mut out = Vec::new();
        for b in body.as_bytes() {
            reader.feed(&[*b], &mut out).unwrap();
        }
        reader.finish(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["method"], "notifications/message");
        assert!(is_response_to(&out[1], 7));
    }

    #[test]
    fn stream_reader_matches_batch_parser_on_one_chunk() {
        let body = "data: {\"a\":1}\n\ndata: not json\n\n";
        let batch = parse_sse(body, "x").unwrap();
        let mut reader = SseByteReader::new(None);
        let mut out = Vec::new();
        reader.feed(body.as_bytes(), &mut out).unwrap();
        reader.finish(&mut out);
        assert_eq!(out, batch);
        // The malformed payload is preserved verbatim, not dropped.
        assert_eq!(out[1], Value::String("not json".to_string()));
    }

    #[test]
    fn stream_reader_aborts_a_giant_event_at_the_cap() {
        // A single data line far larger than the cap must abort mid-stream with
        // MessageTooLarge — not buffer the whole thing first.
        let mut reader = SseByteReader::new(Some(16));
        let mut out = Vec::new();
        let big = format!("data: {}\n\n", "x".repeat(100));
        let err = reader.feed(big.as_bytes(), &mut out).unwrap_err();
        assert!(matches!(err, JigError::MessageTooLarge { limit: 16 }));
    }

    #[test]
    fn stream_reader_allows_events_up_to_the_cap() {
        // Small events under the cap flow through fine.
        let mut reader = SseByteReader::new(Some(64));
        let mut out = Vec::new();
        reader
            .feed(b"data: {\"id\":1,\"result\":{}}\n\n", &mut out)
            .unwrap();
        reader.finish(&mut out);
        assert_eq!(out.len(), 1);
    }
}
