//! The high-level MCP client: spawn a server, perform the handshake, and run
//! the core operations, all while every message is captured by the tap.

use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{JigError, Result};
use crate::protocol::{
    Implementation, InitializeResult, Prompt, Resource, Tool, ToolCallResult,
    LATEST_PROTOCOL_VERSION,
};
use crate::tap::ProtocolTap;
use crate::transport::{StdioTransport, DEFAULT_REQUEST_TIMEOUT};

/// Jig's identity, advertised to servers as `clientInfo`.
const CLIENT_NAME: &str = "jig";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tunable options for a [`Client`] connection.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Per-request timeout applied to every JSON-RPC request (including the
    /// initialize handshake). `None` waits indefinitely. Defaults to
    /// [`DEFAULT_REQUEST_TIMEOUT`].
    pub request_timeout: Option<Duration>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
        }
    }
}

/// A connected, initialized MCP client over a stdio transport.
///
/// Construct via [`Client::connect`], which spawns the server and completes
/// the `initialize` / `notifications/initialized` handshake before returning.
pub struct Client {
    transport: StdioTransport,
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
        let transport =
            StdioTransport::spawn_with_timeout(program, args, tap, options.request_timeout)?;
        let init = Self::handshake(&transport).await?;
        Ok(Client { transport, init })
    }

    /// Run the MCP lifecycle handshake: `initialize` request, then the
    /// `notifications/initialized` notification. Returns the negotiated
    /// [`InitializeResult`].
    async fn handshake(transport: &StdioTransport) -> Result<InitializeResult> {
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
    /// as unsupported.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        if !self.has_capability("tools") {
            return Ok(Vec::new());
        }
        match self.transport.request("tools/list", json!({})).await {
            Ok(result) => extract_list(&result, "tools"),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// List the server's resources. Graceful when unsupported (see
    /// [`Client::list_tools`]).
    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        if !self.has_capability("resources") {
            return Ok(Vec::new());
        }
        match self.transport.request("resources/list", json!({})).await {
            Ok(result) => extract_list(&result, "resources"),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// List the server's prompts. Graceful when unsupported (see
    /// [`Client::list_tools`]).
    pub async fn list_prompts(&self) -> Result<Vec<Prompt>> {
        if !self.has_capability("prompts") {
            return Ok(Vec::new());
        }
        match self.transport.request("prompts/list", json!({})).await {
            Ok(result) => extract_list(&result, "prompts"),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
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
