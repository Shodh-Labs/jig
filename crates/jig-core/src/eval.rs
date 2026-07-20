//! The **`.jig` eval suite**: regression tests for an MCP server's *model
//! behavior*, versioned in git next to the server.
//!
//! Where [the bench engine](mod@crate::bench) is an exploratory microscope — "what does a
//! model do with this task?" — `eval` is the assertion layer built on top of
//! it: a YAML suite pins the *expected* tool selection (and, optionally, the
//! arguments) for each task, and the runner executes N bench runs per case and
//! scores them.
//!
//! # Design principles (non-negotiable)
//!
//! * **Statistical honesty.** A case is never one run and never a boolean. Each
//!   case runs N times (default [`DEFAULT_RUNS`]) and is scored by a *selection
//!   rate* against a per-case `min_rate`. A case that is *mostly* right is a
//!   [`CaseVerdict::Fail`] with its rate shown, not a silent pass; a case that
//!   flips between runs is flagged [`CaseReport::flaky`] even when it passes —
//!   flakiness is a finding, not noise.
//! * **Deterministic scoring only (v1).** Every gate is a mechanical matcher
//!   ([`Matcher`]): `exact`, `contains`, `regex`, `one_of`, `range`, plus the
//!   tool name and JSON-Schema validity of the selected arguments (reusing
//!   [`bench::validate_args`]). There is deliberately **no** LLM-judge.
//! * **Everything pinnable is pinned.** The model id and its *reported* version,
//!   temperature, N, the suite files, and the system prompt all ride in the
//!   report so a run is reproducible.
//!
//! # The format
//!
//! ```yaml
//! suite: search-basics          # optional; defaults to the file stem
//! defaults:                     # optional per-suite defaults
//!   runs: 3
//!   temp: 1.0
//! cases:
//!   - id: find-rate-limits      # required, unique within the suite
//!     task: "Find the docs page about rate limits"
//!     expect:
//!       tool: search_docs                     # required expected selection
//!       args:                                 # optional arg matchers
//!         query: { contains: "rate limit" }
//!         limit: { range: { min: 1, max: 50 } }
//!       not_tools: [fetch_page]               # selecting any of these = hard fail
//!     runs: 5                                 # overrides the default
//!     min_rate: 0.8                           # selection-rate gate (default 0.8)
//!     must_pass: false                        # true = its failure fails the run
//! ```
//!
//! A bare scalar is `exact` shorthand: `query: "rate limit"` ≡
//! `query: { exact: "rate limit" }`. Any unknown field is a hard error naming
//! the field and file (`serde` `deny_unknown_fields`) — a silently-ignored typo
//! in a test file is a lying test.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::bench::{self, ArgCheck, BenchConfig, BenchModel, BenchReport, Outcome};
use crate::protocol::Tool;

/// Default number of runs per case when neither the case, the suite defaults,
/// nor a CLI override specifies one.
pub const DEFAULT_RUNS: usize = 3;
/// Default sampling temperature when neither suite defaults nor a CLI override
/// specifies one.
pub const DEFAULT_TEMP: f64 = 1.0;
/// Default per-case selection-rate gate. Deliberately **not** 1.0: MCP
/// tool-selection is probabilistic, so demanding a perfect rate by default would
/// make almost every honest suite flaky. A case that needs certainty sets
/// `min_rate: 1.0` (or `must_pass: true`) explicitly.
pub const DEFAULT_MIN_RATE: f64 = 0.8;

// ---------------------------------------------------------------------------
// Format types
// ---------------------------------------------------------------------------

/// A parsed `.jig` suite (one YAML file).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    /// The suite name. Defaults to the file stem when omitted.
    #[serde(default)]
    pub suite: Option<String>,
    /// Per-suite defaults applied to every case that does not override them.
    #[serde(default)]
    pub defaults: Defaults,
    /// The cases in this suite (required, at least the key must be present).
    pub cases: Vec<Case>,
    /// The source label (file path or `<inline>`), filled in after parsing so it
    /// can appear in reports and errors. Not part of the YAML.
    #[serde(skip)]
    pub source: String,
}

impl Suite {
    /// The effective suite name (never empty once loaded via
    /// [`load_suite_str`]/[`load_suite_file`]).
    pub fn name(&self) -> &str {
        self.suite.as_deref().unwrap_or("suite")
    }
}

/// Per-suite defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Default runs per case.
    #[serde(default)]
    pub runs: Option<usize>,
    /// Default temperature.
    #[serde(default)]
    pub temp: Option<f64>,
}

/// A single eval case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Unique id within the suite.
    pub id: String,
    /// The natural-language task handed to the model.
    pub task: String,
    /// The expected behavior.
    pub expect: Expect,
    /// Override the runs for this case.
    #[serde(default)]
    pub runs: Option<usize>,
    /// The per-case selection-rate gate (default [`DEFAULT_MIN_RATE`]).
    #[serde(default)]
    pub min_rate: Option<f64>,
    /// When `true`, this case failing fails the whole run regardless of the
    /// `--gate` math.
    #[serde(default)]
    pub must_pass: bool,
}

