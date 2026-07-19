//! The high-level MCP client: spawn a server, perform the handshake, and run
//! the core operations, all while every message is captured by the tap.

use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{JigError, Result};
use crate::http::HttpTransport;
use crate::protocol::{
    Implementation, InitializeResult, Prompt, PromptGetResult, Resource, ResourceReadResult, Tool,
    ToolCallResult, LATEST_PROTOCOL_VERSION,
};
use crate::tap::ProtocolTap;
use crate::transport::{
    StdioTransport, Transport, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_REQUEST_TIMEOUT,
};

/// Jig's identity, advertised to servers as `clientInfo`.
const CLIENT_NAME: &str = "jig";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard cap on `*/list` pages followed via `nextCursor`. Generous enough for
/// any real server, but bounded so a buggy server that always returns a fresh
/// cursor cannot make Jig loop forever.
const MAX_LIST_PAGES: usize = 1000;

/// Tunable options for a [`Client`] connection.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Per-request timeout applied to every JSON-RPC request (including the
    /// initialize handshake). `None` waits indefinitely. Defaults to
    /// [`DEFAULT_REQUEST_TIMEOUT`].
    pub request_timeout: Option<Duration>,
    /// Maximum size, in bytes, of a single inbound message. A message larger
    /// than this fails with [`JigError::MessageTooLarge`] instead of being
    /// buffered without limit. `None` disables the cap. Defaults to
    /// [`DEFAULT_MAX_MESSAGE_BYTES`].
    ///
    /// [`JigError::MessageTooLarge`]: crate::JigError::MessageTooLarge
    pub max_message_bytes: Option<usize>,
    /// Whether the HTTP transport may open the standalone server→client GET SSE
    /// stream (see [`Client::listen`]). Default `false`: a diagnostic tool opens
    /// that stream only when explicitly asked (`--listen`). Ignored by stdio,
    /// which has no such stream.
    pub listen: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            max_message_bytes: Some(DEFAULT_MAX_MESSAGE_BYTES),
            listen: false,
        }
    }
}

/// A connected, initialized MCP client, transport-agnostic.
///
/// Construct via [`Client::connect`] (stdio — spawns a subprocess) or
/// [`Client::connect_http`] (remote Streamable HTTP). Either way the
/// constructor completes the `initialize` / `notifications/initialized`
/// handshake before returning, and every subsequent operation is identical
/// regardless of the underlying [`Transport`].
pub struct Client {
    transport: Transport,
    init: InitializeResult,
}

impl Client {
    /// Spawn `program` (with `args`) as an MCP server over stdio, perform the
    /// full handshake, and return a ready client.
    pub async fn connect(program: &str, args: &[String]) -> Result<Self> {
        Self::connect_with_tap(program, args, ProtocolTap::new()).await
    }

    /// Like [`Client::connect`], but records into a caller-supplied tap. Use
    /// this when you want to own the tap (e.g. to write it out even if the
    /// handshake fails). Uses [`ClientOptions::default`].
    pub async fn connect_with_tap(
        program: &str,
        args: &[String],
        tap: ProtocolTap,
    ) -> Result<Self> {
        Self::connect_with_options(program, args, tap, ClientOptions::default()).await
    }

    /// Full-control constructor: caller-supplied tap and [`ClientOptions`]
    /// (notably the per-request timeout). Spawns the server and completes the
    /// handshake — which is itself bounded by the configured timeout, so a
    /// server that never answers `initialize` fails fast instead of hanging.
    pub async fn connect_with_options(
        program: &str,
        args: &[String],
        tap: ProtocolTap,
        options: ClientOptions,
    ) -> Result<Self> {
        let transport = Transport::Stdio(Box::new(StdioTransport::spawn_with_limits(
            program,
            args,
            tap,
            options.request_timeout,
            options.max_message_bytes,
        )?));
        let init = Self::handshake(&transport).await?;
        Ok(Client { transport, init })
    }

    /// Connect to a remote MCP server over the **Streamable HTTP** transport at
    /// `url` (the single MCP endpoint), performing the full handshake. Uses
    /// [`ClientOptions::default`] and no extra headers.
    pub async fn connect_http(url: &str) -> Result<Self> {
        Self::connect_http_with_options(
            url,
            Vec::new(),
            ProtocolTap::new(),
            ClientOptions::default(),
        )
        .await
    }

