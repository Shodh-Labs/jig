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
use crate::clients::{self, ClientError, ClientRendering};
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
    /// The client rendering this view was built under, when it is not the
    /// default raw-API one. `None` means `--client api`: the body is exactly
    /// what `jig bench` sends, with no client transformation applied.
    pub client: Option<ClientVariant>,
}

/// A per-client rendering applied on top of the raw API request, plus its cost
/// difference — see [`crate::clients`] for the evidence rules.
#[derive(Debug, Clone)]
pub struct ClientVariant {
    /// The rendering: which client, its evidence level, citation, and the
    /// per-tool transformed names.
    pub rendering: ClientRendering,
    /// Total tokens under the raw API rendering, for comparison.
    pub api_total_tokens: usize,
    /// `total_tokens - api_total_tokens`. Signed: a client that shortens names
    /// (VS Code truncates at 64 characters) can render *cheaper* than the raw
    /// request, and reporting that as a positive number would be a lie.
    pub delta_tokens: i64,
}

impl ClientVariant {
    /// The delta rendered for a person: `+12 tokens vs the raw API request`.
    ///
    /// Always states the baseline, so a reader is never left guessing what the
    /// number is relative to.
    pub fn delta_summary(&self) -> String {
        match self.delta_tokens {
            0 => "identical cost to the raw API request".to_string(),
            d => format!(
                "{}{} token{} vs the raw API request",
                if d > 0 { "+" } else { "-" },
                d.abs(),
                if d.abs() == 1 { "" } else { "s" }
            ),
        }
    }
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
    build_inner(provider, model_id, api_model, tools, instructions, None)
}

/// Like [`build`], but rendered as `client` would present the tool surface.
///
/// The resulting [`ContextView::client`] carries the transformation, its
/// citation, and the token delta against the raw API rendering. Only what the
/// citation establishes is applied — see [`crate::clients`].
///
/// # Errors
///
/// As [`build`], plus [`ContextBuildError::Client`] when the id is unknown or
/// names a client whose rendering no public source establishes. Jig refuses to
/// invent a rendering rather than emitting a plausible fiction.
pub fn build_for_client(
    provider: Provider,
    model_id: &str,
    api_model: &str,
    tools: &[Tool],
    instructions: Option<&str>,
    client: &str,
    server_name: &str,
) -> Result<ContextView, ContextBuildError> {
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    let rendering = clients::render_names(client, server_name, &tool_names)?;

    // The baseline is computed the same way for every client, so the delta is a
    // like-for-like comparison rather than two differently-derived numbers.
    let api_view = build_inner(provider, model_id, api_model, tools, instructions, None)?;
    if rendering.spec.id == clients::DEFAULT_CLIENT {
        return Ok(api_view);
    }

    let mut view = build_inner(
        provider,
        model_id,
        api_model,
        tools,
        instructions,
        Some(&rendering),
    )?;
    view.client = Some(ClientVariant {
        api_total_tokens: api_view.total_tokens,
        delta_tokens: view.total_tokens as i64 - api_view.total_tokens as i64,
        rendering,
    });
    Ok(view)
}

