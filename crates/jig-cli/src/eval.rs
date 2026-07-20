//! `jig eval` — run a `.jig` eval suite against a live model.
//!
//! Connects to a server, lists its tools once, then executes every case in
//! every suite via the [`jig_core::eval`] engine (which reuses the bench engine
//! under the hood) and reports the result. This module owns CLI plumbing: suite
//! discovery, key resolution, exit codes, and the three renderers (human table,
//! `--json`, JUnit XML), all pure functions of a [`RunReport`] so they are
//! snapshot-testable.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use jig_core::bench::{self, BenchModel, Outcome};
use jig_core::eval::{self, CaseReport, CaseVerdict, EvalConfig, RunReport, SuiteReport};
use jig_core::ProtocolTap;
use serde_json::{json, Value};

use crate::bench::{auth_label, Endpoint};
use crate::{client_options, emit, warn_non_protocol_output, write_tap_if_requested, Target};

/// Exit code when the eval ran cleanly but the run did not pass its gate /
/// `must_pass` requirements. Distinct from `1` (a jig-level failure) and the
/// reserved `2` (a tool reported an error).
const EXIT_EVAL_FAILURE: u8 = 3;

/// Run `jig eval`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    suite_specs: Vec<PathBuf>,
    model: Option<String>,
    api_model: Option<String>,
    endpoint: Endpoint,
    runs_override: Option<usize>,
    temp: Option<f64>,
    gate: Option<f64>,
    as_json: bool,
    junit_path: Option<&Path>,
    tap_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    // 1) Discover and load suites (fail fast on any format error).
    let suites = load_suites(&suite_specs)?;
    if suites.iter().all(|s| s.cases.is_empty()) {
        return Err("the loaded suite(s) contain no cases".to_string());
    }

    // 2) Resolve the model and its key up front, before touching the server.
    let model_id = model.unwrap_or_else(|| endpoint.default_model().to_string());
    let mut resolved = BenchModel::resolve(&model_id).map_err(|e| e.to_string())?;
    if let Some(over) = api_model {
        resolved = resolved.with_api_model(over);
    }
    let key = endpoint.resolve_key(resolved.provider)?;

    if let Some(g) = gate {
        if !(0.0..=1.0).contains(&g) {
            return Err(format!("--gate must be within 0..=1 (got {g})"));
        }
    }

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        suites,
        resolved,
        key,
        &endpoint,
        runs_override,
        temp,
        gate,
        as_json,
        junit_path,
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
    suites: Vec<eval::Suite>,
    model: BenchModel,
    key: String,
    endpoint: &Endpoint,
    runs_override: Option<usize>,
    temp: Option<f64>,
    gate: Option<f64>,
    as_json: bool,
    junit_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;
    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    client.shutdown().await.map_err(|e| e.to_string())?;

    if tools.is_empty() {
        return Err("the server exposes no tools — there is nothing to eval".to_string());
    }

    let opts = client_options(timeout_secs, max_message_bytes);
    let config = EvalConfig {
        model,
        api_key: key,
        runs_override,
        temp_override: temp,
        gate,
        timeout: opts.request_timeout,
        max_tokens: bench::DEFAULT_MAX_TOKENS,
        base_url: endpoint.effective_base_url(),
    };

    let report = eval::run_eval(&tools, &config, &suites)
        .await
        .map_err(|e| e.to_string())?;

    if as_json {
        emit(&render_json(&report));
    } else {
        emit(&render_human(&report));
    }
    if let Some(path) = junit_path {
        match std::fs::write(path, render_junit(&report)) {
            Ok(()) => eprintln!("jig: wrote JUnit report to {}", path.display()),
            Err(e) => eprintln!(
                "jig: warning: failed to write JUnit to {}: {e}",
                path.display()
            ),
        }
    }

    if report.passed() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(EXIT_EVAL_FAILURE))
    }
}

// ---------------------------------------------------------------------------
// Suite discovery
// ---------------------------------------------------------------------------

