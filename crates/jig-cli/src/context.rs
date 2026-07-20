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

/// The `--client` value that prints the catalog instead of connecting.
const CLIENT_LIST_SENTINEL: &str = "list";

/// Whether `--client` asked for the catalog rather than a rendering.
pub fn is_client_list_request(client: &str) -> bool {
    client.trim().eq_ignore_ascii_case(CLIENT_LIST_SENTINEL)
}

/// Run `jig context --client list`: print every client Jig knows about, its
/// evidence level, and the citation — including the ones deliberately **not**
/// implemented, so the gaps are as visible as the coverage.
pub fn run_client_list(as_json: bool) -> ExitCode {
    if as_json {
        emit(&render_client_list_json());
    } else {
        emit(&render_client_list_human());
    }
    ExitCode::SUCCESS
}

/// Run `jig context`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    model: Option<String>,
    api_model: Option<String>,
    provider_override: Option<Provider>,
    client: String,
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

    // Reject an unrenderable client *before* spawning a server: a refusal is
    // about jig's evidence, not about the target, so making the user wait for a
    // connection first would be pure noise.
    if jig_core::clients::spec(&client).is_none() {
        return Err(format!(
            "unknown client '{}' (known: {}); `jig context --client list` describes each one",
            client,
            jig_core::known_clients().join(", ")
        ));
    }

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        provider,
        &resolved.id,
        &resolved.api_model,
        &client,
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
    client_id: &str,
    raw: bool,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let conn = target.connect(tap, timeout_secs, max_message_bytes).await?;
    let tools = conn.list_tools().await.map_err(|e| e.to_string())?;
    let instructions = conn.instructions().map(|s| s.to_string());
    let server = conn.server_info().clone();
    conn.shutdown().await.map_err(|e| e.to_string())?;

    // The prefix schemes key off the server's own name, so the rendering is
    // derived from the live handshake rather than from a guess about how the
    // user happened to register the server in their client config.
    let view = context::build_for_client(
        provider,
        model_id,
        api_model,
        &tools,
        instructions.as_deref(),
        client_id,
        &server.name,
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

    // Under a client rendering, say so immediately and quantify the difference:
    // the reader must never mistake a variant for the raw API request.
    if let Some(v) = &view.client {
        s.push_str(&format!(
            "  rendered as {} ({}) — {}\n",
            v.rendering.spec.label,
            v.rendering.spec.evidence.tag(),
            v.delta_summary()
        ));
    }

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
    match &view.client {
        None => {
            f.push_str(&format!(
                "This is the {} API rendering — what `jig bench` sends. Chat clients reshape the\n\
                 tool surface before it reaches the model; `--client <name>` renders the ones jig\n\
                 can cite, and `--client list` shows what is verified, approximated or unknown.\n",
                view.provider.label()
            ));
        }
        Some(v) => {
            f.push_str(&format!(
                "Rendered as {} over the {} dialect, {}.\n",
                v.rendering.spec.label,
                view.provider.label(),
                v.delta_summary()
            ));
            f.push_str(&labeled_block("", "Source: ", v.rendering.spec.citation));
            if let Some(caveat) = v.rendering.caveat() {
                // The delta only covers what the citation establishes, so it is
                // a floor. Saying otherwise would overclaim.
                f.push_str(&labeled_block(
                    "",
                    "Not established: ",
                    &format!(
                        "{caveat}. The token delta therefore covers only the transformation \
                         above — treat it as a lower bound, not this client's full cost."
                    ),
                ));
            }
        }
    }
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

    // The client rendering, when one was applied: what it did, on whose
    // authority, what it does not cover, and the signed delta.
    let client = view.client.as_ref().map(|v| {
        let spec = v.rendering.spec;
        json!({
            "id": spec.id,
            "label": spec.label,
            "evidence": spec.evidence.tag(),
            "citation": spec.citation,
            "notEstablished": spec.unestablished,
            "serverName": v.rendering.server_name,
            "apiTotalTokens": v.api_total_tokens,
            "deltaTokens": v.delta_tokens,
            "deltaIsLowerBound": spec.unestablished.is_some(),
            "toolNames": v.rendering.names.iter().map(|n| json!({
                "mcp": n.mcp,
                "rendered": n.rendered,
            })).collect::<Vec<_>>(),
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
            // Absent means the raw provider API request — today's default.
            "clientRendering": client,
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

// ---------------------------------------------------------------------------
// `--client list` — the catalog, gaps included
// ---------------------------------------------------------------------------

/// Render the client catalog for a person.
///
/// Deliberately lists the **unknown** clients alongside the implemented ones.
/// A catalog that showed only what Jig supports would read as if the rest had
/// simply not been considered; showing the refusals, with what was checked,
/// makes the boundary of the evidence part of the product.
pub(crate) fn render_client_list_human() -> String {
    let mut s = String::new();
    s.push_str("Client renderings — how each client reshapes the tool surface before the model\n");
    s.push_str("sees it. Every implemented variant is derived from a citable public source; a\n");
    s.push_str("client whose rendering no source establishes is listed as unknown and refused,\n");
    s.push_str("never guessed.\n\n");

    for spec in jig_core::CLIENTS {
        let mark = match spec.evidence {
            jig_core::Evidence::Verified => '✓',
            jig_core::Evidence::Approximated => '~',
            jig_core::Evidence::Unknown => '·',
        };
        s.push_str(&format!(
            "  {mark} {:<15} {:<13} {}\n",
            spec.id,
            spec.evidence.tag(),
            spec.label
        ));
        s.push_str(&labeled_block("      ", "", spec.summary));
        s.push_str(&labeled_block("      ", "source: ", spec.citation));
        if let Some(gap) = spec.unestablished {
            s.push_str(&labeled_block("      ", "not established: ", gap));
        }
        s.push('\n');
    }

    s.push_str("`--client api` (the default) is the raw provider request `jig bench` sends.\n");
    s.push_str(
        "For any other client the reported delta covers only the cited transformation, so it\n\
         is a lower bound on that client's true context cost.\n",
    );
    s
}

/// Render the client catalog as machine output.
pub(crate) fn render_client_list_json() -> String {
    let clients: Vec<Value> = jig_core::CLIENTS
        .iter()
        .map(|spec| {
            json!({
                "id": spec.id,
                "label": spec.label,
                "evidence": spec.evidence.tag(),
                "implemented": spec.evidence.is_implemented(),
                "citation": spec.citation,
                "summary": spec.summary,
                "notEstablished": spec.unestablished,
            })
        })
        .collect();
    let doc = json!({
        "default": jig_core::DEFAULT_CLIENT,
        "clients": clients,
        "note": "Every implemented rendering is derived from a citable public source. A client \
                 listed as `unknown` has no such source and is refused rather than guessed. For \
                 any non-`api` client the reported token delta covers only the cited \
                 transformation and is a lower bound on that client's true context cost.",
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Width at which citation blocks soft-wrap, including their indent and label.
const CITATION_WIDTH: usize = 92;

/// Render `text` as an indented, soft-wrapped block: `indent + label` on the
/// first line, and continuation lines aligned under the text (not under the
/// label), so a long citation reads as one paragraph rather than repeating its
/// prefix on every line.
fn labeled_block(indent: &str, label: &str, text: &str) -> String {
    let hang = " ".repeat(label.chars().count());
    let width = CITATION_WIDTH
        .saturating_sub(indent.chars().count())
        .saturating_sub(label.chars().count())
        .max(20);
    let mut out = String::new();
    for (i, line) in wrap_at(text, width).into_iter().enumerate() {
        if i == 0 {
            out.push_str(&format!("{indent}{label}{line}\n"));
        } else {
            out.push_str(&format!("{indent}{hang}{line}\n"));
        }
    }
    out
}

/// Soft-wrap `text` at `width` on whitespace, for the citation blocks.
fn wrap_at(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
        // The old text promised per-client renderings as a future milestone.
        // They shipped, so the note now points at the flag that delivers them.
        assert!(out.contains("`--client <name>` renders the ones jig"));
        assert!(out.contains("--client list"));
        assert!(out.contains("No API key is needed or used"));
        // Largest tool first: make_reservation outweighs echo/always_fails.
        let mr = out.find("make_reservation").unwrap();
        let af = out.find("always_fails").unwrap();
        assert!(mr < af, "tools must be largest-first");
    }

    /// A view rendered as a specific client.
    fn client_view(client: &str) -> ContextView {
        context::build_for_client(
            Provider::Anthropic,
            "claude-sonnet",
            "claude-sonnet-5",
            &mock_tools(),
            Some(MOCK_INSTRUCTIONS),
            client,
            "jig-mock-server",
        )
        .unwrap()
    }

    #[test]
    fn client_list_human_snapshot() {
        insta::assert_snapshot!("context_client_list_human", render_client_list_human());
    }

    #[test]
    fn client_list_json_snapshot() {
        insta::assert_snapshot!("context_client_list_json", render_client_list_json());
    }

    #[test]
    fn human_claude_code_snapshot() {
        insta::assert_snapshot!(
            "context_human_claude_code",
            render_human(&mock_server(), &client_view("claude-code"))
        );
    }

    #[test]
    fn client_list_shows_gaps_as_prominently_as_coverage() {
        let out = render_client_list_human();
        // The implemented ones.
        for id in ["api", "claude-code", "vscode", "openai-agents"] {
            assert!(out.contains(id), "missing client {id}");
        }
        // ...and the refusals, with what was checked, not silence.
        assert!(out.contains("claude-desktop"));
        assert!(out.contains("cursor"));
        assert!(out.contains("unknown"));
        assert!(
            out.contains("forum threads"),
            "the reason cursor is unknown must be visible"
        );
        assert!(out.contains("never guessed"));
    }

    #[test]
    fn a_client_view_names_the_client_and_quantifies_the_delta() {
        let out = render_human(&mock_server(), &client_view("claude-code"));
        // Named up front, with its evidence level.
        assert!(out.contains("rendered as Claude Code (approximated)"));
        assert!(out.contains("vs the raw API request"));
        // The transformed names are what the model actually sees.
        assert!(out.contains("mcp__jig-mock-server__echo"));
        // The citation and the limit of the claim both appear.
        assert!(out.contains("code.claude.com/docs/en/hooks"));
        assert!(out.contains("lower bound"));
        // The default's "future milestone" wording is gone for good.
        assert!(!out.contains("future milestone"));
    }

    #[test]
    fn the_default_view_points_at_the_shipped_flag_not_a_future_milestone() {
        let out = render_human(&mock_server(), &view(Provider::OpenAI, "gpt-4o", "gpt-4o"));
        assert!(out.contains("--client list"));
        assert!(!out.contains("future milestone"));
    }

    #[test]
    fn client_json_carries_citation_delta_and_the_lower_bound_flag() {
        let v = client_view("vscode");
        let doc: Value = serde_json::from_str(&render_json(&mock_server(), &v)).unwrap();
        let c = &doc["provenance"]["clientRendering"];
        assert_eq!(c["id"], "vscode");
        assert_eq!(c["evidence"], "verified");
        assert!(c["citation"].as_str().unwrap().contains("mcpTypes.ts"));
        assert_eq!(c["deltaIsLowerBound"], true);
        assert!(c["deltaTokens"].is_i64());
        // Name mapping is explicit, both sides shown.
        assert_eq!(c["toolNames"][0]["mcp"], "echo");
        assert_eq!(
            c["toolNames"][0]["rendered"], "mcp_jig-mock-serv_echo",
            "the server segment is cut so `mcp_<server>_` fits 18 chars"
        );
    }

    #[test]
    fn the_default_json_view_has_no_client_rendering() {
        let v = view(Provider::OpenAI, "gpt-4o", "gpt-4o");
        let doc: Value = serde_json::from_str(&render_json(&mock_server(), &v)).unwrap();
        assert!(doc["provenance"]["clientRendering"].is_null());
    }

    #[test]
    fn list_sentinel_is_recognized_case_insensitively() {
        for s in ["list", "LIST", " List "] {
            assert!(is_client_list_request(s));
        }
        for s in ["api", "vscode", ""] {
            assert!(!is_client_list_request(s));
        }
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