/// Errors from assembling a client-specific context view.
#[derive(Debug, thiserror::Error)]
pub enum ContextBuildError {
    /// The tokenizer/model layer rejected the request.
    #[error(transparent)]
    Token(#[from] TokenError),
    /// The `--client` value was unknown, or names a client whose rendering is
    /// not established by any public source.
    #[error(transparent)]
    Client(#[from] ClientError),
}

fn build_inner(
    provider: Provider,
    model_id: &str,
    api_model: &str,
    tools: &[Tool],
    instructions: Option<&str>,
    rendering: Option<&ClientRendering>,
) -> Result<ContextView, TokenError> {
    let counter = ModelCounter::new(model_id)?;

    let mut body = bench::render_request_parts(
        provider,
        tools,
        CONTEXT_TASK_PLACEHOLDER,
        api_model,
        CONTEXT_TEMPERATURE,
        bench::DEFAULT_MAX_TOKENS,
    );

    // Apply the client's transformation to the body *before* anything is
    // counted, so every per-tool figure and the total describe the same bytes.
    if let Some(r) = rendering {
        clients::apply_to_request_body(&mut body, r);
    }

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
            // Under a client rendering the model sees the *transformed* name;
            // showing the MCP name here would misreport what reaches the model.
            let name = rendering
                .and_then(|r| r.names.get(i))
                .map(|n| n.rendered.clone())
                .unwrap_or_else(|| t.name.clone());
            ToolContext {
                name,
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
        // Filled in by `build_for_client` once the delta is known.
        client: None,
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
    fn default_client_view_carries_no_variant() {
        let tools = vec![tool("echo", Some("Echo it"), json!({ "type": "object" }))];
        let view = build_for_client(
            Provider::OpenAI,
            "gpt-4o",
            "gpt-4o",
            &tools,
            None,
            "api",
            "srv",
        )
        .unwrap();
        assert!(
            view.client.is_none(),
            "`api` is the baseline, not a variant"
        );
        // Byte-identical to the plain build.
        let plain = build(Provider::OpenAI, "gpt-4o", "gpt-4o", &tools, None).unwrap();
        assert_eq!(view.body, plain.body);
        assert_eq!(view.total_tokens, plain.total_tokens);
    }

    #[test]
    fn claude_code_variant_reports_a_positive_delta_and_renames_in_the_body() {
        let tools = vec![tool("echo", Some("Echo it"), json!({ "type": "object" }))];
        let view = build_for_client(
            Provider::Anthropic,
            "claude-sonnet",
            "claude-x",
            &tools,
            None,
            "claude-code",
            "filesystem",
        )
        .unwrap();

        let variant = view.client.as_ref().expect("a variant was requested");
        // The prefix is strictly more text, so the rendering costs more.
        assert!(
            variant.delta_tokens > 0,
            "prefixing must cost tokens: {}",
            variant.delta_tokens
        );
        assert_eq!(
            view.total_tokens as i64,
            variant.api_total_tokens as i64 + variant.delta_tokens
        );
        assert!(variant.delta_summary().contains("vs the raw API request"));

        // The body the model would receive carries the transformed name...
        assert_eq!(view.body["tools"][0]["name"], "mcp__filesystem__echo");
        // ...and so does the per-tool view, so the two never disagree.
        assert_eq!(view.tools[0].name, "mcp__filesystem__echo");
    }

    #[test]
    fn a_verified_no_op_client_reports_a_zero_delta_rather_than_nothing() {
        // The OpenAI Agents SDK verifiably does *not* transform by default.
        // That is a real, citable finding — it must render as an explicit zero,
        // not be silently indistinguishable from an unsupported client.
        let tools = vec![tool("echo", Some("Echo it"), json!({ "type": "object" }))];
        let view = build_for_client(
            Provider::OpenAI,
            "gpt-4o",
            "gpt-4o",
            &tools,
            None,
            "openai-agents",
            "srv",
        )
        .unwrap();
        let variant = view.client.as_ref().unwrap();
        assert_eq!(variant.delta_tokens, 0);
        assert!(variant.rendering.is_identity());
        assert_eq!(
            variant.delta_summary(),
            "identical cost to the raw API request"
        );
    }

    #[test]
    fn an_unestablished_client_is_an_error_not_a_guess() {
        let tools = vec![tool("echo", Some("Echo it"), json!({ "type": "object" }))];
        let err = build_for_client(
            Provider::OpenAI,
            "gpt-4o",
            "gpt-4o",
            &tools,
            None,
            "cursor",
            "srv",
        )
        .unwrap_err();
        assert!(matches!(err, ContextBuildError::Client(_)));
        assert!(err.to_string().contains("will not guess"));
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
