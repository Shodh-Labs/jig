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
    /// Optional behavioural hints. A **sibling** of `inputSchema` on the tool
    /// object, never nested inside it — see [`ToolAnnotations`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Behavioural hints a server may attach to a [`Tool`] (`annotations`), per MCP
/// `2025-06-18`.
///
/// # These are hints, not guarantees
///
/// The specification is explicit that every property here is advisory:
///
/// > NOTE: all properties in ToolAnnotations are **hints**. They are not
/// > guaranteed to provide a faithful description of tool behavior (including
/// > descriptive properties like `title`).
///
/// and, normatively, that
///
/// > For trust & safety and security, clients **MUST** consider tool
/// > annotations to be untrusted unless they come from trusted servers.
///
/// Jig only ever *reports* what a server declared. It never acts on an
/// annotation — it does not, for example, treat `readOnlyHint: true` as licence
/// to invoke a tool.
///
/// # Placement
///
/// `annotations` sits alongside `description`, `inputSchema` and `outputSchema`
/// on the tool object. Hints found inside the *input schema* are not
/// annotations; before this type existed Jig sniffed for them there, which
/// produced both false positives and false negatives.
///
/// # Not counted in the token budget
///
/// Annotations are MCP transport metadata consumed **client-side** (display
/// names, permission gating). They are not part of the `tools` array a client
/// sends to a model provider — the Anthropic Messages API tool object accepts
/// `name`, `description`, `input_schema` (and provider-specific extras), and has
/// no `annotations` member. They therefore cost zero prompt tokens and are
/// deliberately excluded from
/// [`canonical_tool_json`](crate::tokens::canonical_tool_json).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// A human-readable title for the tool. Display-name precedence per spec is
    /// `Tool::title`, then `annotations.title`, then `Tool::name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// If true, the tool does not modify its environment. Spec default `false`.
    #[serde(
        rename = "readOnlyHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only_hint: Option<bool>,
    /// If true, the tool may perform destructive updates. Meaningful only when
    /// `read_only_hint` is not true. Spec default `true`.
    #[serde(
        rename = "destructiveHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub destructive_hint: Option<bool>,
    /// If true, repeated calls with the same arguments have no additional
    /// effect. Meaningful only when `read_only_hint` is not true. Spec default
    /// `false`.
    #[serde(
        rename = "idempotentHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotent_hint: Option<bool>,
    /// If true, the tool may interact with an open world of external entities.
    /// Spec default `true`.
    #[serde(
        rename = "openWorldHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    /// Whether the server declared **any** annotation field.
    ///
    /// A tool carrying a bare `"annotations": {}` declares none: the object is
    /// present but says nothing about the tool's behaviour, so it is not
    /// evidence that the server annotated anything.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.read_only_hint.is_none()
            && self.destructive_hint.is_none()
            && self.idempotent_hint.is_none()
            && self.open_world_hint.is_none()
    }

    /// The behavioural hints only — `title` is a display concern, not a
    /// behavioural claim, so it does not count here.
    pub fn has_behavioural_hint(&self) -> bool {
        self.read_only_hint.is_some()
            || self.destructive_hint.is_some()
            || self.idempotent_hint.is_some()
            || self.open_world_hint.is_some()
    }
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

/// The result of a `resources/read` request.
///
/// A single URI may yield more than one content item (e.g. a directory-like
/// resource), though most yield exactly one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReadResult {
    /// The content items returned for the requested URI.
    #[serde(default)]
    pub contents: Vec<ResourceContents>,
}

/// One content item inside a [`ResourceReadResult`]: either UTF-8 `text` or a
/// base64-encoded `blob`, per the MCP resource-contents shape (`2025-06-18`).
///
/// Deserialized untagged: an item carrying a `text` field is [`Text`], one
/// carrying a `blob` field is [`Blob`]. Binary contents are never decoded here
/// — the base64 is preserved verbatim so a terminal is never flooded with raw
/// bytes; renderers decide how to present it.
///
/// [`Text`]: ResourceContents::Text
/// [`Blob`]: ResourceContents::Blob
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResourceContents {
    /// Textual contents.
    Text {
        /// The resource URI these contents belong to.
        uri: String,
        /// Optional MIME type.
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// The UTF-8 text payload.
        text: String,
    },
    /// Binary contents, base64-encoded.
    Blob {
        /// The resource URI these contents belong to.
        uri: String,
        /// Optional MIME type.
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// The base64-encoded binary payload.
        blob: String,
    },
}

impl ResourceContents {
    /// The URI these contents belong to.
    pub fn uri(&self) -> &str {
        match self {
            ResourceContents::Text { uri, .. } | ResourceContents::Blob { uri, .. } => uri,
        }
    }

