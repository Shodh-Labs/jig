//! Human-readable rendering for `jig` terminal output.
//!
//! The human report is what a person reads; the `--json` path emits full,
//! untruncated data. Both are built here from plain data (not a live
//! [`Client`]) so their exact output can be locked with snapshot tests.

use jig_core::{
    Client, Implementation, ListenSummary, Prompt, PromptGetResult, Resource, ResourceReadResult,
    Tool, ToolCallResult,
};
use serde_json::{json, Value};

/// Build the machine-readable `jig inspect --json` document from plain data.
///
/// Kept a pure function of its inputs (rather than reaching into a [`Client`])
/// so the exact JSON shape is snapshot-testable and stable across refactors.
pub fn inspect_json_doc(
    server_info: &Implementation,
    protocol_version: &str,
    capabilities: &Value,
    instructions: Option<&str>,
    tools: &[Tool],
    resources: &[Resource],
    prompts: &[Prompt],
) -> Value {
    json!({
        "serverInfo": server_info,
        "protocolVersion": protocol_version,
        "capabilities": capabilities,
        "instructions": instructions,
        "tools": tools,
        "resources": resources,
        "prompts": prompts,
    })
}

/// Max characters of a tool description shown in the human report.
const DESC_MAX: usize = 100;

/// Render the full `jig inspect` report from a live client.
///
/// A thin adapter over [`inspect_report_from`]: it pulls the header fields off
/// the [`Client`] so the rendering itself stays a pure function of plain data
/// (and thus snapshot-testable without a live connection).
pub fn inspect_report(
    client: &Client,
    tools: &[Tool],
    resources: &[Resource],
    prompts: &[Prompt],
) -> String {
    let info = client.server_info();
    inspect_report_from(
        &info.name,
        &info.version,
        client.protocol_version(),
        client.capabilities(),
        client.instructions(),
        tools,
        resources,
        prompts,
    )
}

/// Render the `jig inspect` report from plain data (no [`Client`]).
#[allow(clippy::too_many_arguments)]
fn inspect_report_from(
    server_name: &str,
    server_version: &str,
    protocol_version: &str,
    capabilities: &Value,
    instructions: Option<&str>,
    tools: &[Tool],
    resources: &[Resource],
    prompts: &[Prompt],
) -> String {
    let mut s = String::new();

    s.push_str(&format!(
        "Server:       {} v{}\n",
        server_name, server_version
    ));
    s.push_str(&format!("Protocol:     {}\n", protocol_version));
    s.push_str(&format!(
        "Capabilities: {}\n",
        summarize_capabilities(capabilities, protocol_version)
    ));
    if let Some(instr) = instructions {
        s.push_str(&format!("Instructions: {}\n", truncate(instr, DESC_MAX)));
    }
    s.push('\n');

    // Tools.
    s.push_str(&format!("Tools ({}):\n", tools.len()));
    if tools.is_empty() {
        s.push_str("  (none)\n");
    }
    for tool in tools {
        // The callable `name` is always the primary identifier (copy it into
        // `jig call --tool`); the human `title` is a secondary annotation.
        match &tool.title {
            Some(t) => s.push_str(&format!("  - {} — \"{}\"\n", tool.name, t)),
            None => s.push_str(&format!("  - {}\n", tool.name)),
        }
        if let Some(d) = &tool.description {
            s.push_str(&format!("      {}\n", truncate(d, DESC_MAX)));
        }
        s.push_str(&format!(
            "      input: {}\n",
            summarize_schema(&tool.input_schema)
        ));
    }

    // Resources.
    s.push('\n');
    s.push_str(&format!("Resources ({}):\n", resources.len()));
    if resources.is_empty() {
        s.push_str("  (none advertised)\n");
    }
    for r in resources {
        let name = if r.name.is_empty() { &r.uri } else { &r.name };
        s.push_str(&format!("  - {} ({})\n", name, r.uri));
    }

    // Prompts.
    s.push('\n');
    s.push_str(&format!("Prompts ({}):\n", prompts.len()));
    if prompts.is_empty() {
        s.push_str("  (none advertised)\n");
    }
    for p in prompts {
        match &p.description {
            Some(d) => s.push_str(&format!("  - {}  —  {}\n", p.name, truncate(d, DESC_MAX))),
            None => s.push_str(&format!("  - {}\n", p.name)),
        }
    }

    s
}