/// Resolve the `--suite` specs (default `./.jig/`) into loaded suites. A spec
/// that names a directory contributes every `*.yaml`/`*.yml` file in it (sorted,
/// non-recursive); a spec that names a file contributes that file.
fn load_suites(specs: &[PathBuf]) -> Result<Vec<eval::Suite>, String> {
    let default = PathBuf::from("./.jig");
    let specs: Vec<PathBuf> = if specs.is_empty() {
        vec![default]
    } else {
        specs.to_vec()
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for spec in &specs {
        if spec.is_dir() {
            let mut in_dir: Vec<PathBuf> = std::fs::read_dir(spec)
                .map_err(|e| format!("failed to read suite directory {}: {e}", spec.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| is_yaml(p))
                .collect();
            in_dir.sort();
            files.extend(in_dir);
        } else if spec.is_file() {
            files.push(spec.clone());
        } else {
            return Err(format!(
                "suite path {} does not exist (pass a `.jig` file or a directory of them)",
                spec.display()
            ));
        }
    }

    if files.is_empty() {
        return Err(format!(
            "no eval suites found (looked in: {}). Create a `.jig/*.yaml` suite, or draft one \
             with `jig bench --save-case <file>`.",
            specs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut suites = Vec::with_capacity(files.len());
    for f in files {
        suites.push(eval::load_suite_file(&f).map_err(|e| e.to_string())?);
    }
    Ok(suites)
}

fn is_yaml(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Render the human report: a per-suite table, a totals block, and a
/// pinned-context block sufficient to reproduce the run.
pub(crate) fn render_human(report: &RunReport) -> String {
    let mut s = String::new();
    let (n_suites, n_cases) = (report.suites.len(), report.cases().count());
    s.push_str(&format!(
        "Eval — {} suite{}, {} case{}\n",
        n_suites,
        plural(n_suites),
        n_cases,
        plural(n_cases)
    ));
    s.push_str(&format!(
        "Model: {} ({}, api={}) — reported version: {}\n",
        report.model_id,
        report.provider_label,
        report.api_model,
        report.reported_version.as_deref().unwrap_or("<none>")
    ));

    for suite in &report.suites {
        s.push('\n');
        s.push_str(&render_suite(suite));
    }

    s.push_str("\nTotals:\n");
    let acc = match report.overall_accuracy() {
        Some(a) => format!(
            "{}/{} ({:.0}%)",
            report.total_passes(),
            report.total_counted(),
            a * 100.0
        ),
        None => "n/a (no runs counted)".to_string(),
    };
    s.push_str(&format!("  accuracy:  {acc}\n"));
    s.push_str(&format!(
        "  cases:     {} passed · {} failed · {} flaky · {} errored\n",
        report.cases_passed(),
        report.cases_failed(),
        report.cases_flaky(),
        report.cases_errored()
    ));
    match report.gate {
        Some(g) => {
            let met = report.gate_met();
            let detail = match report.overall_accuracy() {
                Some(a) => format!(
                    "{:.0}% {} {:.0}%",
                    a * 100.0,
                    if met { ">=" } else { "<" },
                    g * 100.0
                ),
                None => "no runs counted".to_string(),
            };
            s.push_str(&format!(
                "  gate:      {:.2} → {} ({detail})\n",
                g,
                if met { "met" } else { "NOT MET" }
            ));
        }
        None => s.push_str("  gate:      (none set)\n"),
    }
    let mp = report.must_pass_failures();
    if !mp.is_empty() {
        s.push_str(&format!(
            "  must_pass: {} failing must_pass case(s): {}\n",
            mp.len(),
            mp.iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push_str(&format!(
        "  verdict:   {}\n",
        if report.passed() { "PASS" } else { "FAIL" }
    ));

    s.push_str(&render_pinned(report));
    s
}

fn render_suite(suite: &SuiteReport) -> String {
    let mut s = String::new();
    s.push_str(&format!("Suite: {}  ({})\n", suite.name, suite.source));

    // Column widths.
    let id_w = suite
        .cases
        .iter()
        .map(|c| c.id.chars().count())
        .chain(std::iter::once("case".len()))
        .max()
        .unwrap_or(4);
    let rate_w = suite
        .cases
        .iter()
        .map(|c| rate_cell(c).chars().count())
        .chain(std::iter::once("rate".len()))
        .max()
        .unwrap_or(4);

    s.push_str(&format!(
        "  {:<id_w$}  {:<rate_w$}  {:>4}  {}\n",
        "case",
        "rate",
        "runs",
        "verdict",
        id_w = id_w,
        rate_w = rate_w
    ));
    for c in &suite.cases {
        s.push_str(&format!(
            "  {:<id_w$}  {:<rate_w$}  {:>4}  {}\n",
            c.id,
            rate_cell(c),
            c.runs,
            verdict_cell(c),
            id_w = id_w,
            rate_w = rate_w
        ));
        // Failing-run detail for anything that is not a clean pass.
        if c.verdict != CaseVerdict::Pass || c.flaky {
            for line in detail_lines(c) {
                s.push_str(&format!("      {line}\n"));
            }
        }
    }
    s
}

fn rate_cell(c: &CaseReport) -> String {
    format!("{} ({})", c.rate_fraction(), c.rate_pct())
}

fn verdict_cell(c: &CaseReport) -> String {
    let base = c.verdict.label();
    let mut cell = base.to_string();
    if c.flaky {
        cell.push_str(" · FLAKY");
    }
    if c.must_pass {
        cell.push_str(" · must_pass");
    }
    cell
}

/// The compact failing-run detail lines for one case (capped).
fn detail_lines(c: &CaseReport) -> Vec<String> {
    const MAX: usize = 6;
    let failing: Vec<&_> = c.run_details.iter().filter(|d| !d.passed).collect();
    let mut out: Vec<String> = failing
        .iter()
        .take(MAX)
        .map(|d| format!("run {}: {}", d.index, d.summary))
        .collect();
    if failing.len() > MAX {
        out.push(format!("... and {} more", failing.len() - MAX));
    }
    out
}

fn render_pinned(report: &RunReport) -> String {
    let mut s = String::new();
    s.push_str("\nPinned context (reproduce this run):\n");
    s.push_str(&format!(
        "  model:             {} ({})\n",
        report.model_id, report.provider_label
    ));
    s.push_str(&format!("  api model:         {}\n", report.api_model));
    // Where every case was actually sent, and on whose credential. A pinned
    // context that does not name the endpoint is not reproducible.
    s.push_str(&format!("  endpoint:          {}\n", report.endpoint));
    s.push_str(&format!(
        "  auth:              {}\n",
        auth_label(report.keyless)
    ));
    s.push_str(&format!(
        "  reported version:  {}\n",
        report.reported_version.as_deref().unwrap_or("<none>")
    ));

    // Distinct effective temps / runs across cases.
    let temps = distinct(report.cases().map(|c| format_f(c.temperature)));
    let runs = distinct(report.cases().map(|c| c.runs.to_string()));
    let temp_line = match report.temp_override {
        Some(t) => format!("{} (--temp override)", format_f(t)),
        None => temps.join(", "),
    };
    let runs_line = match report.runs_override {
        Some(n) => format!("{n} (--runs-override)"),
        None => runs.join(", "),
    };
    s.push_str(&format!("  temperature:       {temp_line}\n"));
    s.push_str(&format!("  runs:              {runs_line} (per case)\n"));
    s.push_str(
        "  scoring:           deterministic matchers only (v1): exact · contains · regex · \
         one_of · range; selected args are JSON-Schema validated\n",
    );
    s.push_str(&format!(
        "  system prompt:     {:?}\n",
        report.system_prompt
    ));
    s.push_str("  suites:\n");
    for suite in &report.suites {
        s.push_str(&format!(
            "    {}  ({})  {} case{}\n",
            suite.name,
            suite.source,
            suite.cases.len(),
            plural(suite.cases.len())
        ));
    }
    s
}

fn distinct(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = Vec::new();
    for v in it {
        if !seen.contains(&v) {
            seen.push(v);
        }
    }
    if seen.is_empty() {
        seen.push("-".to_string());
    }
    seen
}

fn format_f(f: f64) -> String {
    // Trim a trailing ".0" for whole numbers so 1.0 renders as "1".
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

/// Render the full machine-readable report: every case, every run, with the
/// underlying bench outcome (arguments, validation, usage) attached.
pub(crate) fn render_json(report: &RunReport) -> String {
    let suites: Vec<Value> = report.suites.iter().map(suite_json).collect();
    let doc = json!({
        "model": report.model_id,
        "provider": report.provider_label,
        "apiModel": report.api_model,
        "reportedVersion": report.reported_version,
        "gate": report.gate,
        "systemPrompt": report.system_prompt,
        "accuracy": report.overall_accuracy(),
        "totals": {
            "passes": report.total_passes(),
            "counted": report.total_counted(),
            "casesPassed": report.cases_passed(),
            "casesFailed": report.cases_failed(),
            "casesFlaky": report.cases_flaky(),
            "casesErrored": report.cases_errored(),
        },
        "verdict": if report.passed() { "pass" } else { "fail" },
        "suites": suites,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

fn suite_json(suite: &SuiteReport) -> Value {
    json!({
        "suite": suite.name,
        "source": suite.source,
        "cases": suite.cases.iter().map(case_json).collect::<Vec<_>>(),
    })
}

fn case_json(c: &CaseReport) -> Value {
    // Zip the scored per-run detail with the underlying bench run for full
    // fidelity (arguments, validation, usage, version, latency).
    let runs: Vec<Value> = c
        .run_details
        .iter()
        .zip(c.bench.results.iter())
        .map(|(d, r)| {
            json!({
                "run": d.index,
                "passed": d.passed,
                "outcome": d.outcome_tag,
                "selectedTool": d.selected_tool,
                "summary": d.summary,
                "detail": outcome_json(&r.outcome),
                "usage": {
                    "inputTokens": r.usage.input_tokens,
                    "outputTokens": r.usage.output_tokens,
                    "totalTokens": r.usage.total_tokens,
                },
                "modelVersion": r.model_version,
                "latencyMs": r.latency_ms,
            })
        })
        .collect();
    json!({
        "id": c.id,
        "task": c.task,
        "expectedTool": c.expected_tool,
        "runs": c.runs,
        "minRate": c.min_rate,
        "temperature": c.temperature,
        "mustPass": c.must_pass,
        "verdict": verdict_tag(c.verdict),
        "flaky": c.flaky,
        "rate": c.rate,
        "passes": c.passes,
        "counted": c.counted,
        "providerErrors": c.provider_errors,
        "notToolsHits": c.not_tools_hits,
        "results": runs,
    })
}

fn verdict_tag(v: CaseVerdict) -> &'static str {
    match v {
        CaseVerdict::Pass => "pass",
        CaseVerdict::Fail => "fail",
        CaseVerdict::NotTools => "not_tools",
        CaseVerdict::Errored => "errored",
    }
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
            "argsCheck": args_check.tag(),
        }),
        Outcome::NoTool { excerpt } => json!({ "type": "no_tool", "excerpt": excerpt }),
        Outcome::HallucinatedTool { name, arguments } => {
            json!({ "type": "hallucinated_tool", "name": name, "arguments": arguments })
        }
        Outcome::ProviderError { detail } => json!({ "type": "provider_error", "detail": detail }),
    }
}

// ---------------------------------------------------------------------------
// JUnit rendering
// ---------------------------------------------------------------------------

/// Render CI-native JUnit XML: one `<testsuite>` per suite, one `<testcase>` per
/// case. A failing case carries a `<failure>` (an errored case a `<error>`)
/// whose body is the per-run breakdown.
pub(crate) fn render_junit(report: &RunReport) -> String {
    let total: usize = report.cases().count();
    let failures = report.cases_failed();
    let errors = report.cases_errored();
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(&format!(
        "<testsuites name=\"jig eval\" tests=\"{total}\" failures=\"{failures}\" errors=\"{errors}\">\n"
    ));
    for suite in &report.suites {
        let s_fail = suite
            .cases
            .iter()
            .filter(|c| matches!(c.verdict, CaseVerdict::Fail | CaseVerdict::NotTools))
            .count();
        let s_err = suite
            .cases
            .iter()
            .filter(|c| c.verdict == CaseVerdict::Errored)
            .count();
        s.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\">\n",
            xml_escape(&suite.name),
            suite.cases.len(),
            s_fail,
            s_err
        ));
        for c in &suite.cases {
            s.push_str(&format!(
                "    <testcase name=\"{}\" classname=\"{}\">\n",
                xml_escape(&c.id),
                xml_escape(&suite.name)
            ));
            let body = detail_lines(c).join("\n");
            match c.verdict {
                CaseVerdict::Pass => {
                    if c.flaky {
                        // A passing-but-flaky case is a finding; surface it
                        // without failing CI (JUnit has no "flaky", so use
                        // system-out).
                        s.push_str(&format!(
                            "      <system-out>{}</system-out>\n",
                            xml_escape(&format!(
                                "FLAKY: {} passed but flipped across runs\n{body}",
                                c.rate_fraction()
                            ))
                        ));
                    }
                }
                CaseVerdict::Errored => {
                    s.push_str(&format!(
                        "      <error message=\"{}\">{}</error>\n",
                        xml_escape(&format!(
                            "{} of {} runs were provider errors",
                            c.provider_errors, c.runs
                        )),
                        xml_escape(&body)
                    ));
                }
                CaseVerdict::Fail | CaseVerdict::NotTools => {
                    s.push_str(&format!(
                        "      <failure message=\"{}\">{}</failure>\n",
                        xml_escape(&format!(
                            "rate {} ({}) below min_rate {:.2}{}",
                            c.rate_fraction(),
                            c.rate_pct(),
                            c.min_rate,
                            if c.verdict == CaseVerdict::NotTools {
                                "; selected a not_tools (known-wrong) tool"
                            } else {
                                ""
                            }
                        )),
                        xml_escape(&body)
                    ));
                }
            }
            s.push_str("    </testcase>\n");
        }
        s.push_str("  </testsuite>\n");
    }
    s.push_str("</testsuites>\n");
    s
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::bench::{ArgCheck, BenchReport, Provider, RunResult, Usage};
    use jig_core::eval::{score_case, Case, Expect, Matcher};

    fn run_result(index: usize, outcome: Outcome) -> RunResult {
        RunResult {
            index,
            outcome,
            latency_ms: 0,
            usage: Usage {
                input_tokens: Some(42),
                output_tokens: Some(7),
                total_tokens: Some(49),
            },
            model_version: Some("gpt-mock-1".into()),
            raw_response: Value::Null,
        }
    }

    fn bench_report(outcomes: Vec<Outcome>) -> BenchReport {
        let results = outcomes
            .into_iter()
            .enumerate()
            .map(|(i, o)| run_result(i + 1, o))
            .collect::<Vec<_>>();
        let runs = results.len();
        BenchReport {
            model_id: "gpt-4o".into(),
            provider: Provider::OpenAI,
            api_model: "gpt-4o".into(),
            temperature: 1.0,
            max_tokens: 1024,
            runs,
            system_prompt: bench::BENCH_SYSTEM_PROMPT,
            rendered_request: Value::Null,
            results,
            server_tool_names: vec!["search_docs".into(), "fetch_page".into()],
            endpoint: bench::provider_endpoint(Provider::OpenAI, None),
            keyless: false,
        }
    }

    fn selected(tool: &str, args: Value) -> Outcome {
        Outcome::Selected {
            tool: tool.into(),
            arguments: args,
            args_check: ArgCheck::Valid,
        }
    }

    fn case(id: &str, tool: &str, args: Option<Vec<(&str, Matcher)>>, must_pass: bool) -> Case {
        Case {
            id: id.into(),
            task: format!("task for {id}"),
            expect: Expect {
                tool: tool.into(),
                args: args.map(|v| v.into_iter().map(|(k, m)| (k.to_string(), m)).collect()),
                not_tools: vec![],
            },
            runs: None,
            min_rate: None,
            must_pass,
        }
    }

    /// A fixture with a clean pass, a flaky-but-passing case, an outright fail,
    /// and an errored case — every rendering branch in one report.
    fn mixed_report() -> RunReport {
        let pass = score_case(
            &case(
                "find-rate-limits",
                "search_docs",
                Some(vec![("query", Matcher::Contains("rate".into()))]),
                false,
            ),
            3,
            0.8,
            1.0,
            bench_report(vec![
                selected("search_docs", json!({ "query": "rate limits" })),
                selected("search_docs", json!({ "query": "rate limits" })),
                selected("search_docs", json!({ "query": "rate limits" })),
            ]),
        );
        let flaky = score_case(
            &case("list-endpoints", "search_docs", None, false),
            4,
            0.5,
            1.0,
            bench_report(vec![
                selected("search_docs", json!({})),
                selected("fetch_page", json!({})),
                selected("search_docs", json!({})),
                selected("fetch_page", json!({})),
            ]),
        );
        let fail = score_case(
            &case("book-table", "make_reservation", None, true),
            3,
            0.8,
            1.0,
            bench_report(vec![
                selected("search_docs", json!({})),
                Outcome::NoTool {
                    excerpt: "I can't help".into(),
                },
                selected("search_docs", json!({})),
            ]),
        );
        let errored = score_case(
            &case("flaky-provider", "search_docs", None, false),
            3,
            0.8,
            1.0,
            bench_report(vec![
                Outcome::ProviderError {
                    detail: "HTTP 500: boom".into(),
                },
                Outcome::ProviderError {
                    detail: "HTTP 500: boom".into(),
                },
                Outcome::ProviderError {
                    detail: "HTTP 500: boom".into(),
                },
            ]),
        );
        RunReport {
            model_id: "gpt-4o".into(),
            provider_label: "openai",
            api_model: "gpt-4o".into(),
            reported_version: Some("gpt-mock-1".into()),
            runs_override: None,
            temp_override: None,
            gate: Some(0.8),
            system_prompt: bench::BENCH_SYSTEM_PROMPT,
            endpoint: bench::provider_endpoint(Provider::OpenAI, None),
            keyless: false,
            suites: vec![SuiteReport {
                name: "search-basics".into(),
                source: ".jig/search.yaml".into(),
                cases: vec![pass, flaky, fail, errored],
            }],
        }
    }

    /// An all-passing report (the clean, green path).
    fn passing_report() -> RunReport {
        let a = score_case(
            &case("find-rate-limits", "search_docs", None, true),
            3,
            0.8,
            1.0,
            bench_report(vec![
                selected("search_docs", json!({})),
                selected("search_docs", json!({})),
                selected("search_docs", json!({})),
            ]),
        );
        let b = score_case(
            &case("open-page", "fetch_page", None, false),
            3,
            0.8,
            1.0,
            bench_report(vec![
                selected("fetch_page", json!({})),
                selected("fetch_page", json!({})),
                selected("fetch_page", json!({})),
            ]),
        );
        RunReport {
            model_id: "gpt-4o".into(),
            provider_label: "openai",
            api_model: "gpt-4o".into(),
            reported_version: Some("gpt-mock-1".into()),
            runs_override: None,
            temp_override: None,
            gate: Some(0.8),
            system_prompt: bench::BENCH_SYSTEM_PROMPT,
            endpoint: bench::provider_endpoint(Provider::OpenAI, None),
            keyless: false,
            suites: vec![SuiteReport {
                name: "search-basics".into(),
                source: ".jig/search.yaml".into(),
                cases: vec![a, b],
            }],
        }
    }

    #[test]
    fn human_report_passing_snapshot() {
        insta::assert_snapshot!("eval_human_pass", render_human(&passing_report()));
    }

    #[test]
    fn human_report_mixed_snapshot() {
        insta::assert_snapshot!("eval_human_mixed", render_human(&mixed_report()));
    }

    #[test]
    fn junit_report_mixed_snapshot() {
        insta::assert_snapshot!("eval_junit_mixed", render_junit(&mixed_report()));
    }

    #[test]
    fn mixed_run_verdict_is_fail() {
        let report = mixed_report();
        // The failing must_pass case + gate-below-threshold both fail the run.
        assert!(!report.passed());
        assert_eq!(report.cases_passed(), 2); // clean pass + flaky pass
        assert_eq!(report.cases_flaky(), 1);
        assert_eq!(report.cases_errored(), 1);
    }
}
