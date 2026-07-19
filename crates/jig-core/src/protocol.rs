//! MCP protocol types and constants.
//!
//! These are intentionally lean: fields Jig actually reads are typed; the
//! long tail of optional/negotiable data (capability sub-objects, arbitrary
//! JSON Schemas) is kept as [`serde_json::Value`] so Jig never rejects a
//! server for advertising something new.
//!
//! Verified against the MCP specification revision `2025-06-18`
//! (<https://modelcontextprotocol.io/specification/2025-06-18>).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The latest stable MCP protocol version Jig advertises in `initialize`.
///
/// Per the lifecycle spec the client proposes a version and the server may
/// respond with a different (older) one it supports; Jig accepts the server's
/// negotiated version. See [`crate::client::Client::protocol_version`].
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Identifies an implementation (used for both `clientInfo` and `serverInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Implementation {
    /// Machine name of the implementation.
    pub name: String,
    /// Version string of the implementation.
    pub version: String,
    /// Optional human-facing title (added in later spec revisions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The negotiated result of a successful `initialize` exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// The protocol version the server agreed to speak.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// The server's advertised capabilities. Kept as raw JSON because the set
    /// of capabilities is open-ended and negotiable.
    #[serde(default)]
    pub capabilities: Value,
    /// Identity of the server.
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
    /// Optional free-form instructions the server offers to the client/model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// A tool exposed by the server via `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Unique tool name (the identifier used in `tools/call`).
    pub name: String,
    /// Optional human/model-facing title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional natural-language description shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's arguments. Left as raw JSON.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    /// Optional JSON Schema describing structured output (spec `2025-06-18`).
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
}

/// A resource exposed by the server via `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    /// The resource URI.
    pub uri: String,
    /// Machine name of the resource.
    #[serde(default)]
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional MIME type.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// A prompt template exposed by the server via `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    /// Unique prompt name.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared arguments for the prompt.
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

/// One argument accepted by a [`Prompt`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the argument is required.
    #[serde(default)]
    pub required: bool,
}

/// The result of a `tools/call` invocation.
///
/// Note: `is_error: true` is *not* a Jig-level failure. It is a well-formed
/// protocol response in which the server reports that the tool itself failed
/// (e.g. bad arguments, downstream error). Callers decide how to treat it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// The content blocks the tool returned.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Optional structured content (spec `2025-06-18`).
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    /// Whether the tool reported an error.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// A single content block returned by a tool (or other MCP payloads).
///
/// Unknown block types deserialize into [`ContentBlock::Other`] so a novel
/// content type never fails the whole call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text payload.
        text: String,
    },
    /// Base64-encoded image data.
    Image {
        /// Base64 data.
        data: String,
        /// MIME type of the image.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Base64-encoded audio data.
    Audio {
        /// Base64 data.
        data: String,
        /// MIME type of the audio.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// An embedded resource reference.
    Resource {
        /// The embedded resource object (kept raw).
        resource: Value,
    },
    /// Any content block type Jig does not model explicitly.
    #[serde(other)]
    Other,
}

impl ContentBlock {
    /// Render this block as a short human-readable string for terminal output.
    pub fn render(&self) -> String {
        match self {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Image { mime_type, data } => {
                format!("[image {} ({} base64 bytes)]", mime_type, data.len())
            }
            ContentBlock::Audio { mime_type, data } => {
                format!("[audio {} ({} base64 bytes)]", mime_type, data.len())
            }
            ContentBlock::Resource { resource } => {
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                format!("[resource {uri}]")
            }
            ContentBlock::Other => "[unsupported content block]".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_text_content_block() {
        let v = json!({ "type": "text", "text": "hello" });
        let block: ContentBlock = serde_json::from_value(v).unwrap();
        assert_eq!(block.render(), "hello");
    }

    #[test]
    fn unknown_content_block_is_other_not_error() {
        let v = json!({ "type": "resource_link", "uri": "file:///x" });
        let block: ContentBlock = serde_json::from_value(v).unwrap();
        assert!(matches!(block, ContentBlock::Other));
    }

    #[test]
    fn tool_call_result_defaults_is_error_false() {
        let v = json!({ "content": [{ "type": "text", "text": "ok" }] });
        let res: ToolCallResult = serde_json::from_value(v).unwrap();
        assert!(!res.is_error);
        assert_eq!(res.content.len(), 1);
    }

    #[test]
    fn initialize_result_tolerates_old_version_and_missing_optionals() {
        // A pre-2025 server negotiating an older protocol version, with no
        // `capabilities` and no `instructions`, must still parse: Jig accepts
        // whatever version the server negotiates and treats optional fields as
        // absent rather than rejecting the handshake.
        let v = json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "legacy-server", "version": "0.1.0" }
        });
        let init: InitializeResult = serde_json::from_value(v).unwrap();
        assert_eq!(init.protocol_version, "2024-11-05");
        assert_eq!(init.server_info.name, "legacy-server");
        assert!(init.capabilities.is_null());
        assert!(init.instructions.is_none());
    }

    #[test]
    fn tool_parses_input_schema_as_raw_json() {
        let v = json!({
            "name": "echo",
            "description": "echoes",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } }
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.input_schema["type"], "object");
    }
}
