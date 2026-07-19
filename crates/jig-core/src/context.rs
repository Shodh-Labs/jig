//! `jig context` — render **exactly what the model sees**.
//!
//! The product's founding promise: developers write tool descriptions blind,
//! never seeing the context block a model actually receives. This module
//! assembles that block — token-annotated — by *reusing* the bench request
//! machinery ([`crate::bench::render_request_parts`]) so the rendering is
//! byte-identical to the body `jig bench` sends, save for a placeholder task.
//!
//! # Honesty
//!
//! Nothing here is ever sent anywhere and **no API key is needed or read**.
//! The rendered [`ContextView::body`] is the exact provider request body, minus
//! auth (which rides in a header, never the body) and with the user task
//! replaced by [`CONTEXT_TASK_PLACEHOLDER`].
//!
//! The server's `instructions` string is surfaced as its own section because
//! it is real context a developer authored — but `jig bench` sends only the
//! system prompt + tools, so instructions are **not** part of the request body
//! and are excluded from the request [`ContextView::total_tokens`]. The `sent_by_bench`
//! flag on [`InstructionsSection`] records this explicitly.

use serde_json::Value;

use crate::bench::{self, Provider, BENCH_SYSTEM_PROMPT};
use crate::protocol::Tool;
use crate::tokens::{Exactness, ModelCounter, TokenError};

/// The placeholder shown where the user's first message would go. Chosen to be
/// obviously a placeholder, never mistaken for real input.
pub const CONTEXT_TASK_PLACEHOLDER: &str = "<your task here>";

/// The sampling temperature `jig context` renders with — `jig bench`'s default
/// (`temp` defaults to `1.0` on the CLI). Mirrored here so the rendered body
/// matches a default `jig bench` invocation.
pub const CONTEXT_TEMPERATURE: f64 = 1.0;

/// One tool as it appears in the request, with its token cost and a compact
/// human rendering of its input schema.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// The tool name.
    pub name: String,
    /// The tool description, if any.
    pub description: Option<String>,
    /// Tokens this tool's entry contributes to the request body (the compact
    /// serialization of its provider-dialect object).
    pub tokens: usize,
    /// The compact human rendering of the tool's parameters, one line each
    /// (nested object properties indented two spaces; deeper nesting elided).
    pub schema_lines: Vec<String>,
}

/// The server `instructions` section — surfaced but **not** part of the bench
/// request body (see the module honesty note).
#[derive(Debug, Clone)]
pub struct InstructionsSection {
    /// The verbatim instructions string.
    pub text: String,
    /// Its token count under the chosen tokenizer.
    pub tokens: usize,
    /// Always `false` for `jig bench`: the bench request does not forward server
    /// instructions to the provider. Recorded so machine output is unambiguous.
    pub sent_by_bench: bool,
}

/// The fully-assembled context view: the exact request body plus per-section
/// token annotations and provenance. A pure function of the inputs — no I/O.
#[derive(Debug, Clone)]
pub struct ContextView {
    /// The provider dialect the body is rendered in.
    pub provider: Provider,
    /// The canonical model id whose tokenizer annotated the counts.
    pub model_id: String,
    /// The concrete API model string that appears in the body.
    pub api_model: String,
    /// The tiktoken encoding label used for counts (e.g. `o200k_base`).
    pub tokenizer: String,
    /// Exactness of the token counts for the chosen model.
    pub exactness: Exactness,
    /// The minimal system prompt ([`BENCH_SYSTEM_PROMPT`]).
    pub system_prompt: &'static str,
    /// Tokens for the system prompt.
    pub system_tokens: usize,
    /// The server instructions section, if the server offered instructions.
    pub instructions: Option<InstructionsSection>,
    /// Per-tool contexts, **largest token cost first** (ties by name ascending).
    pub tools: Vec<ToolContext>,
    /// Sum of every tool's token cost.
    pub tools_tokens: usize,
    /// Total tokens the model receives from `jig bench` before the user's first
    /// word: the system prompt plus the tools. (Server instructions are shown
    /// but excluded — bench does not send them.)
    pub total_tokens: usize,
    /// The exact provider request body, with the placeholder task.
    pub body: Value,
}