impl Case {
    /// The effective per-case `min_rate` (falls back to [`DEFAULT_MIN_RATE`]).
    pub fn min_rate(&self) -> f64 {
        self.min_rate.unwrap_or(DEFAULT_MIN_RATE)
    }
}

/// What a case expects the model to do.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The tool the model is expected to select (required).
    pub tool: String,
    /// Optional per-argument matchers, keyed by argument name.
    #[serde(default)]
    pub args: Option<BTreeMap<String, Matcher>>,
    /// Known-wrong selections: selecting any of these on *any* run is a hard
    /// fail regardless of the selection rate.
    #[serde(default)]
    pub not_tools: Vec<String>,
}

/// An argument matcher. Deserializes from either a bare scalar (the `exact`
/// shorthand) or a single-key mapping naming the matcher kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Matcher {
    /// The value must equal this JSON value exactly.
    Exact(Value),
    /// The value must be a string containing this substring.
    Contains(String),
    /// The value must be a string matching this (pre-validated) regex.
    Regex(String),
    /// The value must equal one of these JSON values.
    OneOf(Vec<Value>),
    /// The value must be a number within `[min, max]` (each bound optional).
    Range {
        /// Inclusive lower bound.
        min: Option<f64>,
        /// Inclusive upper bound.
        max: Option<f64>,
    },
}

impl Matcher {
    /// Interpret a raw JSON value as a matcher. A mapping must be a single known
    /// matcher key; anything else is `exact` shorthand.
    fn from_value(v: Value) -> Result<Matcher, String> {
        let Value::Object(map) = v else {
            // Scalar / array / null → exact shorthand.
            return Ok(Matcher::Exact(v));
        };
        if map.len() != 1 {
            return Err(format!(
                "a matcher takes exactly one of exact/contains/regex/one_of/range, found {} keys",
                map.len()
            ));
        }
        let (key, val) = map.into_iter().next().expect("len == 1");
        match key.as_str() {
            "exact" => Ok(Matcher::Exact(val)),
            "contains" => val
                .as_str()
                .map(|s| Matcher::Contains(s.to_string()))
                .ok_or_else(|| "`contains` requires a string".to_string()),
            "regex" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| "`regex` requires a string".to_string())?;
                // Validate at load time so a bad pattern fails fast with the
                // file/line, not silently at scoring time.
                regex::Regex::new(s).map_err(|e| format!("invalid regex /{s}/: {e}"))?;
                Ok(Matcher::Regex(s.to_string()))
            }
            "one_of" => val
                .as_array()
                .map(|a| Matcher::OneOf(a.clone()))
                .ok_or_else(|| "`one_of` requires an array".to_string()),
            "range" => {
                let obj = val
                    .as_object()
                    .ok_or_else(|| "`range` requires an object with min/max".to_string())?;
                for k in obj.keys() {
                    if k != "min" && k != "max" {
                        return Err(format!("unknown range field '{k}' (expected min, max)"));
                    }
                }
                let min = obj.get("min").and_then(Value::as_f64);
                let max = obj.get("max").and_then(Value::as_f64);
                if min.is_none() && max.is_none() {
                    return Err("`range` needs at least one of min, max".to_string());
                }
                Ok(Matcher::Range { min, max })
            }
            other => Err(format!(
                "unknown matcher '{other}'; expected one of exact, contains, regex, one_of, range"
            )),
        }
    }

    /// Whether `value` satisfies this matcher. **Total** over arbitrary JSON —
    /// never panics.
    pub fn matches(&self, value: &Value) -> bool {
        match self {
            Matcher::Exact(v) => value == v,
            Matcher::Contains(s) => value.as_str().is_some_and(|t| t.contains(s.as_str())),
            Matcher::Regex(p) => value.as_str().is_some_and(|t| {
                // Pre-validated at load; recompile defensively (never panics).
                regex::Regex::new(p)
                    .map(|re| re.is_match(t))
                    .unwrap_or(false)
            }),
            Matcher::OneOf(vs) => vs.iter().any(|v| v == value),
            Matcher::Range { min, max } => match value.as_f64() {
                Some(n) => min.is_none_or(|lo| n >= lo) && max.is_none_or(|hi| n <= hi),
                None => false,
            },
        }
    }

    /// A short human description for failure messages.
    pub fn describe(&self) -> String {
        match self {
            Matcher::Exact(v) => format!("exact {}", compact(v)),
            Matcher::Contains(s) => format!("contains {s:?}"),
            Matcher::Regex(p) => format!("regex /{p}/"),
            Matcher::OneOf(vs) => {
                let items: Vec<String> = vs.iter().map(compact).collect();
                format!("one_of [{}]", items.join(", "))
            }
            Matcher::Range { min, max } => {
                let lo = min.map(|m| m.to_string()).unwrap_or_default();
                let hi = max.map(|m| m.to_string()).unwrap_or_default();
                format!("range({lo}..{hi})")
            }
        }
    }
}

impl<'de> Deserialize<'de> for Matcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = Value::deserialize(deserializer)?;
        Matcher::from_value(v).map_err(serde::de::Error::custom)
    }
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Loading & validation
// ---------------------------------------------------------------------------

