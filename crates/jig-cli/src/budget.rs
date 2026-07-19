//! `jig budget` — the token-budget command.
//!
//! Connects to a server, reads its tools + instructions, prices the tool
//! surface against one or more models, and renders the result as a table, a
//! shareable markdown card, or full JSON. All output is deterministic (stable
//! sort, ties by name) so it can be diffed in CI.

use std::path::Path;
use std::process::ExitCode;

use jig_core::tokens::{self, Exactness, ModelBudget};
use jig_core::{Implementation, ProtocolTap, Tool};
use serde_json::{json, Value};

use crate::{emit, warn_non_protocol_output, write_tap_if_requested, Target};

/// Default models when the user passes no `--model`: one exact (OpenAI) column
/// and one labelled-approximate (Anthropic) column — the headline "what does
/// this cost in Claude context" answer, honestly labelled.
const DEFAULT_MODELS: &[&str] = &["gpt-4o", "claude-sonnet"];

/// Run `jig budget`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    models: Vec<String>,
    as_json: bool,
    as_markdown: bool,
    tap_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
    exact_anthropic: bool,
) -> Result<ExitCode, String> {
    let models: Vec<String> = if models.is_empty() {
        DEFAULT_MODELS.iter().map(|s| s.to_string()).collect()
    } else {
        models
    };

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        &models,
        as_json,
        as_markdown,
        timeout_secs,
        max_message_bytes,
        exact_anthropic,
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
    models: &[String],
    as_json: bool,
    as_markdown: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
    exact_anthropic: bool,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;

    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let instructions = client.instructions().map(|s| s.to_string());
    let server = client.server_info().clone();

    // Build one budget per model.
    let mut budgets = Vec::with_capacity(models.len());
    for model in models {
        let mut mb = tokens::budget_local(model, &tools, instructions.as_deref())
            .map_err(|e| e.to_string())?;
        if exact_anthropic && tokens::is_anthropic_model(model) {
            apply_exact_anthropic(&mut mb, model, &tools, instructions.as_deref()).await;
        }
        budgets.push(mb);
    }

    client.shutdown().await.map_err(|e| e.to_string())?;

    let out = if as_json {
        render_json(&server, &tools, &budgets)
    } else if as_markdown {
        render_markdown(&server, &budgets)
    } else {
        render_table(&server, &budgets)
    };
    emit(&out);

    Ok(ExitCode::SUCCESS)
}

/// Attempt to upgrade an Anthropic model's grand total to an exact figure via
/// the official endpoint. Any failure degrades to the labelled approximation
/// with a warning on stderr — never a crash, never echoing the key.
async fn apply_exact_anthropic(
    mb: &mut ModelBudget,
    model: &str,
    tools: &[Tool],
    instructions: Option<&str>,
) {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "jig: warning: --exact-anthropic requested but ANTHROPIC_API_KEY is not set; \
                 using the labelled approximation for {model}"
            );
            return;
        }
    };
    match tokens::anthropic_exact_total(model, tools, instructions, &key).await {
        Ok(total) => {
            mb.total = total;
            mb.total_exactness = Exactness::Exact;
        }
        Err(e) => {
            eprintln!(
                "jig: warning: --exact-anthropic failed for {model} ({e}); \
                 using the labelled approximation"
            );
        }
    }
}

/// Insert thousands separators: `12345` -> `12,345`.
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

/// A stable display order for tools: descending by the primary model's token
/// count, ties broken by name ascending. Deterministic for CI diffing.
fn tool_order(primary: &ModelBudget) -> Vec<String> {
    let mut idx: Vec<(&str, usize)> = primary
        .tools
        .iter()
        .map(|t| (t.name.as_str(), t.tokens))
        .collect();
    idx.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    idx.into_iter().map(|(n, _)| n.to_string()).collect()
}

/// Look up a tool's token count in a model budget by name.
fn tokens_for<'a>(mb: &'a ModelBudget, name: &str) -> Option<&'a jig_core::ToolBudget> {
    mb.tools.iter().find(|t| t.name == name)
}