/// Assemble the [`ContextView`] for `tools` (+ optional `instructions`) rendered
/// in `provider`'s dialect and annotated with `model_id`'s tokenizer.
///
/// `api_model` is the concrete model string placed in the body (usually a
/// model's registry mapping, or an `--api-model` override). No key is used.
///
/// # Errors
///
/// Returns [`TokenError::UnknownModel`] if `model_id` is not in the registry, or
/// [`TokenError::Tokenizer`] if the tokenizer cannot be built.
pub fn build(
    provider: Provider,
    model_id: &str,
    api_model: &str,
    tools: &[Tool],
    instructions: Option<&str>,
) -> Result<ContextView, TokenError> {
    let counter = ModelCounter::new(model_id)?;

    let body = bench::render_request_parts(
        provider,
        tools,
        CONTEXT_TASK_PLACEHOLDER,
        api_model,
        CONTEXT_TEMPERATURE,
        bench::DEFAULT_MAX_TOKENS,
    );

    let system_tokens = counter.count(BENCH_SYSTEM_PROMPT);

    let instructions = instructions.map(|text| InstructionsSection {
        tokens: counter.count(text),
        text: text.to_string(),
        sent_by_bench: false,
    });

    // Per-tool token cost = the compact serialization of that tool's entry in
    // the request body's `tools` array (index-aligned with `tools`, since the
    // render maps them in order). This is the tool's true contribution to the
    // bytes the model receives.
    let body_tools = body.get("tools").and_then(Value::as_array);
    let mut tool_ctx: Vec<ToolContext> = tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let tokens = body_tools
                .and_then(|arr| arr.get(i))
                .map(|entry| counter.count(&serde_json::to_string(entry).unwrap_or_default()))
                .unwrap_or(0);
            ToolContext {
                name: t.name.clone(),
                description: t.description.clone(),
                tokens,
                schema_lines: schema_to_human_lines(&t.input_schema),
            }
        })
        .collect();
    // Largest first, ties broken by name ascending — deterministic for snapshots.
    tool_ctx.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.name.cmp(&b.name)));

    let tools_tokens: usize = tool_ctx.iter().map(|t| t.tokens).sum();
    let total_tokens = system_tokens + tools_tokens;

    Ok(ContextView {
        provider,
        model_id: counter.model_id().to_string(),
        api_model: api_model.to_string(),
        tokenizer: counter.encoding_label().to_string(),
        exactness: counter.exactness(),
        system_prompt: BENCH_SYSTEM_PROMPT,
        system_tokens,
        instructions,
        tools: tool_ctx,
        tools_tokens,
        total_tokens,
        body,
    })
}

// ---------------------------------------------------------------------------
// Schema → compact human form (pure — unit-testable)
// ---------------------------------------------------------------------------

/// Maximum object-nesting depth expanded in the human schema rendering. Deeper
/// objects are elided with `{…}`.
const MAX_SCHEMA_DEPTH: usize = 1;

/// Render a tool's JSON Schema as compact human lines: one `name: type
/// (required?) — annotations` line per property.
///
/// Nested object properties are expanded one level, indented two spaces; an
/// object nested deeper than that is elided as `name: {…}`.
/// `enum` values render as `one of [a, b, c]`, descriptions as `"…"`, and a
/// schema with no properties renders the single line `(no parameters)`.
pub fn schema_to_human_lines(schema: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match schema.get("properties").and_then(Value::as_object) {
        Some(props) if !props.is_empty() => {
            let required = required_names(schema);
            for (name, prop) in props {
                render_prop(&mut lines, name, prop, required.contains(&name.as_str()), 0);
            }
        }
        _ => lines.push("(no parameters)".to_string()),
    }
    lines
}

/// Collect the `required` property names declared on an object schema.
fn required_names(schema: &Value) -> std::collections::HashSet<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Whether a property schema is an object with declared properties.
fn is_object_schema(prop: &Value) -> bool {
    prop.get("type").and_then(Value::as_str) == Some("object")
        && prop
            .get("properties")
            .map(Value::is_object)
            .unwrap_or(false)
}

fn render_prop(lines: &mut Vec<String>, name: &str, prop: &Value, required: bool, depth: usize) {
    let indent = "  ".repeat(depth);
    let req = if required { " (required)" } else { "" };

    if is_object_schema(prop) {
        if depth >= MAX_SCHEMA_DEPTH {
            // Too deep to expand — elide the shape but keep the required marker.
            lines.push(format!("{indent}{name}: {{…}}{req}"));
            return;
        }
        let mut line = format!("{indent}{name}: object{req}");
        if let Some(desc) = prop.get("description").and_then(Value::as_str) {
            line.push_str(&format!(" — \"{desc}\""));
        }
        lines.push(line);
        let child_required = required_names(prop);
        if let Some(props) = prop.get("properties").and_then(Value::as_object) {
            for (cname, cprop) in props {
                render_prop(
                    lines,
                    cname,
                    cprop,
                    child_required.contains(&cname.as_str()),
                    depth + 1,
                );
            }
        }
        return;
    }

    let mut line = format!("{indent}{name}: {}{req}", type_label(prop));
    let mut annotations: Vec<String> = Vec::new();
    if let Some(variants) = prop.get("enum").and_then(Value::as_array) {
        let rendered: Vec<String> = variants.iter().map(scalar_display).collect();
        annotations.push(format!("one of [{}]", rendered.join(", ")));
    }
    if let Some(desc) = prop.get("description").and_then(Value::as_str) {
        annotations.push(format!("\"{desc}\""));
    }
    if !annotations.is_empty() {
        line.push_str(" — ");
        line.push_str(&annotations.join(" — "));
    }
    lines.push(line);
}