/// An error loading, validating, or running an eval suite.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// The YAML failed to parse or violated the schema (unknown field, bad
    /// matcher, wrong type). The message carries the `line`/`column`.
    #[error("{source_file}: {message}")]
    Parse {
        /// The file (or `<inline>`) the error came from.
        source_file: String,
        /// The serde/`serde_yaml_ng` message, including `at line L column C`.
        message: String,
    },
    /// Two cases in one suite share an id.
    #[error("{source_file}: duplicate case id '{id}' — case ids must be unique within a suite")]
    DuplicateId {
        /// The offending file.
        source_file: String,
        /// The duplicated id.
        id: String,
    },
    /// A suite file could not be read.
    #[error("failed to read {path}: {detail}")]
    Io {
        /// The path attempted.
        path: String,
        /// The underlying I/O error.
        detail: String,
    },
    /// The bench engine failed to assemble a request (e.g. HTTP client build).
    #[error("bench engine error: {0}")]
    Bench(String),
}

/// Parse a suite from a YAML string, labeling errors with `source`.
///
/// Fills in the default suite name (from `source`) and enforces unique case
/// ids. Regex matchers are compiled here so a bad pattern fails fast.
///
/// # Errors
///
/// [`EvalError::Parse`] on malformed/invalid YAML (including an unknown field,
/// with `line`/`column`), or [`EvalError::DuplicateId`] on a repeated case id.
pub fn load_suite_str(yaml: &str, source: &str) -> Result<Suite, EvalError> {
    let mut suite: Suite = serde_yaml_ng::from_str(yaml).map_err(|e| EvalError::Parse {
        source_file: source.to_string(),
        message: e.to_string(),
    })?;
    suite.source = source.to_string();
    if suite.suite.is_none() {
        suite.suite = Some(default_suite_name(source));
    }
    let mut seen = HashSet::new();
    for case in &suite.cases {
        if !seen.insert(case.id.clone()) {
            return Err(EvalError::DuplicateId {
                source_file: source.to_string(),
                id: case.id.clone(),
            });
        }
    }
    Ok(suite)
}

/// Parse a suite from a file on disk.
///
/// # Errors
///
/// [`EvalError::Io`] if the file cannot be read, or any error from
/// [`load_suite_str`].
pub fn load_suite_file(path: &Path) -> Result<Suite, EvalError> {
    let text = std::fs::read_to_string(path).map_err(|e| EvalError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    load_suite_str(&text, &path.display().to_string())
}

/// Derive the default suite name from a source label: the file stem, or
/// `inline` for the `<inline>` sentinel.
fn default_suite_name(source: &str) -> String {
    let stem = Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("suite");
    if stem.is_empty() || stem == "<inline>" {
        "inline".to_string()
    } else {
        stem.to_string()
    }
}

// ---------------------------------------------------------------------------
// Runner configuration
// ---------------------------------------------------------------------------

/// Configuration for a full eval run (one model over one or more suites).
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// The resolved target model.
    pub model: BenchModel,
    /// The API key (read from env by the caller; never logged/serialized).
    pub api_key: String,
    /// CLI `--runs-override`: forces N for every case when set.
    pub runs_override: Option<usize>,
    /// CLI `--temp`: forces the temperature for every suite when set.
    pub temp_override: Option<f64>,
    /// CLI `--gate`: overall weighted-accuracy threshold in `0..=1`.
    pub gate: Option<f64>,
    /// Per-request timeout.
    pub timeout: Option<Duration>,
    /// `max_tokens` for each bench request.
    pub max_tokens: u32,
    /// Provider base URL override (e.g. the mock provider in tests).
    pub base_url: Option<String>,
}

impl EvalConfig {
    /// The effective runs for a case: CLI override, else case, else suite
    /// default, else [`DEFAULT_RUNS`] (clamped to at least 1).
    fn effective_runs(&self, suite: &Suite, case: &Case) -> usize {
        self.runs_override
            .or(case.runs)
            .or(suite.defaults.runs)
            .unwrap_or(DEFAULT_RUNS)
            .max(1)
    }

    /// The effective temperature: CLI override, else suite default, else
    /// [`DEFAULT_TEMP`].
    fn effective_temp(&self, suite: &Suite) -> f64 {
        self.temp_override
            .or(suite.defaults.temp)
            .unwrap_or(DEFAULT_TEMP)
    }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// The verdict for one case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseVerdict {
    /// Selection rate met the case's `min_rate`.
    Pass,
    /// Selection rate fell below `min_rate`.
    Fail,
    /// A `not_tools` (known-wrong) selection occurred on at least one run.
    NotTools,
    /// More than half of the runs ended in a provider error — the case could
    /// not be evaluated.
    Errored,
}

impl CaseVerdict {
    /// Whether this verdict counts as a pass.
    pub fn is_pass(self) -> bool {
        matches!(self, CaseVerdict::Pass)
    }

    /// A short uppercase label.
    pub fn label(self) -> &'static str {
        match self {
            CaseVerdict::Pass => "PASS",
            CaseVerdict::Fail => "FAIL",
            CaseVerdict::NotTools => "FAIL (not_tools)",
            CaseVerdict::Errored => "ERRORED",
        }
    }
}

