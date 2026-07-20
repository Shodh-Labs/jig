//! The sampling-backed bench: `bench_server` with **no credentials anywhere**.
//!
//! # The idea
//!
//! MCP `2025-06-18` lets a server ask its client for a model completion via
//! `sampling/createMessage` — the spec's own framing is "with no server API
//! keys necessary". When Jig runs as an MCP server inside a host that
//! advertises `capabilities.sampling`, the bench can borrow *the host's* model
//! for its tool-selection measurement. Jig holds no key, sees no key, and needs
//! no account.
//!
//! # The flow
//!
//! 1. The host declares `{"capabilities": {"sampling": {}}}` at `initialize`;
//!    [`ServeState::client_supports_sampling`] records it.
//! 2. `bench_server` connects to the *target* server as an ordinary MCP client
//!    and lists its tools.
//! 3. For each run, Jig sends `sampling/createMessage` back to the host with
//!    the tool surface and the task, and waits for `{role, content, model,
//!    stopReason}`.
//! 4. Each reply is classified through the **existing** bench taxonomy —
//!    [`jig_core::classify_sampling_text`] routes through the same
//!    hallucination check as the direct-API path, and
//!    [`jig_core::finalize_args_check`] applies the same schema validator.
//!
//! # Two honesty rules this module exists to enforce
//!
//! **Never degrade silently.** A host without sampling gets an explicit
//! failure naming all three ways to get a model, not a quietly empty result.
//!
//! **Never overclaim the model.** Sampling gives the server no say in which
//! model the host picks — Jig sends no `hints` at all, precisely so it cannot
//! pretend otherwise. The report records whatever identity the host returned,
//! or [`jig_core::SAMPLING_MODEL_UNKNOWN`] when it returned none, and labels
//! the run as host-selected either way.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use jig_core::bench::{
    classify_sampling_text, finalize_args_check, host_models_of, render_sampling_params,
    Distribution, Outcome, RunResult, SamplingBenchReport, Usage, BENCH_SYSTEM_PROMPT,
    SAMPLING_MODEL_UNKNOWN,
};
use serde_json::{json, Value};

use super::tools::{list_target_tools, target_from, timeout_of, Defaults, ToolOutcome};
use super::ServeState;

/// Default runs when the caller does not say.
const DEFAULT_RUNS: usize = 3;
/// Upper bound on runs, mirroring the schema's `maximum`. Each run is a real
/// model call the host pays for; an agent must not be able to ask for a
/// thousand.
const MAX_RUNS: usize = 20;
/// Default requested temperature.
const DEFAULT_TEMPERATURE: f64 = 1.0;

/// The message shown when the host does not support sampling.
///
/// Deliberately exhaustive: this is the single most likely way a user meets
/// `bench_server`, and a dead end that does not name the way out is a bug.
const NO_SAMPLING_HELP: &str = "This host does not support MCP sampling, so `bench_server` has no \
    model to measure with.

`bench_server` works by asking the host for completions via `sampling/createMessage` (MCP \
2025-06-18). A client that supports it declares `capabilities.sampling` at initialize; this one \
did not.

Three ways forward:
  1. Run `jig serve` inside a host that supports sampling — then this tool works with no \
credentials at all.
  2. Use a local model from the command line:
     jig bench --stdio \"<target server>\" --task \"<task>\" \\
       --base-url http://localhost:11434/v1 --no-auth --api-model llama3.1
  3. Use your own provider key from the command line:
     ANTHROPIC_API_KEY=... jig bench --stdio \"<target server>\" --task \"<task>\"

Every other tool here — check_server, budget_server, context_server, inspect_server, \
list_local_servers — needs no model at all and works right now.";