    /// Full-control HTTP constructor: the MCP endpoint `url`, `headers` attached
    /// to every request (e.g. `("Authorization", "Bearer …")` for remote SaaS
    /// servers), a caller-supplied `tap`, and [`ClientOptions`] (per-request
    /// timeout). Completes the handshake before returning.
    pub async fn connect_http_with_options(
        url: &str,
        headers: Vec<(String, String)>,
        tap: ProtocolTap,
        options: ClientOptions,
    ) -> Result<Self> {
        let transport = Transport::Http(HttpTransport::connect(
            url,
            headers,
            tap,
            options.request_timeout,
            options.max_message_bytes,
            options.listen,
        )?);
        let init = Self::handshake(&transport).await?;
        Ok(Client { transport, init })
    }

    /// Run the MCP lifecycle handshake: `initialize` request, then the
    /// `notifications/initialized` notification. Returns the negotiated
    /// [`InitializeResult`].
    async fn handshake(transport: &Transport) -> Result<InitializeResult> {
        let params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION,
            },
        });

        let result = transport.request("initialize", params).await?;
        let init: InitializeResult = serde_json::from_value(result)
            .map_err(|e| JigError::protocol(format!("invalid initialize result: {e}")))?;

        // Per spec the client confirms readiness before issuing operations.
        transport
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(init)
    }

    /// The protocol tap capturing this session's traffic.
    pub fn tap(&self) -> &ProtocolTap {
        self.transport.tap()
    }

    /// The server's advertised identity.
    pub fn server_info(&self) -> &Implementation {
        &self.init.server_info
    }

    /// The protocol version the server negotiated (may differ from the version
    /// Jig proposed).
    pub fn protocol_version(&self) -> &str {
        &self.init.protocol_version
    }

    /// The server's advertised capabilities, as raw JSON.
    pub fn capabilities(&self) -> &Value {
        &self.init.capabilities
    }

    /// Optional server-supplied instructions.
    pub fn instructions(&self) -> Option<&str> {
        self.init.instructions.as_deref()
    }

    /// The full negotiated initialize result.
    pub fn initialize_result(&self) -> &InitializeResult {
        &self.init
    }

    /// List the server's tools. Returns an empty vec (not an error) if the
    /// server does not advertise the `tools` capability or reports the method
    /// as unsupported. Follows `nextCursor` pagination to completion.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        self.list_paginated("tools", "tools/list", "tools").await
    }

    /// List the server's resources. Graceful when unsupported (see
    /// [`Client::list_tools`]); paginated.
    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        self.list_paginated("resources", "resources/list", "resources")
            .await
    }

    /// List the server's prompts. Graceful when unsupported (see
    /// [`Client::list_tools`]); paginated.
    pub async fn list_prompts(&self) -> Result<Vec<Prompt>> {
        self.list_paginated("prompts", "prompts/list", "prompts")
            .await
    }

    /// Shared driver for the `*/list` operations.
    ///
    /// * Skips the call entirely if `capability` was not advertised.
    /// * Degrades a `-32601 method not found` to an empty list, so a server
    ///   that advertises a capability but has not implemented the method does
    ///   not fail the whole inspection.
    /// * Follows MCP cursor pagination: it passes back any `nextCursor` the
    ///   server returns until the cursor is absent/empty, accumulating every
    ///   page. A diagnostic tool that showed only the first page would quietly
    ///   lie about what the server exposes. Two safety valves prevent a
    ///   misbehaving server from looping forever: a hard page cap and a guard
    ///   against a cursor that never advances.
    async fn list_paginated<T: serde::de::DeserializeOwned>(
        &self,
        capability: &str,
        method: &str,
        key: &str,
    ) -> Result<Vec<T>> {
        if !self.has_capability(capability) {
            return Ok(Vec::new());
        }

        let mut items: Vec<T> = Vec::new();
        let mut cursor: Option<String> = None;

        for _page in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = match self.transport.request(method, params).await {
                Ok(result) => result,
                // Unsupported method: keep whatever we already gathered.
                Err(e) if e.is_method_not_found() => return Ok(items),
                Err(e) => return Err(e),
            };

            items.extend(extract_list::<T>(&result, key)?);

            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => {
                    // A server that returns the same cursor forever would loop
                    // us indefinitely; stop if it fails to advance.
                    if cursor.as_deref() == Some(next) {
                        break;
                    }
                    cursor = Some(next.to_string());
                }
                // No cursor (absent, null, or empty) means the last page.
                _ => break,
            }
        }

        Ok(items)
    }

    /// Invoke a tool by name with the given arguments.
    ///
    /// A returned `ToolCallResult` with `is_error == true` is a *successful*
    /// protocol call in which the tool reported failure; it is `Ok`, not
    /// `Err`. `Err` is reserved for transport/protocol/server-level faults.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        let result = self.transport.request("tools/call", params).await?;
        serde_json::from_value(result)
            .map_err(|e| JigError::protocol(format!("invalid tools/call result: {e}")))
    }

    /// Read a resource's contents by URI (`resources/read`). Returns the content
    /// items the server provides — each either UTF-8 `text` or a base64 `blob`.
    ///
    /// Unlike the graceful `*/list` operations, this is an explicit invocation:
    /// a server error (e.g. `-32002 resource not found`) surfaces as `Err`, not
    /// an empty result, because the caller asked for a specific URI.
    pub async fn read_resource(&self, uri: &str) -> Result<ResourceReadResult> {
        let params = json!({ "uri": uri });
        let result = self.transport.request("resources/read", params).await?;
        serde_json::from_value(result)
            .map_err(|e| JigError::protocol(format!("invalid resources/read result: {e}")))
    }

    /// Fetch a prompt's rendered messages by name (`prompts/get`), passing an
    /// `arguments` map (use `json!({})` for none). A server error (e.g.
    /// `-32602 invalid params`) surfaces as `Err`.
    pub async fn get_prompt(&self, name: &str, arguments: Value) -> Result<PromptGetResult> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.transport.request("prompts/get", params).await?;
        serde_json::from_value(result)
            .map_err(|e| JigError::protocol(format!("invalid prompts/get result: {e}")))
    }

    /// Open the standalone server→client stream (HTTP GET SSE) and process
    /// pushed traffic — notifications and server-initiated requests — for
    /// `duration`, returning a [`ListenSummary`]. Every pushed message and every
    /// reply Jig sends is captured in the tap.
    ///
    /// Only meaningful on the Streamable HTTP transport and only when
    /// [`ClientOptions::listen`] was set; otherwise this returns a clear error
    /// rather than silently doing nothing.
    ///
    /// [`ListenSummary`]: crate::ListenSummary
    pub async fn listen(&self, duration: Duration) -> Result<crate::ListenSummary> {
        self.transport.listen(duration).await
    }

    /// Whether the server advertised a top-level capability key (e.g.
    /// `"tools"`, `"resources"`, `"prompts"`).
    pub fn has_capability(&self, key: &str) -> bool {
        self.init
            .capabilities
            .get(key)
            .map(|v| !v.is_null())
            .unwrap_or(false)
    }

    /// Cleanly terminate the server process.
    pub async fn shutdown(self) -> Result<()> {
        self.transport.shutdown().await
    }
}