/// The compact per-run detail behind a case's rate.
#[derive(Debug, Clone)]
pub struct RunDetail {
    /// 1-based run index.
    pub index: usize,
    /// Whether this run counted as a pass (right tool, valid args, all matchers).
    pub passed: bool,
    /// Whether this run ended in a provider error (excluded from the rate).
    pub is_provider_error: bool,
    /// Whether this run selected a `not_tools` (known-wrong) tool.
    pub is_not_tool: bool,
    /// The outcome taxonomy tag (`selected`, `no_tool`, …).
    pub outcome_tag: &'static str,
    /// The selected tool, if any.
    pub selected_tool: Option<String>,
    /// A compact human reason (why it did or did not pass).
    pub summary: String,
}

/// The scored result for one case.
#[derive(Debug, Clone)]
pub struct CaseReport {
    /// The case id.
    pub id: String,
    /// The task text.
    pub task: String,
    /// The expected tool.
    pub expected_tool: String,
    /// Effective runs executed.
    pub runs: usize,
    /// Effective `min_rate` gate.
    pub min_rate: f64,
    /// Effective temperature.
    pub temperature: f64,
    /// Whether this case is `must_pass`.
    pub must_pass: bool,
    /// Runs that counted as a pass.
    pub passes: usize,
    /// Runs that ended in a provider error (excluded from the denominator).
    pub provider_errors: usize,
    /// Runs that selected a known-wrong (`not_tools`) tool.
    pub not_tools_hits: usize,
    /// Runs counted toward the rate (`runs - provider_errors`).
    pub counted: usize,
    /// Selection rate = `passes / counted`; `None` when nothing counted.
    pub rate: Option<f64>,
    /// The case verdict.
    pub verdict: CaseVerdict,
    /// Whether the case flipped between runs (a finding even when passing).
    pub flaky: bool,
    /// Per-run detail.
    pub run_details: Vec<RunDetail>,
    /// The full underlying bench report (for `--json`).
    pub bench: BenchReport,
}

impl CaseReport {
    /// A `passes/counted` fraction string, e.g. `4/5`.
    pub fn rate_fraction(&self) -> String {
        format!("{}/{}", self.passes, self.counted)
    }

    /// The rate as a rounded percentage string, or `n/a` when nothing counted.
    pub fn rate_pct(&self) -> String {
        match self.rate {
            Some(r) => format!("{:.0}%", r * 100.0),
            None => "n/a".to_string(),
        }
    }
}

/// One suite's scored cases.
#[derive(Debug, Clone)]
pub struct SuiteReport {
    /// The suite name.
    pub name: String,
    /// The suite source label (file path or `<inline>`).
    pub source: String,
    /// The scored cases, in file order.
    pub cases: Vec<CaseReport>,
}

/// The full report for one eval run.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Canonical model id.
    pub model_id: String,
    /// Provider label (`anthropic` / `openai`).
    pub provider_label: &'static str,
    /// Concrete API model string sent on the wire.
    pub api_model: String,
    /// The model version the provider reported (first seen), if any.
    pub reported_version: Option<String>,
    /// The CLI `--runs-override`, if any (for the pinned-context block).
    pub runs_override: Option<usize>,
    /// The CLI `--temp` override, if any.
    pub temp_override: Option<f64>,
    /// The `--gate` threshold, if any.
    pub gate: Option<f64>,
    /// The minimal bench system prompt (pinned for reproducibility).
    pub system_prompt: &'static str,
    /// The exact endpoint every case was sent to — the vendor default, or the
    /// `--base-url` an OpenAI-compatible endpoint was reached at.
    pub endpoint: String,
    /// Whether the run sent no credential at all (`--no-auth`).
    pub keyless: bool,
    /// The suites, in the order given.
    pub suites: Vec<SuiteReport>,
}

impl RunReport {
    /// Every case across every suite.
    pub fn cases(&self) -> impl Iterator<Item = &CaseReport> {
        self.suites.iter().flat_map(|s| s.cases.iter())
    }

    /// Total passing runs across all cases.
    pub fn total_passes(&self) -> usize {
        self.cases().map(|c| c.passes).sum()
    }

    /// Total counted runs across all cases (provider errors excluded).
    pub fn total_counted(&self) -> usize {
        self.cases().map(|c| c.counted).sum()
    }

    /// Overall weighted selection accuracy = total passes / total counted runs
    /// (weighted by each case's counted runs). `None` when nothing counted.
    pub fn overall_accuracy(&self) -> Option<f64> {
        let counted = self.total_counted();
        (counted > 0).then(|| self.total_passes() as f64 / counted as f64)
    }

    /// Count of cases with a passing verdict.
    pub fn cases_passed(&self) -> usize {
        self.cases().filter(|c| c.verdict.is_pass()).count()
    }

    /// Count of cases that failed (`Fail` or `NotTools`).
    pub fn cases_failed(&self) -> usize {
        self.cases()
            .filter(|c| matches!(c.verdict, CaseVerdict::Fail | CaseVerdict::NotTools))
            .count()
    }

    /// Count of cases that errored out.
    pub fn cases_errored(&self) -> usize {
        self.cases()
            .filter(|c| c.verdict == CaseVerdict::Errored)
            .count()
    }

    /// Count of cases flagged flaky (regardless of pass/fail).
    pub fn cases_flaky(&self) -> usize {
        self.cases().filter(|c| c.flaky).count()
    }