/// Render a `jig call` result for a person.
pub fn call_result(tool: &str, result: &ToolCallResult) -> String {
    let mut s = String::new();
    let status = if result.is_error { "ERROR" } else { "ok" };
    s.push_str(&format!("Tool:   {tool}\n"));
    s.push_str(&format!("Status: {status}\n"));
    s.push_str("Result:\n");

    if result.content.is_empty() {
        s.push_str("  (no content)\n");
    }
    for block in &result.content {
        for line in block.render().lines() {
            s.push_str(&format!("  {line}\n"));
        }
    }

    if let Some(structured) = &result.structured_content {
        s.push_str("Structured content:\n");
        let pretty =
            serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
        for line in pretty.lines() {
            s.push_str(&format!("  {line}\n"));
        }
    }

    s
}

/// Render a `jig read` result (`resources/read`) for a person.
///
/// Text contents are shown verbatim; a blob is summarized as its MIME type and
/// base64 length — never dumped as raw bytes to the terminal (use `--json` for
/// the full base64).
pub fn resource_read_result(uri: &str, result: &ResourceReadResult) -> String {
    let mut s = String::new();
    s.push_str(&format!("Resource: {uri}\n"));
    s.push_str(&format!("Contents ({}):\n", result.contents.len()));
    if result.contents.is_empty() {
        s.push_str("  (no contents)\n");
    }
    for c in &result.contents {
        let mime = c.mime_type().unwrap_or("(no mimeType)");
        s.push_str(&format!("  - {} [{}]\n", c.uri(), mime));
        for line in c.render().lines() {
            s.push_str(&format!("      {line}\n"));
        }
    }
    s
}

/// Render a `jig prompt` result (`prompts/get`) for a person: the optional
/// description followed by each message's role and content.
pub fn prompt_get_result(name: &str, result: &PromptGetResult) -> String {
    let mut s = String::new();
    s.push_str(&format!("Prompt: {name}\n"));
    if let Some(d) = &result.description {
        s.push_str(&format!("Description: {}\n", truncate(d, DESC_MAX)));
    }
    s.push_str(&format!("Messages ({}):\n", result.messages.len()));
    if result.messages.is_empty() {
        s.push_str("  (no messages)\n");
    }
    for m in &result.messages {
        s.push_str(&format!("  [{}]\n", m.role));
        for line in m.content.render().lines() {
            s.push_str(&format!("      {line}\n"));
        }
    }
    s
}

/// Render the standalone-GET-stream listening summary for a person.
pub fn listen_summary(summary: &ListenSummary) -> String {
    let secs = summary.duration.as_secs_f64();
    if !summary.opened {
        return format!(
            "GET stream: not offered (HTTP {}). The server has no standalone server→client \
             stream — spec-permitted, not an error.\n",
            summary.status
        );
    }
    format!(
        "GET stream: opened (HTTP {}). Observed {} notification(s), {} server ping(s), \
         {} other server request(s) in {:.1}s. (See the tap for full detail.)\n",
        summary.status, summary.notifications, summary.pings, summary.other_requests, secs
    )
}

/// Summarize a capabilities object as a comma-separated list of advertised
/// keys, annotating notable sub-flags (e.g. `tools(listChanged)`) and flagging
/// any capability not defined in the **negotiated** spec revision. Legality is
/// version-relative — the same version-aware table `jig check` grades against
/// (see [`jig_core::capability_offspec_note`]) — so a capability like `tasks` is
/// flagged under `2025-06-18` but not under a revision that defines it.
fn summarize_capabilities(caps: &Value, protocol_version: &str) -> String {
    let Some(map) = caps.as_object() else {
        return "(none)".to_string();
    };
    if map.is_empty() {
        return "(none)".to_string();
    }
    let mut parts = Vec::new();
    for (key, val) in map {
        let mut flags = Vec::new();
        if let Some(inner) = val.as_object() {
            for flag in ["listChanged", "subscribe"] {
                if inner.get(flag).and_then(Value::as_bool) == Some(true) {
                    flags.push(flag);
                }
            }
        }
        let mut label = if flags.is_empty() {
            key.clone()
        } else {
            format!("{}({})", key, flags.join(","))
        };
        if let Some(note) = jig_core::capability_offspec_note(key, protocol_version) {
            label.push_str(&format!(" ({note})"));
        }
        parts.push(label);
    }
    parts.join(", ")
}

