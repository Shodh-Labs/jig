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
use std::time::Duration;

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
/// Bytes of an error response body to include in a diagnostic message.
const BODY_SNIPPET_LEN: usize = 512;

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
    /// dispatching on content type.
    async fn read_body(
        &self,
        resp: reqwest::Response,
        content_type: &str,
        method: &str,
    ) -> Result<Vec<Value>> {
        if content_type.contains("text/event-stream") {
            let text = resp
                .text()
                .await
                .map_err(|e| self.map_send_error(e, method))?;
            self.check_size(text.len())?;
            parse_sse(&text, method)
        } else {
            // Treat everything else (application/json, or a server that omits
            // the header) as a single JSON object.
            let text = resp
                .text()
                .await
                .map_err(|e| self.map_send_error(e, method))?;
            self.check_size(text.len())?;
            if text.trim().is_empty() {
                return Ok(Vec::new());
            }
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                JigError::protocol(format!(
                    "response to '{method}' was not valid JSON ({e}): {}",
                    snippet(&text)
                ))
            })?;
            Ok(vec![value])
        }
    }

    /// Enforce the inbound size cap against a response body length, mirroring
    /// the stdio transport's per-message cap.
    fn check_size(&self, len: usize) -> Result<()> {
        if let Some(limit) = self.max_message_bytes {
            if len > limit {
                return Err(JigError::MessageTooLarge { limit });
            }
        }
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

/// Parse an SSE body into the JSON-RPC messages carried by its `data:` fields.
///
/// SSE framing (WHATWG): events are separated by blank lines; a `data:` field
/// contributes a line to the event's data buffer (multiple `data:` lines are
/// joined with `\n`); lines beginning with `:` are comments; other fields
/// (`event:`, `id:`, `retry:`) are ignored for our purposes. Each dispatched
/// event's data buffer is one JSON-RPC message.
///
/// A data payload that is not valid JSON is preserved verbatim as a JSON string
/// (mirroring the stdio reader's truth-telling about malformed input) so it
/// still lands in the tap. If the stream claims to be an event-stream but yields
/// no events at all from a non-empty body, that is flagged as invalid framing.
///
/// Total over arbitrary input: any string yields either a `Vec` of messages or a
/// typed [`JigError::Protocol`] — never a panic — which the property and fuzz
/// harnesses rely on.
pub fn parse_sse(text: &str, method: &str) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut data = String::new();
    let mut have_data = false;
    let mut saw_event = false;

    let flush = |data: &mut String, have_data: &mut bool, messages: &mut Vec<Value>| {
        if *have_data {
            let payload = data.clone();
            // A payload that is not valid JSON is preserved verbatim (as stdio
            // does with malformed lines) so it still reaches the tap.
            let value = match serde_json::from_str::<Value>(&payload) {
                Ok(v) => v,
                Err(_) => Value::String(payload),
            };
            messages.push(value);
        }
        data.clear();
        *have_data = false;
    };

    for raw_line in text.split('\n') {
        // Tolerate CRLF line endings.
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if line.is_empty() {
            // Blank line dispatches the pending event.
            if have_data {
                saw_event = true;
                flush(&mut data, &mut have_data, &mut messages);
            }
            continue;
        }
        if line.starts_with(':') {
            // Comment line — ignore.
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };
        if field == "data" {
            if have_data {
                data.push('\n');
            }
            data.push_str(value);
            have_data = true;
        }
        // Other fields (event/id/retry) are not needed by jig.
    }
    // A trailing event not terminated by a blank line still counts.
    if have_data {
        saw_event = true;
        flush(&mut data, &mut have_data, &mut messages);
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
}