/// Pull a typed list out of a `*/list` result envelope, e.g. the `tools` array
/// inside `{ "tools": [...] }`.
fn extract_list<T: serde::de::DeserializeOwned>(result: &Value, key: &str) -> Result<Vec<T>> {
    let array = match result.get(key) {
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(JigError::protocol(format!(
                "'{key}' field in list result was not an array"
            )))
        }
        // A result without the array is treated as an empty list rather than
        // an error, to tolerate servers that omit empty collections.
        None => return Ok(Vec::new()),
    };

    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let parsed = serde_json::from_value(item.clone())
            .map_err(|e| JigError::protocol(format!("invalid {key} entry: {e}")))?;
        out.push(parsed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Tool;

    #[test]
    fn extract_list_parses_tools() {
        let result = json!({
            "tools": [
                { "name": "a", "inputSchema": { "type": "object" } },
                { "name": "b", "inputSchema": { "type": "object" } }
            ]
        });
        let tools: Vec<Tool> = extract_list(&result, "tools").unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "a");
    }

    #[test]
    fn extract_list_missing_key_is_empty() {
        let result = json!({});
        let tools: Vec<Tool> = extract_list(&result, "tools").unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn extract_list_non_array_is_protocol_error() {
        let result = json!({ "tools": "nope" });
        let err = extract_list::<Tool>(&result, "tools").unwrap_err();
        assert!(matches!(err, JigError::Protocol(_)));
    }
}