/// Summarize a JSON Schema as a compact one-liner, e.g.
/// `object { text: string } required: [text]`.
fn summarize_schema(schema: &Value) -> String {
    let Some(obj) = schema.as_object() else {
        return "(no schema)".to_string();
    };

    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("object");
    if ty != "object" {
        return ty.to_string();
    }

    let mut fields = Vec::new();
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (name, spec) in props {
            fields.push(format!("{}: {}", name, field_type(spec)));
        }
    }

    let mut out = if fields.is_empty() {
        "object {}".to_string()
    } else {
        format!("object {{ {} }}", fields.join(", "))
    };

    if let Some(req) = obj.get("required").and_then(Value::as_array) {
        let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            out.push_str(&format!("  required: [{}]", names.join(", ")));
        }
    }
    out
}

/// Describe a single schema property's type compactly, surfacing enums and
/// nested object/array shapes one level deep.
fn field_type(spec: &Value) -> String {
    let Some(obj) = spec.as_object() else {
        return "any".to_string();
    };

    if let Some(variants) = obj.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = variants.iter().map(compact_value).collect();
        return format!("enum[{}]", names.join("|"));
    }

    match obj.get("type").and_then(Value::as_str) {
        Some("object") => {
            // Name the nested keys so a reviewer sees the shape at a glance.
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                let keys: Vec<&str> = props.keys().map(String::as_str).collect();
                format!("object{{{}}}", keys.join(","))
            } else {
                "object".to_string()
            }
        }
        Some("array") => {
            let item = obj
                .get("items")
                .map(field_type)
                .unwrap_or_else(|| "any".to_string());
            format!("array<{item}>")
        }
        Some(other) => other.to_string(),
        None => "any".to_string(),
    }
}