/// Run `bench_server`.
pub(crate) async fn bench_server(
    state: &Arc<ServeState>,
    args: &Value,
    defaults: Defaults,
) -> ToolOutcome {
    // Refuse loudly, before touching the target server, when there is no model
    // to be had. Connecting first would waste a process spawn to reach the same
    // conclusion.
    if !state.client_supports_sampling().await {
        return Err(NO_SAMPLING_HELP.to_string());
    }

    let target = target_from(args)?;
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "`task` is required: give the model a plain-language job, e.g. \"find the docs page \
             about rate limits\"."
                .to_string()
        })?
        .to_string();
    let runs = args
        .get("runs")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_RUNS))
        .unwrap_or(DEFAULT_RUNS);
    let temperature = args
        .get("temperature")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TEMPERATURE);

    let (server, tools, _) =
        list_target_tools(&target, defaults, timeout_of(args, defaults)).await?;
    if tools.is_empty() {
        return Err(
            "The target server exposes no tools, so there is nothing to bench.".to_string(),
        );
    }

    let report = run_sampling_bench(state, &tools, &task, runs, temperature).await;
    let summary = render_summary(&server, &report);
    let structured = render_structured(&server, &report);
    Ok((summary, structured))
}

/// Issue the `sampling/createMessage` runs and classify each reply.
async fn run_sampling_bench(
    state: &Arc<ServeState>,
    tools: &[jig_core::Tool],
    task: &str,
    runs: usize,
    temperature: f64,
) -> SamplingBenchReport {
    let server_tools: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    let params = render_sampling_params(
        tools,
        task,
        temperature,
        jig_core::bench::DEFAULT_MAX_TOKENS,
    );

    let mut results = Vec::with_capacity(runs);
    for index in 1..=runs {
        let started = Instant::now();
        let sent = state
            .request_client(
                "sampling/createMessage",
                params.clone(),
                super::tools::BENCH_SAMPLING_TIMEOUT,
            )
            .await;
        let latency_ms = started.elapsed().as_millis();

        let result = match sent {
            Ok(result) => {
                let text = sampled_text(&result);
                let mut outcome = classify_sampling_text(&text, &server_tools);
                // The same schema validation the direct-API path applies.
                finalize_args_check(tools, &mut outcome);
                RunResult {
                    index,
                    outcome,
                    latency_ms,
                    usage: Usage::default(),
                    // Whatever the host said it used — never a guess.
                    model_version: result
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    raw_response: result,
                }
            }
            // A host that refused, errored or timed out is a provider error in
            // exactly the sense the taxonomy already means: the measurement did
            // not happen, and that is data.
            Err(detail) => RunResult {
                index,
                outcome: Outcome::ProviderError { detail },
                latency_ms,
                usage: Usage::default(),
                model_version: None,
                raw_response: Value::Null,
            },
        };
        results.push(result);
    }

    let mut server_tool_names: Vec<String> = server_tools.into_iter().collect();
    server_tool_names.sort();

    SamplingBenchReport {
        task: task.to_string(),
        runs,
        temperature,
        max_tokens: jig_core::bench::DEFAULT_MAX_TOKENS,
        system_prompt: BENCH_SYSTEM_PROMPT,
        rendered_request: params,
        host_models: host_models_of(&results),
        results,
        server_tool_names,
    }
}

/// Extract the assistant text from a `sampling/createMessage` result.
///
/// The spec's result shape is `{role, content: {type: "text", text}, model,
/// stopReason}` — `content` is a single block, not an array. Some hosts send an
/// array anyway; accept both rather than scoring a well-behaved model as
/// "no tool" because its host framed the reply differently.
fn sampled_text(result: &Value) -> String {
    let content = result.get("content");
    match content {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        Some(block) => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    }
}

