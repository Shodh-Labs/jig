//! `jig context` — see exactly what the model sees.
//!
//! Connects once, lists the tool surface and captures the server instructions,
//! then renders the **exact provider API request body** that `jig bench` would
//! send — tools mapped to the provider dialect, the system prompt, and a
//! placeholder user message — token-annotated per section. It reuses
//! [`jig_core::context`] (which reuses the bench request assembly), so the body
//! shown is byte-identical to bench's, minus the placeholder task.
//!
//! `jig context` needs **no API key** and sends nothing anywhere; the rendering
//! functions are pure over a [`ContextView`], so the human / `--raw` / `--json`
//! surfaces are all snapshot-testable.

use std::process::ExitCode;

use jig_core::bench::{BenchModel, Provider};
use jig_core::context::{self, ContextView};
use jig_core::{Implementation, ProtocolTap};
use serde_json::{json, Value};

use crate::{emit, warn_non_protocol_output, write_tap_if_requested, Target};

/// Run `jig context`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    model: Option<String>,
    api_model: Option<String>,
    provider_override: Option<Provider>,
    raw: bool,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    // Resolve the model (default: same logic as bench) and its provider dialect.
    // No key is ever required or read — context sends nothing.
    let model_id = model.unwrap_or_else(|| crate::bench::default_model().to_string());
    let mut resolved = BenchModel::resolve(&model_id).map_err(|e| e.to_string())?;
    if let Some(over) = api_model {
        resolved = resolved.with_api_model(over);
    }
    let provider = provider_override.unwrap_or(resolved.provider);

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        provider,
        &resolved.id,
        &resolved.api_model,
        raw,
        as_json,
        timeout_secs,
        max_message_bytes,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    target: &Target,
    tap: ProtocolTap,
    provider: Provider,
    model_id: &str,
    api_model: &str,
    raw: bool,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;
    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let instructions = client.instructions().map(|s| s.to_string());
    let server = client.server_info().clone();
    client.shutdown().await.map_err(|e| e.to_string())?;

    let view = context::build(
        provider,
        model_id,
        api_model,
        &tools,
        instructions.as_deref(),
    )
    .map_err(|e| e.to_string())?;

    if as_json {
        emit(&render_json(&server, &view));
    } else if raw {
        emit(&render_raw(&view));
    } else {
        emit(&render_human(&server, &view));
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Human rendering (the product surface)
// ---------------------------------------------------------------------------

/// The column at which right-aligned token annotations sit.
const TOK_COL: usize = 62;
/// How many lines of the server instructions to preview before eliding.
const INSTRUCTIONS_PREVIEW_LINES: usize = 3;

/// Render the structured, readable human view.
pub(crate) fn render_human(server: &Implementation, view: &ContextView) -> String {
    let mut s = String::new();

    // Header: what <model> (<dialect>) receives from <server> vX — nothing sent.
    let left = format!(
        "Context — what {} ({} dialect) receives from {} v{}",
        view.model_id,
        view.provider.label(),
        server.name,
        server.version
    );
    s.push_str(&pad_right(&left, "[nothing is sent to any API]"));
    s.push('\n');

    // System prompt.
    s.push('\n');
    s.push_str(&labeled("  system prompt", &tok(view.system_tokens)));
    for line in wrap_preview(view.system_prompt, INSTRUCTIONS_PREVIEW_LINES) {
        s.push_str(&format!("    {line}\n"));
    }

    // Server instructions (shown, but not part of the bench request body).
    if let Some(instr) = &view.instructions {
        s.push('\n');
        s.push_str(&labeled("  server instructions", &tok(instr.tokens)));
        s.push_str("    (offered by the server; not sent by `jig bench` — see footer)\n");
        for line in wrap_preview(&instr.text, INSTRUCTIONS_PREVIEW_LINES) {
            s.push_str(&format!("    {line}\n"));
        }
    }

    // Tools, largest first, drawn as a tree.
    s.push('\n');
    let tools_label = format!("  tools ({})", view.tools.len());
    s.push_str(&labeled(&tools_label, &tok(view.tools_tokens)));
    for (i, t) in view.tools.iter().enumerate() {
        let last = i + 1 == view.tools.len();
        let branch = if last { "└─" } else { "├─" };
        let cont = if last { "  " } else { "│ " };
        let header = format!("  {branch} {}", t.name);
        s.push_str(&pad_right(&header, &tok(t.tokens)));
        s.push('\n');
        if let Some(desc) = &t.description {
            s.push_str(&format!("  {cont}   \"{}\"\n", one_line(desc)));
        }
        for line in &t.schema_lines {
            s.push_str(&format!("  {cont}   {line}\n"));
        }
    }

    // Grand total — token count aligned to the section column, provenance after.
    s.push('\n');
    let total_note = format!(
        "  ({}, {})",
        view.model_id,
        view.exactness.tag().trim_start_matches('~')
    );
    let total_label = "  TOTAL context before the user's first word";
    s.push_str(&pad_right(total_label, &tok(view.total_tokens)));
    s.push_str(&total_note);
    s.push('\n');

    // Footer: the honesty contract.
    s.push('\n');
    s.push_str(&footer(view));
    s
}

/// The honesty footer (vault Q5): name the rendering, disclaim universality,
/// and state that nothing is sent and no key is used.
fn footer(view: &ContextView) -> String {
    let mut f = String::new();
    f.push_str(&format!(
        "This is the {} API rendering — what `jig bench` sends. Chat clients (Claude Desktop,\n\
         Cursor, …) may render tool context differently; per-client renderings are a future\n\
         milestone.\n",
        view.provider.label()
    ));
    f.push_str(
        "No API key is needed or used: context computes this locally and sends nothing to any\n\
         provider.\n",
    );
    if view.instructions.is_some() {
        f.push_str(
            "Server instructions are shown for reference but are not in the request body: `jig\n\
             bench` sends only the system prompt + tools.\n",
        );
    }
    if !view.exactness.is_exact() {
        if let Some(m) = view.exactness.method() {
            f.push_str(&format!("~approx — {m}\n"));
        }
    }
    f
}

/// A left label with a right-aligned token annotation, terminated by a newline
/// (used for section headers).
fn labeled(label: &str, annotation: &str) -> String {
    let mut line = pad_right(label, annotation);
    line.push('\n');
    line
}

/// Right-align `right` against `left` so `right` ends near [`TOK_COL`], with at
/// least two spaces of separation. Never truncates `left`.
fn pad_right(left: &str, right: &str) -> String {
    let left_w = left.chars().count();
    let right_w = right.chars().count();
    let target = TOK_COL.saturating_sub(right_w);
    let pad = if target > left_w { target - left_w } else { 2 };
    format!("{left}{}{right}", " ".repeat(pad))
}

/// Format a token count as `1,094 tok`.
fn tok(n: usize) -> String {
    format!("{} tok", commas(n))
}

/// Insert thousands separators: `1234` -> `1,234`.
fn commas(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let first = bytes.len() % 3;
    for (i, b) in bytes.iter().enumerate() {
        if i != 0 && i >= first && (i - first).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Flatten whitespace in `s` to single spaces for a one-line preview.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Preview the first `max` lines of `text`, appending `… +N lines` when longer.
/// Blank lines are dropped from the preview so it stays compact.
fn wrap_preview(text: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= max {
        return lines.into_iter().map(str::to_string).collect();
    }
    let mut out: Vec<String> = lines.iter().take(max).map(|l| l.to_string()).collect();
    out.push(format!("… +{} lines", lines.len() - max));
    out
}

// ---------------------------------------------------------------------------
// Raw rendering
// ---------------------------------------------------------------------------

/// Render the full JSON request body, pretty-printed, exactly as the API would
/// receive it (minus auth, which rides in a header).
pub(crate) fn render_raw(view: &ContextView) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&view.body).unwrap_or_else(|_| "{}".to_string())
    )
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

/// Render machine output: the raw body + per-section token annotations +
/// provenance (model, tokenizer, exactness, dialect).
pub(crate) fn render_json(server: &Implementation, view: &ContextView) -> String {
    let tools: Vec<Value> = view
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "tokens": t.tokens,
                "schema": t.schema_lines,
            })
        })
        .collect();

    let instructions = view.instructions.as_ref().map(|i| {
        json!({
            "text": i.text,
            "tokens": i.tokens,
            "sentByBench": i.sent_by_bench,
        })
    });

    let doc = json!({
        "serverInfo": server,
        "provenance": {
            "model": view.model_id,
            "apiModel": view.api_model,
            "dialect": view.provider.label(),
            "tokenizer": view.tokenizer,
            "exactness": exactness_json(view),
        },
        "taskPlaceholder": jig_core::context::CONTEXT_TASK_PLACEHOLDER,
        "sections": {
            "systemPrompt": {
                "text": view.system_prompt,
                "tokens": view.system_tokens,
                "sentByBench": true,
            },
            "serverInstructions": instructions,
            "tools": {
                "count": view.tools.len(),
                "tokens": view.tools_tokens,
                "items": tools,
            },
        },
        "totalTokens": view.total_tokens,
        "requestBody": view.body,
        "note": "No API key is used; nothing is sent to any provider. This is the bench request \
                 body (system prompt + tools + placeholder task). Server instructions are shown \
                 but not sent by `jig bench`.",
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

fn exactness_json(view: &ContextView) -> Value {
    match view.exactness.method() {
        Some(method) => json!({ "exact": false, "method": method }),
        None => json!({ "exact": true }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::Tool;

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

    const MOCK_INSTRUCTIONS: &str = "A toy MCP server for exercising Jig.";

    fn mock_server() -> Implementation {
        Implementation {
            name: "jig-mock-server".to_string(),
            version: "0.1.0".to_string(),
            title: None,
        }
    }

    fn view(provider: Provider, model: &str, api_model: &str) -> ContextView {
        context::build(
            provider,
            model,
            api_model,
            &mock_tools(),
            Some(MOCK_INSTRUCTIONS),
        )
        .unwrap()
    }

    #[test]
    fn human_openai_snapshot() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        insta::assert_snapshot!("context_human_openai", render_human(&mock_server(), &v));
    }

    #[test]
    fn human_anthropic_snapshot() {
        let v = view(Provider::Anthropic, "claude-sonnet", "claude-sonnet-5");
        insta::assert_snapshot!("context_human_anthropic", render_human(&mock_server(), &v));
    }

    #[test]
    fn raw_snapshot() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        insta::assert_snapshot!("context_raw_openai", render_raw(&v));
    }

    #[test]
    fn json_snapshot() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        insta::assert_snapshot!("context_json_openai", render_json(&mock_server(), &v));
    }

    #[test]
    fn human_view_carries_honesty_footer_and_no_key_note() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        let out = render_human(&mock_server(), &v);
        assert!(out.contains("[nothing is sent to any API]"));
        assert!(out.contains("what `jig bench` sends"));
        assert!(out.contains("per-client renderings are a future"));
        assert!(out.contains("No API key is needed or used"));
        // Largest tool first: make_reservation outweighs echo/always_fails.
        let mr = out.find("make_reservation").unwrap();
        let af = out.find("always_fails").unwrap();
        assert!(mr < af, "tools must be largest-first");
    }

    #[test]
    fn raw_body_has_no_key_and_is_the_request_body() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        let raw = render_raw(&v);
        assert!(raw.contains("\"tool_choice\""));
        assert!(raw.contains("<your task here>"));
        // OpenAI dialect wraps tools as {type: function, ...}.
        assert!(raw.contains("\"type\": \"function\""));
    }
}