/// Render a JSON scalar compactly for enum listings (strings without quotes).
fn compact_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Collapse every run of whitespace (including newlines and tabs) into a single
/// space and trim the ends. The human report lays out descriptions and
/// instructions as single-line cells; real servers embed newlines in those
/// fields (e.g. server-memory's multi-paragraph tool descriptions), which would
/// otherwise smear across lines and destroy the report's alignment.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate `s` to at most `max` characters (on a char boundary), appending an
/// ellipsis when truncated. Internal whitespace is collapsed first so the
/// result is always a single tidy line regardless of what the server sent.
fn truncate(s: &str, max: usize) -> String {
    let flat = collapse_whitespace(s);
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    // `json` comes in via `super::*` (used by the renderers themselves).
    use super::*;

    #[test]
    fn schema_summary_simple_object() {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        });
        assert_eq!(
            summarize_schema(&schema),
            "object { text: string }  required: [text]"
        );
    }

    #[test]
    fn schema_summary_surfaces_enum_and_nested_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "party": {
                    "type": "object",
                    "properties": { "size": { "type": "integer" }, "seating": { "type": "string" } }
                },
                "mode": { "enum": ["a", "b"] }
            }
        });
        let out = summarize_schema(&schema);
        assert!(out.contains("party: object{"), "got: {out}");
        assert!(out.contains("mode: enum[a|b]"), "got: {out}");
    }

    #[test]
    fn capabilities_summary_lists_keys_with_flags() {
        let caps = json!({ "tools": { "listChanged": true }, "prompts": {} });
        let out = summarize_capabilities(&caps, "2025-06-18");
        assert!(out.contains("tools(listChanged)"));
        assert!(out.contains("prompts"));
    }

    #[test]
    fn capabilities_summary_flags_offspec_capability_version_aware() {
        let caps = json!({ "tools": {}, "tasks": {} });
        // Under 2025-06-18, `tasks` is off-spec (first defined in 2025-11-25).
        let out = summarize_capabilities(&caps, "2025-06-18");
        assert!(
            out.contains("tasks (not defined in negotiated revision 2025-06-18"),
            "got: {out}"
        );
        assert!(
            out.contains("2025-11-25"),
            "names where tasks is defined: {out}"
        );
        assert!(!out.contains("tools (not defined"), "got: {out}");

        // Under 2025-11-25, `tasks` is in-spec and not annotated.
        let out = summarize_capabilities(&caps, "2025-11-25");
        assert!(!out.contains("tasks (not defined"), "got: {out}");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        let long = "x".repeat(200);
        let out = truncate(&long, 100);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_collapses_embedded_newlines_to_single_line() {
        // Real servers (e.g. server-memory, server-everything) put newlines and
        // blank lines inside descriptions/instructions. The report cell must
        // stay a single line so the layout survives.
        let multiline = "First paragraph.\n\nSecond paragraph.\n  indented\ttab";
        let out = truncate(multiline, 100);
        assert!(!out.contains('\n'), "must be single line: {out:?}");
        assert!(!out.contains('\t'), "must have no tabs: {out:?}");
        assert_eq!(out, "First paragraph. Second paragraph. indented tab");
    }

    // ---- Snapshot fixtures: the jig-mock-server tool surface ----------------

    /// The three tools jig-mock-server exposes, as stable fixture data for the
    /// snapshot tests. Kept in sync with `jig-mock-server`'s `handle_tools_list`
    /// so the snapshots reflect a real server's output shape.
    fn mock_tools() -> Vec<Tool> {
        serde_json::from_value(json!([
            {
                "name": "echo",
                "description": "Echo the provided text straight back.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string", "description": "Text to echo." } },
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
                                "seating": { "type": "string", "enum": ["indoor", "outdoor", "bar"] }
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
        ]))
        .unwrap()
    }

    #[test]
    fn inspect_report_snapshot() {
        // Mirrors the mock server: only `tools` advertised (resources/prompts
        // absent), a fixed instructions string.
        let caps = json!({ "tools": {} });
        let report = inspect_report_from(
            "jig-mock-server",
            "0.1.0",
            "2025-06-18",
            &caps,
            Some("A toy MCP server for exercising Jig."),
            &mock_tools(),
            &[],
            &[],
        );
        insta::assert_snapshot!("inspect_report", report);
    }

    #[test]
    fn inspect_json_snapshot() {
        let caps = json!({ "tools": {} });
        let doc = inspect_json_doc(
            &Implementation {
                name: "jig-mock-server".to_string(),
                version: "0.1.0".to_string(),
                title: None,
            },
            "2025-06-18",
            &caps,
            Some("A toy MCP server for exercising Jig."),
            &mock_tools(),
            &[],
            &[],
        );
        let pretty = serde_json::to_string_pretty(&doc).unwrap();
        insta::assert_snapshot!("inspect_json", pretty);
    }

    #[test]
    fn call_result_ok_snapshot() {
        let result: ToolCallResult = serde_json::from_value(json!({
            "content": [ { "type": "text", "text": "echo: hello jig" } ],
            "isError": false
        }))
        .unwrap();
        insta::assert_snapshot!("call_result_ok", call_result("echo", &result));
    }

    #[test]
    fn call_result_error_snapshot() {
        let result: ToolCallResult = serde_json::from_value(json!({
            "content": [ { "type": "text", "text": "This tool always fails, by design." } ],
            "isError": true
        }))
        .unwrap();
        insta::assert_snapshot!("call_result_error", call_result("always_fails", &result));
    }

    #[test]
    fn read_result_text_snapshot() {
        let result: ResourceReadResult = serde_json::from_value(json!({
            "contents": [
                {
                    "uri": "mock://text/hello",
                    "mimeType": "text/plain",
                    "text": "Hello from a jig mock text resource.\nSecond line."
                }
            ]
        }))
        .unwrap();
        insta::assert_snapshot!(
            "read_result_text",
            resource_read_result("mock://text/hello", &result)
        );
    }

    #[test]
    fn read_result_blob_snapshot() {
        // A blob renders as a summary (mime + base64 length), never raw bytes.
        let result: ResourceReadResult = serde_json::from_value(json!({
            "contents": [
                {
                    "uri": "mock://blob/logo",
                    "mimeType": "image/png",
                    "blob": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGP4DwABBAEAHnGpJQAAAABJRU5ErkJggg=="
                }
            ]
        }))
        .unwrap();
        insta::assert_snapshot!(
            "read_result_blob",
            resource_read_result("mock://blob/logo", &result)
        );
    }

    #[test]
    fn prompt_result_snapshot() {
        let result: PromptGetResult = serde_json::from_value(json!({
            "description": "A friendly greeting.",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "Please greet Ada warmly." } }
            ]
        }))
        .unwrap();
        insta::assert_snapshot!("prompt_result", prompt_get_result("greet", &result));
    }

    #[test]
    fn listen_summary_opened_snapshot() {
        let summary = ListenSummary {
            opened: true,
            status: 200,
            notifications: 2,
            pings: 1,
            other_requests: 1,
            duration: std::time::Duration::from_secs_f64(10.0),
        };
        insta::assert_snapshot!("listen_summary_opened", listen_summary(&summary));
    }

    #[test]
    fn listen_summary_405_snapshot() {
        let summary = ListenSummary {
            opened: false,
            status: 405,
            ..Default::default()
        };
        insta::assert_snapshot!("listen_summary_405", listen_summary(&summary));
    }
}
