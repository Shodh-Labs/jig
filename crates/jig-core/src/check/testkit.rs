//! Shared fixtures for the `check` module's unit tests.

use super::*;
use serde_json::json;

pub(crate) fn tool(name: &str, desc: Option<&str>, schema: Value) -> Tool {
    let mut m = serde_json::Map::new();
    m.insert("name".to_string(), json!(name));
    if let Some(d) = desc {
        m.insert("description".to_string(), json!(d));
    }
    m.insert("inputSchema".to_string(), schema);
    serde_json::from_value(Value::Object(m)).unwrap()
}

/// Like [`tool`], but carrying a real behavioural annotation as a **sibling**
/// of `inputSchema` — the placement MCP `2025-06-18` specifies. Hints buried
/// inside the input schema annotate nothing and must not be mistaken for
/// these.
pub(crate) fn tool_annotated(name: &str, desc: Option<&str>, schema: Value) -> Tool {
    let mut m = serde_json::Map::new();
    m.insert("name".to_string(), json!(name));
    if let Some(d) = desc {
        m.insert("description".to_string(), json!(d));
    }
    m.insert("inputSchema".to_string(), schema);
    m.insert("annotations".to_string(), json!({ "readOnlyHint": true }));
    serde_json::from_value(Value::Object(m)).unwrap()
}

/// A clean input over the three mock-server tools.
pub(crate) fn clean_input() -> CheckInput {
    CheckInput {
        server_name: "jig-mock-server".to_string(),
        server_version: "0.1.0".to_string(),
        protocol_version: "2025-06-18".to_string(),
        capabilities: json!({ "tools": {} }),
        instructions: Some("A toy MCP server for exercising Jig.".to_string()),
        tools: vec![
            tool(
                "echo",
                Some("Echo the provided text straight back."),
                json!({ "type": "object", "properties": { "text": { "type": "string", "description": "Text to echo." } }, "required": ["text"] }),
            ),
            tool(
                "make_reservation",
                Some("Book a table. Demonstrates a nested object argument and an enum."),
                json!({ "type": "object", "properties": {
                        "party": { "type": "object", "properties": { "size": { "type": "integer" } } },
                        "date": { "type": "string", "description": "ISO-8601 date." }
                    }, "required": ["party", "date"] }),
            ),
            tool(
                "always_fails",
                Some("A tool that always reports an error, for testing error paths."),
                json!({ "type": "object", "properties": {} }),
            ),
        ],
        observations: Observations {
            pollution_lines: 0,
            list_latency: Some(Duration::from_millis(12)),
            clean_shutdown: true,
            // A conformant server: unknown methods → -32601.
            unknown_method: UnknownMethodProbe::Errored(-32601),
            ..Default::default()
        },
    }
}

/// A [`CheckInput`] over `tools` with everything else clean, so a test can
/// isolate one dimension.
pub(crate) fn input_with_tools(tools: Vec<Tool>) -> CheckInput {
    CheckInput {
        tools,
        ..clean_input()
    }
}

/// `n` tools of which the first `defective` are defective in **every**
/// schema-hygiene class at once (no tool description, one parameter with
/// neither a description nor a type, no annotations) and the rest are clean
/// in every class.
///
/// Because each tool carries exactly one parameter, the tool-level and
/// parameter-level denominators are both `n`, so every class has the same
/// defect rate `defective / n` — which makes the resulting score exactly
/// `100 - 85 * rate` and independent of `n`.
pub(crate) fn schema_rate_tools(n: usize, defective: usize) -> Vec<Tool> {
    (0..n)
        .map(|i| {
            if i < defective {
                tool(
                    &format!("tool_{i}"),
                    None,
                    json!({ "type": "object", "properties": { "arg": {} } }),
                )
            } else {
                tool_annotated(
                    &format!("tool_{i}"),
                    Some("Does a specific, well-described thing for the caller."),
                    json!({
                        "type": "object",
                        "properties": {
                            "arg": { "type": "string", "description": "The argument." }
                        }
                    }),
                )
            }
        })
        .collect()
}

pub(crate) fn schema_score(n: usize, defective: usize) -> f64 {
    evaluate(&input_with_tools(schema_rate_tools(n, defective)), None)
        .dimension(Dimension::SchemaHygiene)
        .unwrap()
        .score
        .unwrap()
}