/// A display label for a tool row: `name` primary, `title` secondary.
fn tool_label(mb: &ModelBudget, name: &str) -> String {
    match tokens_for(mb, name).and_then(|t| t.title.as_deref()) {
        Some(title) => format!("{name} — \"{title}\""),
        None => name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

/// Render the default terminal table.
fn render_table(server: &Implementation, budgets: &[ModelBudget]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Server: {} v{}\n\n", server.name, server.version));

    for mb in budgets {
        s.push_str(&format!("Model:  {}\n", mb.header));
        if !mb.total_exactness.is_exact() {
            if let Some(m) = mb.per_tool_exactness.method() {
                s.push_str(&format!("        note: {m}\n"));
            }
        } else if !mb.per_tool_exactness.is_exact() {
            // Exact total but approximate per-tool rows (the --exact-anthropic
            // case): the endpoint reports only a request-level total.
            s.push_str(
                "        note: total is exact (count_tokens API); per-tool rows are the \
                 ~approx o200k proxy — the endpoint reports no per-tool breakdown\n",
            );
        }
    }
    s.push('\n');

    let primary = &budgets[0];
    let order = tool_order(primary);
    let single = budgets.len() == 1;

    // Header row.
    let mut headers = vec!["Tool".to_string()];
    for mb in budgets {
        headers.push(mb.model_id.clone());
    }
    if single {
        headers.push("% of total".to_string());
    }

    // Build the rows.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for name in &order {
        let mut row = vec![tool_label(primary, name)];
        for mb in budgets {
            let t = tokens_for(mb, name).map(|t| t.tokens).unwrap_or(0);
            row.push(format!("{} {}", commas(t), tag_for(&mb.per_tool_exactness)));
        }
        if single {
            let t = tokens_for(primary, name).map(|t| t.tokens).unwrap_or(0);
            row.push(percent(t, primary.total));
        }
        rows.push(row);
    }

    // Instructions row.
    if primary.instructions_tokens.is_some() {
        let mut row = vec!["(server instructions)".to_string()];
        for mb in budgets {
            let t = mb.instructions_tokens.unwrap_or(0);
            row.push(format!("{} {}", commas(t), tag_for(&mb.per_tool_exactness)));
        }
        if single {
            let t = primary.instructions_tokens.unwrap_or(0);
            row.push(percent(t, primary.total));
        }
        rows.push(row);
    }

    // Total row.
    let mut total_row = vec!["TOTAL".to_string()];
    for mb in budgets {
        total_row.push(format!(
            "{} {}",
            commas(mb.total),
            tag_for(&mb.total_exactness)
        ));
    }
    if single {
        total_row.push("100%".to_string());
    }

    s.push_str(&render_aligned(&headers, &rows, &total_row));
    s
}

/// The exactness tag (`exact` / `~approx`) for a table cell.
fn tag_for(e: &Exactness) -> &'static str {
    e.tag()
}

fn percent(part: usize, total: usize) -> String {
    if total == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", (part as f64 / total as f64) * 100.0)
    }
}

/// Render a left-aligned first column and right-aligned numeric columns, with a
/// separator before the total row.
fn render_aligned(headers: &[String], rows: &[Vec<String>], total: &[String]) -> String {
    let ncols = headers.len();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = widths[i].max(h.chars().count());
    }
    for row in rows.iter().map(Vec::as_slice).chain(std::iter::once(total)) {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }

    let fmt_row = |row: &[String]| -> String {
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            if i == 0 {
                line.push_str(&format!("{:<width$}", c, width = widths[i]));
            } else {
                line.push_str(&format!("  {:>width$}", c, width = widths[i]));
            }
        }
        line.push('\n');
        line
    };

    let mut s = String::new();
    s.push_str(&fmt_row(headers));
    // underline
    let total_width: usize = widths.iter().sum::<usize>() + 2 * (ncols - 1);
    s.push_str(&"-".repeat(total_width));
    s.push('\n');
    for row in rows {
        s.push_str(&fmt_row(row));
    }
    s.push_str(&"-".repeat(total_width));
    s.push('\n');
    s.push_str(&fmt_row(total));
    s
}

// ---------------------------------------------------------------------------
// Markdown card (the shareable growth artifact)
// ---------------------------------------------------------------------------