    /// The declared MIME type, if any.
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            ResourceContents::Text { mime_type, .. } | ResourceContents::Blob { mime_type, .. } => {
                mime_type.as_deref()
            }
        }
    }

    /// Render this content item for a person. Text is shown verbatim; a blob is
    /// summarized as its MIME type and base64 length — never dumped as raw bytes
    /// to a terminal.
    pub fn render(&self) -> String {
        match self {
            ResourceContents::Text { text, .. } => text.clone(),
            ResourceContents::Blob {
                mime_type, blob, ..
            } => {
                format!(
                    "[blob {} ({} base64 chars)]",
                    mime_type.as_deref().unwrap_or("application/octet-stream"),
                    blob.len()
                )
            }
        }
    }
}

/// The result of a `prompts/get` request: an optional description and the
/// rendered prompt messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptGetResult {
    /// Optional human-readable description of the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The messages the prompt expands to.
    #[serde(default)]
    pub messages: Vec<PromptMessage>,
}

/// One message inside a [`PromptGetResult`]: a role plus one content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    /// The speaker: `"user"` or `"assistant"`.
    pub role: String,
    /// The message content (text/image/audio/embedded-resource).
    pub content: ContentBlock,
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
    fn resource_contents_text_and_blob_parse_untagged() {
        let text: ResourceContents = serde_json::from_value(json!({
            "uri": "mock://text", "mimeType": "text/plain", "text": "hello"
        }))
        .unwrap();
        assert!(matches!(text, ResourceContents::Text { .. }));
        assert_eq!(text.render(), "hello");
        assert_eq!(text.uri(), "mock://text");

        let blob: ResourceContents = serde_json::from_value(json!({
            "uri": "mock://blob", "mimeType": "application/octet-stream", "blob": "QUJD"
        }))
        .unwrap();
        assert!(matches!(blob, ResourceContents::Blob { .. }));
        // A blob is summarized (mime + base64 length), never dumped verbatim.
        assert_eq!(
            blob.render(),
            "[blob application/octet-stream (4 base64 chars)]"
        );
    }

    #[test]
    fn prompt_get_result_parses_role_and_content() {
        let res: PromptGetResult = serde_json::from_value(json!({
            "description": "greet someone",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "Hi Ada" } }
            ]
        }))
        .unwrap();
        assert_eq!(res.description.as_deref(), Some("greet someone"));
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].role, "user");
        assert_eq!(res.messages[0].content.render(), "Hi Ada");
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
        // No `annotations` sent → the typed field is absent, not a default.
        assert!(tool.annotations.is_none());
    }

    #[test]
    fn tool_annotations_parse_as_a_sibling_of_input_schema() {
        let v = json!({
            "name": "delete_file",
            "inputSchema": { "type": "object" },
            "annotations": {
                "title": "Delete a file",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        let a = tool.annotations.expect("annotations should parse");
        assert_eq!(a.title.as_deref(), Some("Delete a file"));
        assert_eq!(a.read_only_hint, Some(false));
        assert_eq!(a.destructive_hint, Some(true));
        assert_eq!(a.idempotent_hint, Some(true));
        assert_eq!(a.open_world_hint, Some(false));
        assert!(!a.is_empty());
        assert!(a.has_behavioural_hint());
    }

    #[test]
    fn hints_inside_the_input_schema_are_not_annotations() {
        // The pre-typed heuristic sniffed the *input schema* for `*Hint` keys.
        // Per spec these are properties of the arguments object, not tool
        // annotations, and must not be mistaken for them.
        let v = json!({
            "name": "misleading",
            "inputSchema": {
                "type": "object",
                "properties": { "readOnlyHint": { "type": "boolean" } },
                "annotations": {}
            }
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        assert!(tool.annotations.is_none());
    }

    #[test]
    fn empty_annotations_object_declares_nothing() {
        let v = json!({
            "name": "bare",
            "inputSchema": { "type": "object" },
            "annotations": {}
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        let a = tool.annotations.expect("the object itself is present");
        assert!(a.is_empty());
        assert!(!a.has_behavioural_hint());
    }

    #[test]
    fn annotation_title_alone_is_not_a_behavioural_hint() {
        let v = json!({
            "name": "t",
            "inputSchema": { "type": "object" },
            "annotations": { "title": "Pretty name" }
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        let a = tool.annotations.unwrap();
        assert!(!a.is_empty());
        assert!(!a.has_behavioural_hint());
    }

    #[test]
    fn unknown_annotation_keys_do_not_fail_the_tool() {
        // A future spec revision adding a hint must not break parsing.
        let v = json!({
            "name": "t",
            "inputSchema": { "type": "object" },
            "annotations": { "readOnlyHint": true, "futureHint": "whatever" }
        });
        let tool: Tool = serde_json::from_value(v).unwrap();
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(true));
    }
}