/// The human summary. Leads with the provenance caveat, because a reader who
/// skims must not come away thinking a named model was measured.
fn render_summary(server: &jig_core::Implementation, report: &SamplingBenchReport) -> String {
    let dist = report.distribution();
    let mut s = String::new();
    s.push_str(&format!("Server: {} v{}\n", server.name, server.version));
    s.push_str(&format!("Task:   {}\n", report.task));
    s.push_str(&format!(
        "Model:  {} — chosen by the host, not by Jig\n",
        report.model_label()
    ));
    s.push_str(&format!(
        "Params: temp={} (requested) · runs={} · max_tokens={}\n",
        report.temperature, report.runs, report.max_tokens
    ));
    s.push_str("Source: MCP sampling — the host's model answered; no API key was used.\n");

    s.push_str("\nDistribution:\n");
    if dist.selected.is_empty() && dist.hallucinated.is_empty() {
        s.push_str("  (no tool selected in any run)\n");
    }
    for (tool, count) in &dist.selected {
        s.push_str(&format!("  {tool}  {count}/{}\n", dist.total));
    }
    for (name, count) in &dist.hallucinated {
        s.push_str(&format!(
            "  {name} (hallucinated)  {count}/{}\n",
            dist.total
        ));
    }
    if dist.no_tool > 0 {
        s.push_str(&format!(
            "  (no tool / text answer)  {}/{}\n",
            dist.no_tool, dist.total
        ));
    }
    if dist.provider_error > 0 {
        s.push_str(&format!(
            "  (host error)  {}/{}\n",
            dist.provider_error, dist.total
        ));
    }

    s.push_str("\nPer-run:\n");
    for r in &report.results {
        let detail = match &r.outcome {
            Outcome::Selected {
                tool, args_check, ..
            } => format!("selected `{tool}` (args {})", args_check.tag()),
            Outcome::NoTool { excerpt } => format!("no tool — \"{excerpt}\""),
            Outcome::HallucinatedTool { name, .. } => format!("hallucinated `{name}`"),
            Outcome::ProviderError { detail } => format!("host error: {detail}"),
        };
        s.push_str(&format!("  {}. {detail} ({}ms)\n", r.index, r.latency_ms));
    }

    s.push_str(&format!("\nTakeaway: {}\n", dist.takeaway()));
    s
}

/// The machine-readable report. Mirrors the direct-API bench document where the
/// two are comparable, and departs from it exactly where honesty demands: there
/// is no `apiModel` or `provider`, because Jig did not choose either.
fn render_structured(server: &jig_core::Implementation, report: &SamplingBenchReport) -> Value {
    let dist = report.distribution();
    json!({
        "serverInfo": server,
        "task": report.task,
        "modelAccess": "mcp-sampling",
        "keyless": true,
        "hostModels": report.host_models,
        "modelLabel": report.model_label(),
        "modelSelectedBy": "host",
        "temperatureRequested": report.temperature,
        "maxTokens": report.max_tokens,
        "runs": report.runs,
        "systemPrompt": report.system_prompt,
        "serverTools": report.server_tool_names,
        "renderedRequest": report.rendered_request,
        "distribution": distribution_json(&dist),
        "takeaway": dist.takeaway(),
        "results": report.results.iter().map(run_json).collect::<Vec<_>>(),
    })
}

fn distribution_json(dist: &Distribution) -> Value {
    json!({
        "total": dist.total,
        "selected": dist.selected.iter().map(|(n, c)| json!({ "tool": n, "count": c })).collect::<Vec<_>>(),
        "hallucinated": dist.hallucinated.iter().map(|(n, c)| json!({ "name": n, "count": c })).collect::<Vec<_>>(),
        "noTool": dist.no_tool,
        "providerError": dist.provider_error,
        "consistent": dist.is_consistent(),
    })
}

