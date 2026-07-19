//! Human-readable rendering for `jig` terminal output.
//!
//! The `--json` paths bypass this module entirely and emit full, untruncated
//! data; everything here is for the friendly report a person reads.

use jig_core::{Client, Prompt, Resource, Tool, ToolCallResult};
use serde_json::Value;

/// Max characters of a tool description shown in the human report.
const DESC_MAX: usize = 100;

/// Render the full `jig inspect` report.
pub fn inspect_report(
    client: &Client,
    tools: &[Tool],
    resources: &[Resource],
    prompts: &[Prompt],
) -> String {
    let mut s = String::new();
    let info = client.server_info();

    s.push_str(&format!("Server:       {} v{}\n", info.name, info.version));
    s.push_str(&format!("Protocol:     {}\n", client.protocol_version()));
    s.push_str(&format!(
        "Capabilities: {}\n",
        summarize_capabilities(client.capabilities())
    ));
    if let Some(instr) = client.instructions() {
        s.push_str(&format!("Instructions: {}\n", truncate(instr, DESC_MAX)));
    }
    s.push('\n');

    // Tools.
    s.push_str(&format!("Tools ({}):\n", tools.len()));
    if tools.is_empty() {
        s.push_str("  (none)\n");
    }
    for tool in tools {
        let title = tool.title.as_deref().unwrap_or(&tool.name);
        match &tool.description {
            Some(d) => s.push_str(&format!("  - {}  —  {}\n", title, truncate(d, DESC_MAX))),
            None => s.push_str(&format!("  - {}\n", title)),
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

/// Summarize a capabilities object as a comma-separated list of advertised
/// keys, annotating notable sub-flags (e.g. `tools(listChanged)`).
fn summarize_capabilities(caps: &Value) -> String {
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
        if flags.is_empty() {
            parts.push(key.clone());
        } else {
            parts.push(format!("{}({})", key, flags.join(",")));
        }
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

/// Truncate `s` to at most `max` characters (on a char boundary), appending an
/// ellipsis when truncated.
fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let out = summarize_capabilities(&caps);
        assert!(out.contains("tools(listChanged)"));
        assert!(out.contains("prompts"));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        let long = "x".repeat(200);
        let out = truncate(&long, 100);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
    }
}