/// A property's type label: the `type` string, a `a|b` union for a type array,
/// or `any` when no type is declared.
fn type_label(prop: &Value) -> String {
    match prop.get("type") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(types)) => {
            let parts: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            if parts.is_empty() {
                "any".to_string()
            } else {
                parts.join("|")
            }
        }
        _ => "any".to_string(),
    }
}

/// Display an enum value: a bare string (no quotes) for readability, else its
/// compact JSON form.
fn scalar_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: Option<&str>, schema: Value) -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": desc,
            "inputSchema": schema,
        }))
        .unwrap()
    }

    #[test]
    fn schema_simple_required_string_with_description() {
        let lines = schema_to_human_lines(&json!({
            "type": "object",
            "properties": { "message": { "type": "string", "description": "Message to echo" } },
            "required": ["message"],
        }));
        assert_eq!(
            lines,
            vec![r#"message: string (required) — "Message to echo""#]
        );
    }

    #[test]
    fn schema_missing_description_omits_the_dash() {
        let lines = schema_to_human_lines(&json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } },
        }));
        assert_eq!(lines, vec!["count: integer"]);
    }

    #[test]
    fn schema_enum_renders_one_of() {
        let lines = schema_to_human_lines(&json!({
            "type": "object",
            "properties": {
                "seating": { "type": "string", "enum": ["indoor", "outdoor", "bar"] }
            },
        }));
        assert_eq!(
            lines,
            vec!["seating: string — one of [indoor, outdoor, bar]"]
        );
    }

    #[test]
    fn schema_nested_object_indents_one_level() {
        let lines = schema_to_human_lines(&json!({
            "type": "object",
            "properties": {
                "party": {
                    "type": "object",
                    "properties": {
                        "size": { "type": "integer" },
                        "seating": { "type": "string", "enum": ["indoor", "outdoor"] }
                    },
                    "required": ["size"]
                }
            },
            "required": ["party"],
        }));
        assert_eq!(
            lines,
            vec![
                "party: object (required)".to_string(),
                "  seating: string — one of [indoor, outdoor]".to_string(),
                "  size: integer (required)".to_string(),
            ]
        );
    }

    #[test]
    fn schema_deeper_nesting_is_elided() {
        let lines = schema_to_human_lines(&json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "type": "object",
                            "properties": { "x": { "type": "string" } }
                        }
                    },
                    "required": ["inner"]
                }
            },
        }));
        assert_eq!(
            lines,
            vec![
                "outer: object".to_string(),
                "  inner: {…} (required)".to_string(),
            ]
        );
    }

    #[test]
    fn schema_no_properties_says_no_parameters() {
        let lines = schema_to_human_lines(&json!({ "type": "object", "properties": {} }));
        assert_eq!(lines, vec!["(no parameters)"]);
    }

    #[test]
    fn build_orders_tools_largest_first_and_totals() {
        let tools = vec![
            tool(
                "a",
                Some("short"),
                json!({ "type": "object", "properties": {} }),
            ),
            tool(
                "b",
                Some("a much longer description that costs more tokens to render out"),
                json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
            ),
        ];
        let view = build(Provider::OpenAI, "gpt-4o", "gpt-4o", &tools, Some("hi")).unwrap();
        // `b` is larger, so it sorts first.
        assert_eq!(view.tools[0].name, "b");
        assert_eq!(view.tools[1].name, "a");
        // Total = system + tools (instructions excluded).
        assert_eq!(view.total_tokens, view.system_tokens + view.tools_tokens);
        let instr = view.instructions.unwrap();
        assert!(!instr.sent_by_bench);
        assert!(instr.tokens > 0);
    }

    #[test]
    fn build_body_is_the_bench_body_with_placeholder() {
        let tools = vec![tool(
            "echo",
            Some("Echo it"),
            json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
        )];
        let view = build(
            Provider::Anthropic,
            "claude-sonnet",
            "claude-x",
            &tools,
            None,
        )
        .unwrap();
        // Byte-identical to what bench assembles for the same parts + placeholder.
        let bench_body = bench::render_request_parts(
            Provider::Anthropic,
            &tools,
            CONTEXT_TASK_PLACEHOLDER,
            "claude-x",
            CONTEXT_TEMPERATURE,
            bench::DEFAULT_MAX_TOKENS,
        );
        assert_eq!(view.body, bench_body);
        assert_eq!(
            view.body["messages"][0]["content"],
            json!(CONTEXT_TASK_PLACEHOLDER)
        );
    }
}