    /// The `must_pass` cases that did not pass.
    pub fn must_pass_failures(&self) -> Vec<&CaseReport> {
        self.cases()
            .filter(|c| c.must_pass && !c.verdict.is_pass())
            .collect()
    }

    /// Whether the `--gate` (if any) is met by the overall accuracy.
    pub fn gate_met(&self) -> bool {
        match self.gate {
            None => true,
            Some(g) => self.overall_accuracy().is_some_and(|a| a >= g),
        }
    }

    /// The run verdict: all `must_pass` cases pass **and** the gate is met.
    ///
    /// Note: without a `--gate` and without any `must_pass` case there is
    /// nothing to enforce, so the run passes (it is informational). Add a gate
    /// or mark cases `must_pass` to turn an eval into a CI regression gate.
    pub fn passed(&self) -> bool {
        self.must_pass_failures().is_empty() && self.gate_met()
    }
}

// ---------------------------------------------------------------------------
// Scoring (pure — no network)
// ---------------------------------------------------------------------------

/// Score one case's [`BenchReport`] against its [`Expect`]. Pure: no network,
/// fully deterministic, so it is directly unit- and snapshot-testable.
pub fn score_case(
    case: &Case,
    runs: usize,
    min_rate: f64,
    temperature: f64,
    report: BenchReport,
) -> CaseReport {
    let expect = &case.expect;
    let not_tools: HashSet<&str> = expect.not_tools.iter().map(String::as_str).collect();

    let mut passes = 0usize;
    let mut provider_errors = 0usize;
    let mut not_tools_hits = 0usize;
    let mut details = Vec::with_capacity(report.results.len());

    for r in &report.results {
        let mut passed = false;
        let mut is_provider_error = false;
        let mut is_not_tool = false;
        let outcome_tag = r.outcome.tag();
        let mut selected_tool = None;
        let summary;

        match &r.outcome {
            Outcome::Selected {
                tool,
                arguments,
                args_check,
            } => {
                selected_tool = Some(tool.clone());
                if not_tools.contains(tool.as_str()) {
                    is_not_tool = true;
                    summary = format!("selected known-wrong tool `{tool}`");
                } else if tool != &expect.tool {
                    summary = format!("selected `{tool}` (expected `{}`)", expect.tool);
                } else if let Some(reason) = arg_failure(args_check, expect, arguments) {
                    summary = format!("selected `{tool}` but {reason}");
                } else {
                    passed = true;
                    summary = format!("selected `{tool}`");
                }
            }
            Outcome::NoTool { .. } => {
                summary = format!("no tool selected (expected `{}`)", expect.tool);
            }
            Outcome::HallucinatedTool { name, .. } => {
                summary = format!("hallucinated `{name}` (expected `{}`)", expect.tool);
            }
            Outcome::ProviderError { detail } => {
                is_provider_error = true;
                summary = format!("provider error: {}", truncate(detail, 80));
            }
        }

        if passed {
            passes += 1;
        }
        if is_provider_error {
            provider_errors += 1;
        }
        if is_not_tool {
            not_tools_hits += 1;
        }
        details.push(RunDetail {
            index: r.index,
            passed,
            is_provider_error,
            is_not_tool,
            outcome_tag,
            selected_tool,
            summary,
        });
    }

    let counted = runs.saturating_sub(provider_errors);
    let rate = (counted > 0).then(|| passes as f64 / counted as f64);
    let failures = counted.saturating_sub(passes);
    let flaky = passes > 0 && failures > 0;

    // Verdict precedence: a known-wrong selection is a hard fail; then a
    // provider-error majority errors the case; otherwise the rate decides.
    let verdict = if not_tools_hits > 0 {
        CaseVerdict::NotTools
    } else if provider_errors * 2 > runs {
        CaseVerdict::Errored
    } else {
        match rate {
            Some(r) if r >= min_rate => CaseVerdict::Pass,
            Some(_) => CaseVerdict::Fail,
            None => CaseVerdict::Errored,
        }
    };

    CaseReport {
        id: case.id.clone(),
        task: case.task.clone(),
        expected_tool: expect.tool.clone(),
        runs,
        min_rate,
        temperature,
        must_pass: case.must_pass,
        passes,
        provider_errors,
        not_tools_hits,
        counted,
        rate,
        verdict,
        flaky,
        run_details: details,
        bench: report,
    }
}