fn run_json(r: &RunResult) -> Value {
    let outcome = match &r.outcome {
        Outcome::Selected {
            tool,
            arguments,
            args_check,
        } => json!({
            "type": "selected",
            "tool": tool,
            "arguments": arguments,
            "argsValid": args_check.is_valid(),
        }),
        Outcome::NoTool { excerpt } => json!({ "type": "no_tool", "excerpt": excerpt }),
        Outcome::HallucinatedTool { name, arguments } => {
            json!({ "type": "hallucinated_tool", "name": name, "arguments": arguments })
        }
        Outcome::ProviderError { detail } => json!({ "type": "provider_error", "detail": detail }),
    };
    json!({
        "run": r.index,
        "outcome": outcome,
        "latencyMs": r.latency_ms,
        "hostModel": r.model_version.clone().unwrap_or_else(|| SAMPLING_MODEL_UNKNOWN.to_string()),
        "rawResponse": r.raw_response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_no_sampling_message_names_all_three_alternatives() {
        // The failure mode this guards against is a dead end. Each escape route
        // must be present and runnable.
        assert!(NO_SAMPLING_HELP.contains("capabilities.sampling"));
        assert!(NO_SAMPLING_HELP.contains("host that supports sampling"));
        assert!(NO_SAMPLING_HELP.contains("--base-url http://localhost:11434/v1 --no-auth"));
        assert!(NO_SAMPLING_HELP.contains("ANTHROPIC_API_KEY"));
        // …and it must say what still works without any of them.
        assert!(NO_SAMPLING_HELP.contains("check_server"));
    }

    #[test]
    fn sampled_text_reads_the_spec_shape_and_the_common_variant() {
        // The 2025-06-18 shape: content is one block.
        let spec = json!({
            "role": "assistant",
            "content": { "type": "text", "text": "{\"tool\": null}" },
            "model": "claude-3-sonnet-20240307",
            "stopReason": "endTurn"
        });
        assert_eq!(sampled_text(&spec), "{\"tool\": null}");

        // A host that sends an array instead is still understood.
        let array = json!({
            "content": [ { "type": "text", "text": "a" }, { "type": "text", "text": "b" } ]
        });
        assert_eq!(sampled_text(&array), "a b");

        assert_eq!(sampled_text(&json!({})), "");
    }

    #[test]
    fn a_host_that_names_no_model_is_reported_as_unknown_not_guessed() {
        let report = SamplingBenchReport {
            task: "t".into(),
            runs: 1,
            temperature: 1.0,
            max_tokens: 1024,
            system_prompt: BENCH_SYSTEM_PROMPT,
            rendered_request: Value::Null,
            results: vec![RunResult {
                index: 1,
                outcome: Outcome::NoTool {
                    excerpt: "x".into(),
                },
                latency_ms: 1,
                usage: Usage::default(),
                model_version: None,
                raw_response: Value::Null,
            }],
            server_tool_names: vec![],
            host_models: vec![SAMPLING_MODEL_UNKNOWN.to_string()],
        };
        let server = jig_core::Implementation {
            name: "t".into(),
            version: "1".into(),
            title: None,
        };
        let summary = render_summary(&server, &report);
        assert!(summary.contains(SAMPLING_MODEL_UNKNOWN), "{summary}");
        assert!(summary.contains("chosen by the host"), "{summary}");
        assert!(summary.contains("no API key was used"), "{summary}");

        let doc = render_structured(&server, &report);
        assert_eq!(doc["modelSelectedBy"], "host");
        assert_eq!(doc["modelAccess"], "mcp-sampling");
        assert_eq!(doc["keyless"], true);
        // No field anywhere claims a model Jig chose.
        assert!(doc.get("apiModel").is_none());
        assert!(doc.get("provider").is_none());
        assert_eq!(doc["results"][0]["hostModel"], SAMPLING_MODEL_UNKNOWN);
    }

    #[test]
    fn a_varying_host_model_is_labelled_as_varying() {
        let report = SamplingBenchReport {
            task: "t".into(),
            runs: 2,
            temperature: 1.0,
            max_tokens: 1024,
            system_prompt: BENCH_SYSTEM_PROMPT,
            rendered_request: Value::Null,
            results: vec![],
            server_tool_names: vec![],
            host_models: vec!["model-a".into(), "model-b".into()],
        };
        assert!(report.model_label().contains("host varied the model"));
    }
}
