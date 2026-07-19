//! `jig bench` — the model-in-the-loop bench command.
//!
//! Connects to a server, lists its tools, then assembles and sends a *real*
//! tool-use request to the chosen provider N times, classifying each response
//! into the outcome taxonomy and reporting the distribution. See
//! [`jig_core::bench`] for the engine; this module owns CLI plumbing, key
//! resolution, and rendering (human table + `--json`), all snapshot-testable
//! because rendering is a pure function of a [`BenchReport`].

use std::path::Path;
use std::process::ExitCode;

use jig_core::bench::{self, ArgCheck, BenchConfig, BenchModel, BenchReport, Outcome, Provider};
use jig_core::ProtocolTap;
use serde_json::{json, Value};

use crate::{client_options, emit, warn_non_protocol_output, write_tap_if_requested, Target};

/// Run `jig bench`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    models: Vec<String>,
    api_model: Option<String>,
    task: String,
    runs: usize,
    temperature: f64,
    as_json: bool,
    save_case: Option<&Path>,
    tap_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    // Default model: claude-sonnet if an Anthropic key is present, else gpt-4o.
    let models: Vec<String> = if models.is_empty() {
        vec![default_model().to_string()]
    } else {
        models
    };

    // Resolve every model and its key up front, BEFORE touching the server, so a
    // missing key fails fast with an actionable message.
    let mut resolved: Vec<(BenchModel, String)> = Vec::with_capacity(models.len());
    for model in &models {
        let mut m = BenchModel::resolve(model).map_err(|e| e.to_string())?;
        if models.len() == 1 {
            if let Some(over) = &api_model {
                m = m.with_api_model(over.clone());
            }
        } else if api_model.is_some() {
            return Err(
                "--api-model applies to a single --model; drop it when benching multiple models"
                    .to_string(),
            );
        }
        let key = require_key(m.provider)?;
        resolved.push((m, key));
    }

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        resolved,
        &task,
        runs,
        temperature,
        as_json,
        save_case,
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
    resolved: Vec<(BenchModel, String)>,
    task: &str,
    runs: usize,
    temperature: f64,
    as_json: bool,
    save_case: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;
    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let server = client.server_info().clone();
    client.shutdown().await.map_err(|e| e.to_string())?;

    if tools.is_empty() {
        return Err("the server exposes no tools — there is nothing to bench".to_string());
    }

    let opts = client_options(timeout_secs, max_message_bytes);
    let mut reports = Vec::with_capacity(resolved.len());
    for (model, key) in resolved {
        let config = BenchConfig {
            model,
            task: task.to_string(),
            runs,
            temperature,
            max_tokens: bench::DEFAULT_MAX_TOKENS,
            timeout: opts.request_timeout,
            base_url: std::env::var("JIG_BENCH_BASE_URL").ok(),
            api_key: key,
        };
        let report = bench::run_bench(&tools, &config)
            .await
            .map_err(|e| e.to_string())?;
        reports.push(report);
    }

    if as_json {
        emit(&render_json(&server, &reports));
    } else {
        emit(&render_human(&server, &reports));
    }

    // `--save-case`: turn this exploration into a regression test by drafting a
    // case into a `.jig` suite file. Uses the first model's report.
    if let Some(path) = save_case {
        let report = reports.first().expect("at least one report");
        match save_drafted_case(path, report, task)? {
            SaveResult::Wrote { id } => {
                eprintln!(
                    "jig: drafted case '{id}' → {} (review the `# TODO` and commit it)",
                    path.display()
                );
            }
            SaveResult::Refused { reason } => {
                eprintln!(
                    "jig: --save-case: not drafting a case — {reason}. Nothing was written to {}.",
                    path.display()
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The outcome of a `--save-case` attempt.
enum SaveResult {
    /// A case was drafted and appended, with this id.
    Wrote {
        /// The generated case id.
        id: String,
    },
    /// No case was drafted (nothing sensible to assert), with the reason.
    Refused {
        /// Why drafting was refused.
        reason: String,
    },
}

/// Draft a case from a bench report and append it to `path`, creating the file
/// and any parent directory if absent.
///
/// Refuses (returns [`SaveResult::Refused`]) when the majority outcome across
/// runs was not a tool selection — there is no tool to assert. Never writes a
/// file that would fail to parse as a `.jig` suite: the assembled content is
/// re-parsed before it is written.
fn save_drafted_case(path: &Path, report: &BenchReport, task: &str) -> Result<SaveResult, String> {
    let dist = report.distribution();
    let best = dist.selected.first();
    let best_count = best.map(|(_, c)| *c).unwrap_or(0);
    let other_max = dist
        .no_tool
        .max(dist.provider_error)
        .max(dist.hallucinated.iter().map(|(_, c)| *c).max().unwrap_or(0));
    let Some((tool, _)) = best else {
        return Ok(SaveResult::Refused {
            reason: format!("the majority outcome was `{}`", dist.takeaway()),
        });
    };
    if best_count < other_max {
        return Ok(SaveResult::Refused {
            reason: "no tool selection was the plurality outcome".to_string(),
        });
    }

    // Arguments come from the first run that selected the majority tool.
    let args = report.results.iter().find_map(|r| match &r.outcome {
        Outcome::Selected {
            tool: t, arguments, ..
        } if t == tool => arguments.as_object().cloned(),
        _ => None,
    });

    // Existing ids (and a parse guard) if the file is already present.
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {} for --save-case: {e}", path.display()))?
    } else {
        String::new()
    };
    let existing_ids: Vec<String> = if existing.trim().is_empty() {
        Vec::new()
    } else {
        let suite = jig_core::eval::load_suite_str(&existing, &path.display().to_string())
            .map_err(|e| {
                format!(
                    "refusing to append: {} is not a valid .jig suite: {e}",
                    path.display()
                )
            })?;
        suite.cases.into_iter().map(|c| c.id).collect()
    };

    let id = unique_id(
        &slugify(task).unwrap_or_else(|| tool.clone()),
        &existing_ids,
    );
    let block = draft_case_block(&id, task, tool, args.as_ref(), report.runs);

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("suite");
    let new_content = if existing.trim().is_empty() {
        format!("# {stem} — drafted by `jig bench --save-case`\ncases:\n{block}")
    } else {
        let mut base = existing;
        if !base.ends_with('\n') {
            base.push('\n');
        }
        format!("{base}{block}")
    };

    // Never write a file that would not parse back.
    jig_core::eval::load_suite_str(&new_content, &path.display().to_string())
        .map_err(|e| format!("internal: drafted case would not parse ({e}) — not writing"))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, new_content)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(SaveResult::Wrote { id })
}

/// Build the YAML text for one drafted case (2-space indented list item), with a
/// review-me comment line.
fn draft_case_block(
    id: &str,
    task: &str,
    tool: &str,
    args: Option<&serde_json::Map<String, Value>>,
    runs: usize,
) -> String {
    let mut s = String::new();
    s.push_str("  # TODO: review drafted case\n");
    s.push_str(&format!("  - id: {id}\n"));
    s.push_str(&format!("    task: {}\n", yaml_scalar(&json!(task))));
    s.push_str("    expect:\n");
    s.push_str(&format!("      tool: {tool}\n"));
    if let Some(map) = args.filter(|m| !m.is_empty()) {
        s.push_str("      args:\n");
        for (k, v) in map {
            s.push_str(&format!("        {k}: {}\n", arg_matcher_yaml(v)));
        }
    }
    s.push_str(&format!("    runs: {runs}\n"));
    s
}

/// Render an argument value as an `exact` matcher in YAML: a bare scalar (the
/// shorthand) for strings/numbers/bools, or `{ exact: <json> }` for structured
/// or null values (which have no bare-scalar shorthand).
fn arg_matcher_yaml(v: &Value) -> String {
    if v.is_string() || v.is_number() || v.is_boolean() {
        yaml_scalar(v)
    } else {
        format!(
            "{{ exact: {} }}",
            serde_json::to_string(v).unwrap_or_default()
        )
    }
}

/// Render a JSON scalar as a YAML scalar. Strings use JSON's double-quoted form
/// (valid YAML), which safely escapes quotes/backslashes/newlines.
fn yaml_scalar(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Turn a task string into a lowercase, hyphenated slug of at most a few words.
/// `None` if nothing usable remains.
fn slugify(task: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in task.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    // Keep it short: at most the first ~6 hyphen-separated words.
    let slug: String = trimmed
        .split('-')
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    (!slug.is_empty()).then_some(slug)
}

/// Ensure `base` is unique among `existing`, appending `-2`, `-3`, … if needed.
fn unique_id(base: &str, existing: &[String]) -> String {
    if !existing.iter().any(|e| e == base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The default model id: `claude-sonnet` when `ANTHROPIC_API_KEY` is set,
/// otherwise `gpt-4o`. Shared with `jig eval`.
pub(crate) fn default_model() -> &'static str {
    if env_present("ANTHROPIC_API_KEY") {
        "claude-sonnet"
    } else {
        "gpt-4o"
    }
}

fn env_present(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Read the provider's key from the environment, or return an actionable error
/// naming the variable. `JIG_BENCH_API_KEY` is honored as a test override so the
/// mock-provider integration tests can supply a dummy key without a real one.
/// Shared with `jig eval`.
pub(crate) fn require_key(provider: Provider) -> Result<String, String> {
    if let Ok(k) = std::env::var("JIG_BENCH_API_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let var = provider.env_var();
    match std::env::var(var) {
        Ok(k) if !k.is_empty() => Ok(k),
        _ => Err(format!(
            "{var} is not set — `jig bench` needs a {} API key for this model. Set it in your \
             environment (it is never logged or written to output).",
            provider.label()
        )),
    }
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Render the human report for one or more model sections.
pub(crate) fn render_human(server: &jig_core::Implementation, reports: &[BenchReport]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Server: {} v{}\n", server.name, server.version));
    if let Some(first) = reports.first() {
        s.push_str(&format!("Task:   {}\n", first.rendered_task()));
    }
    for report in reports {
        s.push('\n');
        s.push_str(&render_model_section(report));
    }
    s
}

fn render_model_section(report: &BenchReport) -> String {
    let mut s = String::new();
    let version = report
        .results
        .iter()
        .find_map(|r| r.model_version.clone())
        .unwrap_or_else(|| "<no version reported>".to_string());
    s.push_str(&format!(
        "Model:  {} ({}, api={}) — reported version: {}\n",
        report.model_id,
        report.provider.label(),
        report.api_model,
        version
    ));
    s.push_str(&format!(
        "Params: temp={} · runs={} · max_tokens={}\n",
        report.temperature, report.runs, report.max_tokens
    ));

    // Distribution block.
    let dist = report.distribution();
    s.push_str("\nDistribution:\n");
    if dist.selected.is_empty() && dist.hallucinated.is_empty() {
        s.push_str("  (no tool selected in any run)\n");
    }
    for (tool, count) in &dist.selected {
        s.push_str(&format!(
            "  {}  {}/{} ({})\n",
            tool,
            count,
            dist.total,
            pct(*count, dist.total)
        ));
    }
    for (name, count) in &dist.hallucinated {
        s.push_str(&format!(
            "  {} (hallucinated)  {}/{} ({})\n",
            name,
            count,
            dist.total,
            pct(*count, dist.total)
        ));
    }
    if dist.no_tool > 0 {
        s.push_str(&format!(
            "  (no tool / text answer)  {}/{} ({})\n",
            dist.no_tool,
            dist.total,
            pct(dist.no_tool, dist.total)
        ));
    }
    if dist.provider_error > 0 {
        s.push_str(&format!(
            "  (provider error)  {}/{} ({})\n",
            dist.provider_error,
            dist.total,
            pct(dist.provider_error, dist.total)
        ));
    }

    // Per-run table.
    s.push_str("\nPer-run:\n");
    s.push_str(&per_run_table(report));

    // One-line takeaway.
    s.push_str(&format!("\nTakeaway: {}\n", dist.takeaway()));
    s
}

/// Build the per-run table (run #, outcome, tool, args, latency, tokens).
fn per_run_table(report: &BenchReport) -> String {
    let headers = ["#", "outcome", "tool / detail", "args", "latency", "tokens"];
    let mut rows: Vec<[String; 6]> = Vec::new();
    for r in &report.results {
        let (outcome, detail, args) = match &r.outcome {
            Outcome::Selected {
                tool, args_check, ..
            } => (
                "selected".to_string(),
                tool.clone(),
                args_check.tag().to_string(),
            ),
            Outcome::NoTool { excerpt } => (
                "no_tool".to_string(),
                truncate(excerpt, 40),
                "-".to_string(),
            ),
            Outcome::HallucinatedTool { name, .. } => {
                ("hallucinated".to_string(), name.clone(), "-".to_string())
            }
            Outcome::ProviderError { detail } => (
                "provider_error".to_string(),
                truncate(detail, 40),
                "-".to_string(),
            ),
        };
        let tokens = match (r.usage.input_tokens, r.usage.output_tokens) {
            (Some(i), Some(o)) => format!("{i}in/{o}out"),
            _ => "-".to_string(),
        };
        rows.push([
            r.index.to_string(),
            outcome,
            detail,
            args,
            format!("{}ms", r.latency_ms),
            tokens,
        ]);
    }
    render_table(&headers, &rows)
}

/// A simple left-aligned fixed-width table.
fn render_table(headers: &[&str; 6], rows: &[[String; 6]]) -> String {
    let mut widths = [0usize; 6];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let fmt = |cells: &[String; 6]| {
        let mut line = String::from("  ");
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{:<width$}", c, width = widths[i]));
        }
        // Trim trailing spaces for stable snapshots.
        while line.ends_with(' ') {
            line.pop();
        }
        line.push('\n');
        line
    };
    let header_owned: [String; 6] = std::array::from_fn(|i| headers[i].to_string());
    let mut s = fmt(&header_owned);
    for row in rows {
        s.push_str(&fmt(row));
    }
    s
}

fn pct(part: usize, total: usize) -> String {
    if total == 0 {
        "0%".to_string()
    } else {
        format!("{:.0}%", (part as f64 / total as f64) * 100.0)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

/// Render full machine-readable JSON, including the exact rendered provider
/// request (minus auth), every raw provider response, and each run's
/// classification.
pub(crate) fn render_json(server: &jig_core::Implementation, reports: &[BenchReport]) -> String {
    let models: Vec<Value> = reports.iter().map(model_json).collect();
    let doc = json!({
        "serverInfo": server,
        "systemPrompt": bench::BENCH_SYSTEM_PROMPT,
        "models": models,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

fn model_json(report: &BenchReport) -> Value {
    let dist = report.distribution();
    let runs: Vec<Value> = report
        .results
        .iter()
        .map(|r| {
            json!({
                "run": r.index,
                "outcome": outcome_json(&r.outcome),
                "latencyMs": r.latency_ms,
                "usage": {
                    "inputTokens": r.usage.input_tokens,
                    "outputTokens": r.usage.output_tokens,
                    "totalTokens": r.usage.total_tokens,
                },
                "modelVersion": r.model_version,
                "rawResponse": r.raw_response,
            })
        })
        .collect();
    json!({
        "model": report.model_id,
        "provider": report.provider.label(),
        "apiModel": report.api_model,
        "temperature": report.temperature,
        "maxTokens": report.max_tokens,
        "runs": report.runs,
        "serverTools": report.server_tool_names,
        "renderedRequest": report.rendered_request,
        "distribution": distribution_json(&dist),
        "takeaway": dist.takeaway(),
        "results": runs,
    })
}

fn distribution_json(dist: &bench::Distribution) -> Value {
    json!({
        "total": dist.total,
        "selected": dist.selected.iter().map(|(n, c)| json!({ "tool": n, "count": c })).collect::<Vec<_>>(),
        "hallucinated": dist.hallucinated.iter().map(|(n, c)| json!({ "name": n, "count": c })).collect::<Vec<_>>(),
        "noTool": dist.no_tool,
        "providerError": dist.provider_error,
        "consistent": dist.is_consistent(),
    })
}

fn outcome_json(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Selected {
            tool,
            arguments,
            args_check,
        } => json!({
            "type": "selected",
            "tool": tool,
            "arguments": arguments,
            "argsValid": args_check.is_valid(),
            "argsCheck": args_check_json(args_check),
        }),
        Outcome::NoTool { excerpt } => json!({ "type": "no_tool", "excerpt": excerpt }),
        Outcome::HallucinatedTool { name, arguments } => json!({
            "type": "hallucinated_tool", "name": name, "arguments": arguments,
        }),
        Outcome::ProviderError { detail } => json!({ "type": "provider_error", "detail": detail }),
    }
}

fn args_check_json(check: &ArgCheck) -> Value {
    match check {
        ArgCheck::Valid => json!({ "status": "valid" }),
        ArgCheck::Invalid { errors } => json!({ "status": "invalid", "errors": errors }),
        ArgCheck::Unparseable { detail } => json!({ "status": "unparseable", "detail": detail }),
    }
}

/// Extract the task string from the rendered request for the header line. Works
/// for both dialects (last message is the user task).
trait RenderedTask {
    fn rendered_task(&self) -> String;
}

impl RenderedTask for BenchReport {
    fn rendered_task(&self) -> String {
        self.rendered_request
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|m| m.last())
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::bench::{RunResult, Usage};
    use jig_core::Implementation;

    fn server() -> Implementation {
        Implementation {
            name: "jig-mock-server".into(),
            version: "0.1.0".into(),
            title: None,
        }
    }

    fn tool(name: &str, schema: Value) -> jig_core::Tool {
        serde_json::from_value(json!({ "name": name, "inputSchema": schema })).unwrap()
    }

    /// A deterministic report over fixed data — no network — for snapshotting.
    fn fixture_report() -> BenchReport {
        let tools = vec![
            tool(
                "echo",
                json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }),
            ),
            tool(
                "make_reservation",
                json!({ "type": "object", "properties": { "date": { "type": "string" } }, "required": ["date"] }),
            ),
        ];
        let config = BenchConfig {
            model: BenchModel::resolve("gpt-4o").unwrap(),
            task: "Book a table for two tonight".into(),
            runs: 4,
            temperature: 1.0,
            max_tokens: 1024,
            timeout: None,
            base_url: None,
            api_key: "unused".into(),
        };
        let rendered = bench::render_request(Provider::OpenAI, &config, &tools);

        let results = vec![
            RunResult {
                index: 1,
                outcome: Outcome::Selected {
                    tool: "make_reservation".into(),
                    arguments: json!({ "date": "tonight" }),
                    args_check: ArgCheck::Valid,
                },
                latency_ms: 512,
                usage: Usage {
                    input_tokens: Some(42),
                    output_tokens: Some(7),
                    total_tokens: Some(49),
                },
                model_version: Some("gpt-4o-2024-08-06".into()),
                raw_response: json!({ "model": "gpt-4o-2024-08-06" }),
            },
            RunResult {
                index: 2,
                outcome: Outcome::Selected {
                    tool: "make_reservation".into(),
                    arguments: json!({}),
                    args_check: ArgCheck::Invalid {
                        errors: vec!["(root): missing required field 'date'".into()],
                    },
                },
                latency_ms: 488,
                usage: Usage {
                    input_tokens: Some(42),
                    output_tokens: Some(5),
                    total_tokens: Some(47),
                },
                model_version: Some("gpt-4o-2024-08-06".into()),
                raw_response: Value::Null,
            },
            RunResult {
                index: 3,
                outcome: Outcome::Selected {
                    tool: "echo".into(),
                    arguments: json!({ "text": "table for two" }),
                    args_check: ArgCheck::Valid,
                },
                latency_ms: 501,
                usage: Usage {
                    input_tokens: Some(42),
                    output_tokens: Some(8),
                    total_tokens: Some(50),
                },
                model_version: Some("gpt-4o-2024-08-06".into()),
                raw_response: Value::Null,
            },
            RunResult {
                index: 4,
                outcome: Outcome::NoTool {
                    excerpt: "I need a bit more information to book that.".into(),
                },
                latency_ms: 470,
                usage: Usage {
                    input_tokens: Some(42),
                    output_tokens: Some(11),
                    total_tokens: Some(53),
                },
                model_version: Some("gpt-4o-2024-08-06".into()),
                raw_response: Value::Null,
            },
        ];

        BenchReport {
            model_id: "gpt-4o".into(),
            provider: Provider::OpenAI,
            api_model: "gpt-4o".into(),
            temperature: 1.0,
            max_tokens: 1024,
            runs: 4,
            system_prompt: bench::BENCH_SYSTEM_PROMPT,
            rendered_request: rendered,
            results,
            server_tool_names: vec!["echo".into(), "make_reservation".into()],
        }
    }

    #[test]
    fn human_report_snapshot() {
        insta::assert_snapshot!("bench_human", render_human(&server(), &[fixture_report()]));
    }

    #[test]
    fn json_report_snapshot() {
        insta::assert_snapshot!("bench_json", render_json(&server(), &[fixture_report()]));
    }

    #[test]
    fn takeaway_is_unstable_for_mixed_outcomes() {
        let report = fixture_report();
        assert!(report.distribution().takeaway().starts_with("UNSTABLE"));
    }
}