/// Render a clean GitHub-flavored markdown card designed to be pasted into a
/// PR or tweet.
fn render_markdown(server: &Implementation, budgets: &[ModelBudget]) -> String {
    let primary = &budgets[0];
    let order = tool_order(primary);
    let n = primary.tools.len();

    let mut s = String::new();
    s.push_str(&format!(
        "## 🧰 MCP token budget — {} v{}\n\n",
        server.name, server.version
    ));

    // Headline line: total(s) + tool count + model labels.
    let totals: Vec<String> = budgets
        .iter()
        .map(|mb| {
            format!(
                "**{}** tokens on `{}` ({})",
                commas(mb.total),
                mb.model_id,
                mb.total_exactness.tag()
            )
        })
        .collect();
    s.push_str(&format!(
        "{} across **{}** tool{}.\n\n",
        totals.join(" · "),
        n,
        if n == 1 { "" } else { "s" }
    ));

    // Table header.
    s.push_str("| Tool |");
    for mb in budgets {
        s.push_str(&format!(" {} |", mb.model_id));
    }
    if budgets.len() == 1 {
        s.push_str(" % |");
    }
    s.push('\n');
    s.push_str("|:-----|");
    for _ in budgets {
        s.push_str("-----:|");
    }
    if budgets.len() == 1 {
        s.push_str("---:|");
    }
    s.push('\n');

    // Rows.
    for name in &order {
        let label = tool_label(primary, name);
        s.push_str(&format!("| `{}` |", md_escape(&label)));
        for mb in budgets {
            let t = tokens_for(mb, name).map(|t| t.tokens).unwrap_or(0);
            s.push_str(&format!(" {} |", commas(t)));
        }
        if budgets.len() == 1 {
            let t = tokens_for(primary, name).map(|t| t.tokens).unwrap_or(0);
            s.push_str(&format!(" {} |", percent(t, primary.total)));
        }
        s.push('\n');
    }

    // Instructions row.
    if primary.instructions_tokens.is_some() {
        s.push_str("| _server instructions_ |");
        for mb in budgets {
            s.push_str(&format!(
                " {} |",
                commas(mb.instructions_tokens.unwrap_or(0))
            ));
        }
        if budgets.len() == 1 {
            let t = primary.instructions_tokens.unwrap_or(0);
            s.push_str(&format!(" {} |", percent(t, primary.total)));
        }
        s.push('\n');
    }

    // Total row.
    s.push_str("| **Total** |");
    for mb in budgets {
        s.push_str(&format!(" **{}** |", commas(mb.total)));
    }
    if budgets.len() == 1 {
        s.push_str(" **100%** |");
    }
    s.push('\n');

    // Exactness footnote for approximate columns.
    let approx: Vec<&ModelBudget> = budgets
        .iter()
        .filter(|mb| !mb.total_exactness.is_exact())
        .collect();
    if !approx.is_empty() {
        let ids: Vec<&str> = approx.iter().map(|mb| mb.model_id.as_str()).collect();
        s.push_str(&format!(
            "\n> `~approx` — {} counted with the o200k_base tokenizer as a proxy (Claude's \
             tokenizer is not public). Run with `--exact-anthropic` for the exact total.\n",
            ids.join(", ")
        ));
    }

    s.push_str("\n_Measured with Jig — github.com/Shodh-Labs/jig_\n");
    s
}