/// Return `Some(reason)` when a selected call's arguments fail schema validation
/// or a matcher; `None` when they pass. Schema-validity is always checked (even
/// with no matchers), because a schema-invalid call is a real defect.
fn arg_failure(args_check: &ArgCheck, expect: &Expect, arguments: &Value) -> Option<String> {
    match args_check {
        ArgCheck::Invalid { errors } => {
            return Some(format!(
                "args invalid: {}",
                errors.first().cloned().unwrap_or_default()
            ));
        }
        ArgCheck::Unparseable { detail } => {
            return Some(format!("args unparseable: {detail}"));
        }
        ArgCheck::Valid => {}
    }
    if let Some(matchers) = &expect.args {
        for (name, matcher) in matchers {
            let actual = arguments.get(name).cloned().unwrap_or(Value::Null);
            if !matcher.matches(&actual) {
                return Some(format!(
                    "arg '{name}' ({}) did not match {}",
                    matcher.describe(),
                    compact(&actual)
                ));
            }
        }
    }
    None
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
// The live runner
// ---------------------------------------------------------------------------

/// Run every case in every suite against the model, reusing the bench engine.
///
/// `tools` is the server's already-listed tool surface (listed once by the
/// caller and shared across all cases). Each case builds a [`BenchConfig`] from
/// the shared config plus its own task/runs/temp and is scored by
/// [`score_case`].
///
/// # Errors
///
/// [`EvalError::Bench`] if the bench engine cannot assemble a request. A
/// misbehaving provider does **not** error here — it degrades into
/// provider-error runs that the scorer accounts for.
pub async fn run_eval(
    tools: &[Tool],
    config: &EvalConfig,
    suites: &[Suite],
) -> Result<RunReport, EvalError> {
    let mut suite_reports = Vec::with_capacity(suites.len());
    for suite in suites {
        let mut cases = Vec::with_capacity(suite.cases.len());
        for case in &suite.cases {
            cases.push(run_case(tools, config, suite, case).await?);
        }
        suite_reports.push(SuiteReport {
            name: suite.name().to_string(),
            source: suite.source.clone(),
            cases,
        });
    }

    let reported_version = suite_reports
        .iter()
        .flat_map(|s| s.cases.iter())
        .flat_map(|c| c.bench.results.iter())
        .find_map(|r| r.model_version.clone());

    Ok(RunReport {
        model_id: config.model.id.clone(),
        provider_label: config.model.provider.label(),
        api_model: config.model.api_model.clone(),
        reported_version,
        runs_override: config.runs_override,
        temp_override: config.temp_override,
        gate: config.gate,
        system_prompt: bench::BENCH_SYSTEM_PROMPT,
        endpoint: bench::provider_endpoint(config.model.provider, config.base_url.as_deref()),
        keyless: config.api_key.is_empty(),
        suites: suite_reports,
    })
}

async fn run_case(
    tools: &[Tool],
    config: &EvalConfig,
    suite: &Suite,
    case: &Case,
) -> Result<CaseReport, EvalError> {
    let runs = config.effective_runs(suite, case);
    let temperature = config.effective_temp(suite);
    let bench_config = BenchConfig {
        model: config.model.clone(),
        task: case.task.clone(),
        runs,
        temperature,
        max_tokens: config.max_tokens,
        timeout: config.timeout,
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
    };
    let report = bench::run_bench(tools, &bench_config)
        .await
        .map_err(|e| EvalError::Bench(e.to_string()))?;
    Ok(score_case(case, runs, case.min_rate(), temperature, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{Provider, RunResult, Usage};
    use serde_json::json;

    // ---- format parsing ----------------------------------------------------

    const VALID: &str = r#"
suite: search-basics
defaults:
  runs: 3
  temp: 0.5
cases:
  - id: find-rate-limits
    task: "Find the docs page about rate limits"
    expect:
      tool: search_docs
      args:
        query: { contains: "rate limit" }
        limit: { range: { min: 1, max: 50 } }
        mode: { one_of: ["fast", "deep"] }
        slug: { regex: "^[a-z-]+$" }
        exactish: hello
      not_tools: [fetch_page]
    runs: 5
    min_rate: 0.8
    must_pass: true
"#;

    #[test]
    fn parses_a_valid_suite_with_every_matcher() {
        let suite = load_suite_str(VALID, "search.yaml").expect("valid");
        assert_eq!(suite.name(), "search-basics");
        assert_eq!(suite.defaults.runs, Some(3));
        assert_eq!(suite.cases.len(), 1);
        let case = &suite.cases[0];
        assert_eq!(case.id, "find-rate-limits");
        assert!(case.must_pass);
        assert_eq!(case.runs, Some(5));
        let args = case.expect.args.as_ref().unwrap();
        assert!(matches!(args["query"], Matcher::Contains(_)));
        assert!(matches!(args["limit"], Matcher::Range { .. }));
        assert!(matches!(args["mode"], Matcher::OneOf(_)));
        assert!(matches!(args["slug"], Matcher::Regex(_)));
        assert!(matches!(args["exactish"], Matcher::Exact(_)));
        assert_eq!(case.expect.not_tools, vec!["fetch_page".to_string()]);
    }

    #[test]
    fn bare_scalar_is_exact_shorthand() {
        let y = r#"
cases:
  - id: c
    task: t
    expect:
      tool: echo
      args:
        text: "hello world"
        n: 3
"#;
        let suite = load_suite_str(y, "s.yaml").unwrap();
        let args = suite.cases[0].expect.args.as_ref().unwrap();
        assert_eq!(args["text"], Matcher::Exact(json!("hello world")));
        assert_eq!(args["n"], Matcher::Exact(json!(3)));
        // Suite name defaults to the file stem.
        assert_eq!(suite.name(), "s");
    }

    #[test]
    fn unknown_field_is_an_error_naming_field_and_file() {
        let y = r#"
cases:
  - id: c
    task: t
    expct:
      tool: echo
"#;
        let err = load_suite_str(y, "typo.yaml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("typo.yaml"), "{msg}");
        assert!(
            msg.contains("expct") || msg.contains("unknown field"),
            "{msg}"
        );
        // The location rides along.
        assert!(msg.contains("line"), "{msg}");
    }

    #[test]
    fn unknown_matcher_key_is_an_error() {
        let y = r#"
cases:
  - id: c
    task: t
    expect:
      tool: echo
      args:
        q: { containz: "x" }
"#;
        let err = load_suite_str(y, "m.yaml").unwrap_err();
        assert!(err.to_string().contains("unknown matcher"), "{err}");
    }

    #[test]
    fn bad_regex_fails_at_load() {
        let y = r#"
cases:
  - id: c
    task: t
    expect:
      tool: echo
      args:
        q: { regex: "(" }
"#;
        let err = load_suite_str(y, "r.yaml").unwrap_err();
        assert!(err.to_string().contains("regex"), "{err}");
    }

    #[test]
    fn duplicate_case_id_is_an_error() {
        let y = r#"
cases:
  - id: dup
    task: a
    expect: { tool: echo }
  - id: dup
    task: b
    expect: { tool: echo }
"#;
        let err = load_suite_str(y, "d.yaml").unwrap_err();
        match err {
            EvalError::DuplicateId { id, source_file } => {
                assert_eq!(id, "dup");
                assert_eq!(source_file, "d.yaml");
            }
            other => panic!("expected duplicate id, got {other}"),
        }
    }

    #[test]
    fn missing_required_field_is_an_error() {
        let y = r#"
cases:
  - id: c
    expect: { tool: echo }
"#; // no `task`
        let err = load_suite_str(y, "x.yaml").unwrap_err();
        assert!(err.to_string().contains("task"), "{err}");
    }

    // ---- matcher unit tests ------------------------------------------------

    #[test]
    fn matcher_exact() {
        assert!(Matcher::Exact(json!("a")).matches(&json!("a")));
        assert!(!Matcher::Exact(json!("a")).matches(&json!("b")));
        assert!(Matcher::Exact(json!(3)).matches(&json!(3)));
    }

    #[test]
    fn matcher_contains_only_strings() {
        assert!(Matcher::Contains("rate".into()).matches(&json!("rate limit")));
        assert!(!Matcher::Contains("rate".into()).matches(&json!("nope")));
        assert!(!Matcher::Contains("rate".into()).matches(&json!(42)));
    }

    #[test]
    fn matcher_regex() {
        let m = Matcher::Regex("^[a-z-]+$".into());
        assert!(m.matches(&json!("rate-limits")));
        assert!(!m.matches(&json!("Rate")));
        assert!(!m.matches(&json!(3)));
    }

    #[test]
    fn matcher_one_of() {
        let m = Matcher::OneOf(vec![json!("a"), json!(2)]);
        assert!(m.matches(&json!("a")));
        assert!(m.matches(&json!(2)));
        assert!(!m.matches(&json!("b")));
    }

    #[test]
    fn matcher_range_inclusive_and_open() {
        let m = Matcher::Range {
            min: Some(1.0),
            max: Some(50.0),
        };
        assert!(m.matches(&json!(1)));
        assert!(m.matches(&json!(50)));
        assert!(!m.matches(&json!(0)));
        assert!(!m.matches(&json!(51)));
        assert!(!m.matches(&json!("x")));
        let open = Matcher::Range {
            min: Some(10.0),
            max: None,
        };
        assert!(open.matches(&json!(1000)));
        assert!(!open.matches(&json!(9)));
    }

    // ---- scoring -----------------------------------------------------------

    fn report_over(outcomes: Vec<Outcome>) -> BenchReport {
        let results = outcomes
            .into_iter()
            .enumerate()
            .map(|(i, o)| RunResult {
                index: i + 1,
                outcome: o,
                latency_ms: 0,
                usage: Usage::default(),
                model_version: Some("mock-1".into()),
                raw_response: Value::Null,
            })
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
            server_tool_names: vec!["search_docs".into()],
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

    fn case_expecting(tool: &str, not_tools: Vec<&str>) -> Case {
        Case {
            id: "c".into(),
            task: "t".into(),
            expect: Expect {
                tool: tool.into(),
                args: None,
                not_tools: not_tools.into_iter().map(String::from).collect(),
            },
            runs: None,
            min_rate: None,
            must_pass: false,
        }
    }

    #[test]
    fn scoring_passing_case() {
        let report = report_over(vec![
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
        ]);
        let c = score_case(&case_expecting("search_docs", vec![]), 3, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Pass);
        assert_eq!(c.passes, 3);
        assert_eq!(c.rate, Some(1.0));
        assert!(!c.flaky);
    }

    #[test]
    fn scoring_flaky_but_passing() {
        // 4/5 selects, one no_tool: rate 0.8 == min_rate → pass, but flaky.
        let report = report_over(vec![
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            Outcome::NoTool {
                excerpt: "x".into(),
            },
        ]);
        let c = score_case(&case_expecting("search_docs", vec![]), 5, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Pass);
        assert!(c.flaky, "one differing run must flag flaky");
    }

    #[test]
    fn scoring_wrong_tool_fails() {
        let report = report_over(vec![selected("fetch_page", json!({}))]);
        let c = score_case(&case_expecting("search_docs", vec![]), 1, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Fail);
        assert_eq!(c.passes, 0);
    }

    #[test]
    fn scoring_not_tools_is_hard_fail_even_if_rate_high() {
        // 2 correct, 1 not_tools hit → hard fail regardless of the 2/3 rate.
        let report = report_over(vec![
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            selected("fetch_page", json!({})),
        ]);
        let case = case_expecting("search_docs", vec!["fetch_page"]);
        let c = score_case(&case, 3, 0.5, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::NotTools);
        assert_eq!(c.not_tools_hits, 1);
    }

    #[test]
    fn scoring_provider_errors_excluded_and_majority_errors_case() {
        // 3 of 3 provider errors → counted 0 → Errored.
        let report = report_over(vec![
            Outcome::ProviderError {
                detail: "HTTP 500".into(),
            },
            Outcome::ProviderError {
                detail: "HTTP 500".into(),
            },
            Outcome::ProviderError {
                detail: "HTTP 500".into(),
            },
        ]);
        let c = score_case(&case_expecting("search_docs", vec![]), 3, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Errored);
        assert_eq!(c.counted, 0);
        assert_eq!(c.rate, None);
    }

    #[test]
    fn scoring_one_provider_error_is_excluded_not_fatal() {
        // 2 pass, 1 provider error → counted 2, rate 1.0 → pass.
        let report = report_over(vec![
            selected("search_docs", json!({})),
            selected("search_docs", json!({})),
            Outcome::ProviderError {
                detail: "HTTP 429".into(),
            },
        ]);
        let c = score_case(&case_expecting("search_docs", vec![]), 3, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Pass);
        assert_eq!(c.counted, 2);
        assert_eq!(c.provider_errors, 1);
        assert_eq!(c.rate, Some(1.0));
    }

    #[test]
    fn scoring_matcher_and_schema_gates() {
        // Right tool, but the matcher on `query` fails → not a pass.
        let mut case = case_expecting("search_docs", vec![]);
        case.expect.args = Some(
            [("query".to_string(), Matcher::Contains("rate".into()))]
                .into_iter()
                .collect(),
        );
        let report = report_over(vec![selected("search_docs", json!({ "query": "weather" }))]);
        let c = score_case(&case, 1, 0.8, 1.0, report);
        assert_eq!(c.verdict, CaseVerdict::Fail);
        assert!(
            c.run_details[0].summary.contains("did not match"),
            "{}",
            c.run_details[0].summary
        );
    }

    // ---- run verdict / gate ------------------------------------------------

    fn run_report_with(cases: Vec<CaseReport>, gate: Option<f64>) -> RunReport {
        RunReport {
            model_id: "gpt-4o".into(),
            provider_label: "openai",
            api_model: "gpt-4o".into(),
            reported_version: Some("mock-1".into()),
            runs_override: None,
            temp_override: None,
            gate,
            system_prompt: bench::BENCH_SYSTEM_PROMPT,
            endpoint: bench::provider_endpoint(Provider::OpenAI, None),
            keyless: false,
            suites: vec![SuiteReport {
                name: "s".into(),
                source: "s.yaml".into(),
                cases,
            }],
        }
    }

    fn scored(
        tool_selected: &str,
        expect: &str,
        runs: usize,
        min_rate: f64,
        must_pass: bool,
    ) -> CaseReport {
        let outcomes = (0..runs)
            .map(|_| selected(tool_selected, json!({})))
            .collect();
        let report = report_over(outcomes);
        let mut case = case_expecting(expect, vec![]);
        case.must_pass = must_pass;
        score_case(&case, runs, min_rate, 1.0, report)
    }

    #[test]
    fn run_passes_when_gate_met() {
        let report = run_report_with(
            vec![scored("search_docs", "search_docs", 3, 0.8, false)],
            Some(0.8),
        );
        assert_eq!(report.overall_accuracy(), Some(1.0));
        assert!(report.gate_met());
        assert!(report.passed());
    }

    #[test]
    fn run_fails_when_gate_not_met() {
        // A failing case drags accuracy to 0 < gate 0.8.
        let report = run_report_with(
            vec![scored("fetch_page", "search_docs", 3, 0.8, false)],
            Some(0.8),
        );
        assert_eq!(report.overall_accuracy(), Some(0.0));
        assert!(!report.gate_met());
        assert!(!report.passed(), "gate not met must fail the run");
    }

    #[test]
    fn must_pass_case_failing_fails_run_even_without_gate() {
        let report = run_report_with(
            vec![scored("fetch_page", "search_docs", 3, 0.8, true)],
            None,
        );
        assert!(report.gate_met(), "no gate is trivially met");
        assert_eq!(report.must_pass_failures().len(), 1);
        assert!(!report.passed(), "a failing must_pass case fails the run");
    }

    #[test]
    fn no_gate_no_must_pass_is_informational_pass() {
        let report = run_report_with(
            vec![scored("fetch_page", "search_docs", 3, 0.8, false)],
            None,
        );
        // The case failed, but with no gate and no must_pass there is nothing to
        // enforce, so the run passes (informational).
        assert_eq!(report.cases_failed(), 1);
        assert!(report.passed());
    }
}