/// Escape backticks in a tool label so it renders inside an inline-code span.
fn md_escape(s: &str) -> String {
    s.replace('`', "'")
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Render full machine-readable JSON, including exactness metadata and the
/// canonical rendering that was counted.
fn render_json(server: &Implementation, tools: &[Tool], budgets: &[ModelBudget]) -> String {
    let models: Vec<Value> = budgets
        .iter()
        .map(|mb| {
            let order = tool_order(mb);
            let tool_rows: Vec<Value> = order
                .iter()
                .map(|name| {
                    let tb = tokens_for(mb, name).unwrap();
                    json!({
                        "name": tb.name,
                        "title": tb.title,
                        "tokens": tb.tokens,
                        "canonical": tb.canonical,
                    })
                })
                .collect();
            json!({
                "model": mb.model_id,
                "header": mb.header,
                "perToolExactness": exactness_json(&mb.per_tool_exactness),
                "totalExactness": exactness_json(&mb.total_exactness),
                "tools": tool_rows,
                "instructionsTokens": mb.instructions_tokens,
                "total": mb.total,
            })
        })
        .collect();

    let doc = json!({
        "serverInfo": server,
        "toolCount": tools.len(),
        "canonicalRendering": tokens::CANONICAL_RENDERING_DOC,
        "models": models,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

fn exactness_json(e: &Exactness) -> Value {
    match e {
        Exactness::Exact => json!({ "exact": true }),
        Exactness::Approximate { method } => json!({ "exact": false, "method": method }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::tokens::ToolBudget;

    fn sample_budget(model: &str, header: &str, exact: bool) -> ModelBudget {
        let ex = if exact {
            Exactness::Exact
        } else {
            Exactness::Approximate {
                method: "o200k proxy".to_string(),
            }
        };
        ModelBudget {
            model_id: model.to_string(),
            header: header.to_string(),
            per_tool_exactness: ex.clone(),
            total_exactness: ex,
            tools: vec![
                ToolBudget {
                    name: "zebra".to_string(),
                    title: None,
                    tokens: 50,
                    canonical: "{}".to_string(),
                },
                ToolBudget {
                    name: "alpha".to_string(),
                    title: Some("Alpha Tool".to_string()),
                    tokens: 100,
                    canonical: "{}".to_string(),
                },
                ToolBudget {
                    name: "middle".to_string(),
                    title: None,
                    tokens: 50,
                    canonical: "{}".to_string(),
                },
            ],
            instructions_tokens: Some(20),
            total: 220,
        }
    }

    fn server() -> Implementation {
        Implementation {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            title: None,
        }
    }

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(42), "42");
        assert_eq!(commas(1234), "1,234");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn tool_order_is_desc_tokens_then_name() {
        let mb = sample_budget("gpt-4o", "gpt-4o (o200k_base, exact)", true);
        // alpha=100, then the 50-token tools tie and break by name: middle, zebra.
        assert_eq!(tool_order(&mb), vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn markdown_snapshot_is_stable() {
        let budgets = vec![sample_budget("gpt-4o", "gpt-4o (o200k_base, exact)", true)];
        let out = render_markdown(&server(), &budgets);
        let expected = "\
## 🧰 MCP token budget — demo v1.0.0

**220** tokens on `gpt-4o` (exact) across **3** tools.

| Tool | gpt-4o | % |
|:-----|-----:|---:|
| `alpha — \"Alpha Tool\"` | 100 | 45.5% |
| `middle` | 50 | 22.7% |
| `zebra` | 50 | 22.7% |
| _server instructions_ | 20 | 9.1% |
| **Total** | **220** | **100%** |

_Measured with Jig — github.com/Shodh-Labs/jig_
";
        assert_eq!(out, expected);
    }

    #[test]
    fn markdown_approx_has_footnote() {
        let budgets = vec![sample_budget(
            "claude-sonnet",
            "claude-sonnet (~approx via o200k_base)",
            false,
        )];
        let out = render_markdown(&server(), &budgets);
        assert!(out.contains("`~approx`"), "expected footnote: {out}");
        assert!(out.contains("Measured with Jig — github.com/Shodh-Labs/jig"));
    }

    #[test]
    fn table_is_deterministic_and_has_total() {
        let budgets = vec![sample_budget("gpt-4o", "gpt-4o (o200k_base, exact)", true)];
        let a = render_table(&server(), &budgets);
        let b = render_table(&server(), &budgets);
        assert_eq!(a, b);
        assert!(a.contains("TOTAL"));
        // alpha (highest) appears before zebra (lowest, alphabetically last).
        let ai = a.find("alpha").unwrap();
        let zi = a.find("zebra").unwrap();
        assert!(ai < zi);
    }

    #[test]
    fn json_includes_exactness_and_canonical() {
        let budgets = vec![sample_budget("gpt-4o", "gpt-4o (o200k_base, exact)", true)];
        let out = render_json(&server(), &[], &budgets);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["canonicalRendering"],
            json!(tokens::CANONICAL_RENDERING_DOC)
        );
        assert_eq!(v["models"][0]["totalExactness"]["exact"], json!(true));
        assert!(v["models"][0]["tools"][0]["canonical"].is_string());
    }

    // ---- Snapshots over real mock-server fixture data -----------------------

    /// The jig-mock-server tool surface as fixture data, kept in sync with the
    /// mock's `handle_tools_list`. Priced with the exact OpenAI tokenizer, the
    /// resulting tables/cards are fully deterministic and safe to snapshot.
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

    fn mock_server_info() -> Implementation {
        Implementation {
            name: "jig-mock-server".to_string(),
            version: "0.1.0".to_string(),
            title: None,
        }
    }

    /// Budgets over the mock tools for one exact (gpt-4o) column.
    fn mock_budgets_single() -> Vec<ModelBudget> {
        vec![tokens::budget_local("gpt-4o", &mock_tools(), Some(MOCK_INSTRUCTIONS)).unwrap()]
    }

    /// Budgets over the mock tools for an exact + labelled-approximate pair.
    fn mock_budgets_multi() -> Vec<ModelBudget> {
        vec![
            tokens::budget_local("gpt-4o", &mock_tools(), Some(MOCK_INSTRUCTIONS)).unwrap(),
            tokens::budget_local("claude-sonnet", &mock_tools(), Some(MOCK_INSTRUCTIONS)).unwrap(),
        ]
    }

    #[test]
    fn budget_table_snapshot() {
        insta::assert_snapshot!(
            "budget_table",
            render_table(&mock_server_info(), &mock_budgets_single())
        );
    }

    #[test]
    fn budget_table_multimodel_snapshot() {
        insta::assert_snapshot!(
            "budget_table_multimodel",
            render_table(&mock_server_info(), &mock_budgets_multi())
        );
    }

    #[test]
    fn budget_markdown_snapshot() {
        insta::assert_snapshot!(
            "budget_markdown",
            render_markdown(&mock_server_info(), &mock_budgets_single())
        );
    }

    #[test]
    fn budget_json_snapshot() {
        insta::assert_snapshot!(
            "budget_json",
            render_json(&mock_server_info(), &mock_tools(), &mock_budgets_single())
        );
    }
}
