//! The **model-in-the-loop bench**: give a natural-language task and observe
//! which tool a *real* model selects from a live MCP server's tool surface,
//! with what arguments, across repeated runs.
//!
//! This is Jig's flagship differentiator. Where [`budget`](crate::tokens) prices
//! a server's tool surface statically, `bench` makes the *probabilistic* nature
//! of MCP integration visible and measurable: it assembles a genuine tool-use
//! API request for the target provider (the server's tools mapped to the
//! provider's function-calling format plus the task as the user message), sends
//! it N times, and classifies each response into the outcome taxonomy
//! ([`Outcome`]).
//!
//! # Honesty contract
//!
//! Everything that shapes the result is inspectable. The exact request body we
//! send is captured verbatim in [`BenchReport::rendered_request`] (it carries no
//! auth — the API key rides only in a request *header*, never the body). The
//! minimal system prompt is a single documented constant,
//! [`BENCH_SYSTEM_PROMPT`], echoed in every report. The model's exact version
//! string, the temperature, N, and every raw provider response are all recorded.
//! Jig never reports a bare "pass".
//!
//! # Providers
//!
//! Two dialects are supported, verified against the current provider docs:
//!
//! * **Anthropic Messages API** — `POST /v1/messages`, `tools` array of
//!   `{name, description, input_schema}`, `tool_choice`
//!   `{type: auto, disable_parallel_tool_use: true}`; the response carries
//!   `stop_reason: "tool_use"` and `tool_use` content blocks
//!   `{type, id, name, input}`, with `usage.{input_tokens, output_tokens}`.
//! * **OpenAI Chat Completions** — `POST /v1/chat/completions`, `tools` array of
//!   `{type: function, function: {name, description, parameters}}`; the response
//!   carries `finish_reason: "tool_calls"` and
//!   `message.tool_calls[].function.{name, arguments}` — where `arguments` is a
//!   JSON **string** the model emits and may malform — with
//!   `usage.{prompt_tokens, completion_tokens, total_tokens}`.
//!
//! # Keys
//!
//! API keys are read from the environment by the caller (`ANTHROPIC_API_KEY` /
//! `OPENAI_API_KEY`) and passed in [`BenchConfig::api_key`]. They are never
//! logged, never placed in [`BenchReport::rendered_request`], and redacted from
//! any provider text before it is stored (see [`redact`]).
//!
//! A key is not always required. [`BenchConfig::base_url`] points the bench at
//! any **OpenAI-compatible** endpoint — Ollama, LM Studio, llama.cpp, vLLM, or a
//! company gateway — and [`BenchConfig::api_key`] may then be empty, in which
//! case no `Authorization` header is sent at all. Every report records the exact
//! endpoint it talked to and whether a vendor key was used
//! ([`BenchReport::endpoint`] / [`BenchReport::keyless`]), so a keyless run is
//! never mistaken for a vendor-API run.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::protocol::Tool;

/// The **minimal, documented** system prompt Jig sends with every bench request.
///
/// It is deliberately neutral and short: enough to frame the tool-selection
/// task without steering the model toward any particular tool. Because the
/// system prompt is part of the methodology, this exact string is echoed in
/// every [`BenchReport`] (and `--json` output) so the measurement is never a
/// black box.
pub const BENCH_SYSTEM_PROMPT: &str = "You are connected to a set of tools. Read the user's task \
    and, if one of the available tools is appropriate, call exactly one tool to accomplish it. If \
    no tool fits, answer in plain text.";

/// Default `max_tokens` for a bench request. The Anthropic Messages API requires
/// `max_tokens`; a small cap is plenty for a single tool call and keeps cost
/// bounded. Recorded in every report.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Maximum number of send attempts for one run before it is classified as
/// [`Outcome::ProviderError`] (rather than crashing).
const MAX_ATTEMPTS: u32 = 3;

/// Upper bound on how long a single `Retry-After` back-off will sleep, so a
/// hostile or confused provider cannot stall a bench indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The excerpt length captured from a text-only ([`Outcome::NoTool`]) answer.
const NO_TOOL_EXCERPT_CHARS: usize = 400;

/// Errors from assembling or resolving a bench request (not per-run outcomes —
/// a misbehaving provider degrades into an [`Outcome::ProviderError`], which is
/// data, not an error).
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// The requested model id did not resolve to a known bench model.
    #[error("unknown model '{model}' for bench (known: {known})")]
    UnknownModel {
        /// The unrecognized model id.
        model: String,
        /// Comma-separated list of known model ids.
        known: String,
    },
    /// The HTTP client could not be constructed.
    #[error("failed to build HTTP client: {0}")]
    Client(String),
}

/// A model provider dialect. Determines request/response shape and default
/// endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic Messages API (`tools` / `tool_use` blocks).
    Anthropic,
    /// OpenAI Chat Completions (`tools` / `tool_calls`).
    OpenAI,
}

impl Provider {
    /// The environment variable this provider's key is read from.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
        }
    }

    /// The default API base URL (no trailing slash).
    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAI => "https://api.openai.com",
        }
    }

    /// The request path appended to the base URL.
    pub fn path(self) -> &'static str {
        match self {
            Provider::Anthropic => "/v1/messages",
            Provider::OpenAI => "/v1/chat/completions",
        }
    }

    /// A short human label.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
        }
    }
}

/// A resolved bench model: the canonical id Jig exposes, its provider, and the
/// concrete API model string to send on the wire.
#[derive(Debug, Clone)]
pub struct BenchModel {
    /// Canonical model id (post alias-resolution), e.g. `claude-sonnet`.
    pub id: String,
    /// The provider dialect.
    pub provider: Provider,
    /// The concrete API model string sent to the provider, e.g.
    /// `claude-sonnet-4-5`.
    pub api_model: String,
}

impl BenchModel {
    /// Resolve `model_id` (id or alias) against the shared model registry.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::UnknownModel`] if the id is not in the registry.
    pub fn resolve(model_id: &str) -> Result<Self, BenchError> {
        crate::tokens::bench_model_spec(model_id)
            .map(|(id, provider, api_model)| BenchModel {
                id: id.to_string(),
                provider,
                api_model: api_model.to_string(),
            })
            .ok_or_else(|| BenchError::UnknownModel {
                model: model_id.to_string(),
                known: crate::tokens::known_models().join(", "),
            })
    }

    /// Build a model with an explicit `--api-model` override, keeping the
    /// resolved id/provider but swapping the concrete API string. Hardcoded
    /// mappings age; this is the escape hatch.
    pub fn with_api_model(mut self, api_model: String) -> Self {
        self.api_model = api_model;
        self
    }
}

/// Configuration for one bench run set (one model, N sends).
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// The resolved target model.
    pub model: BenchModel,
    /// The natural-language task, sent as the user message.
    pub task: String,
    /// Number of times to send the request (default 3).
    pub runs: usize,
    /// Sampling temperature, always recorded (default 1.0).
    pub temperature: f64,
    /// `max_tokens` for the response.
    pub max_tokens: u32,
    /// Per-request timeout. `None` waits indefinitely.
    pub timeout: Option<Duration>,
    /// Override the provider base URL — an OpenAI-compatible endpoint (Ollama,
    /// LM Studio, llama.cpp, vLLM, a company gateway) or a mock provider in
    /// tests. `None` uses [`Provider::default_base_url`].
    pub base_url: Option<String>,
    /// The API key — read from env by the caller. Never logged or serialized.
    ///
    /// **Empty means keyless**: no `x-api-key` / `Authorization` header is sent.
    /// That is only sensible against a [`BenchConfig::base_url`] that needs no
    /// credential; the caller is responsible for that pairing.
    pub api_key: String,
}

impl BenchConfig {
    /// Whether this run sends no credential at all (an empty
    /// [`BenchConfig::api_key`]).
    pub fn is_keyless(&self) -> bool {
        self.api_key.is_empty()
    }
}

/// How a selected call's arguments fared against the tool's JSON Schema.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgCheck {
    /// Arguments conform to the (subset of) JSON Schema Jig validates.
    Valid,
    /// Arguments parsed as JSON but violated the schema (types, required, enum).
    Invalid {
        /// One human-readable message per violation.
        errors: Vec<String>,
    },
    /// The model emitted arguments that were not even valid JSON (an OpenAI
    /// `arguments` string that failed to parse). Real models do this; Jig
    /// records it rather than panicking.
    Unparseable {
        /// The parse-failure detail.
        detail: String,
    },
}

impl ArgCheck {
    /// A short tag for tables: `valid`, `INVALID`, or `unparseable`.
    pub fn tag(&self) -> &'static str {
        match self {
            ArgCheck::Valid => "valid",
            ArgCheck::Invalid { .. } => "INVALID",
            ArgCheck::Unparseable { .. } => "unparseable",
        }
    }

    /// Whether the arguments are valid.
    pub fn is_valid(&self) -> bool {
        matches!(self, ArgCheck::Valid)
    }
}

/// The outcome taxonomy for a single run.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The model called a tool the server exposes.
    Selected {
        /// The tool name.
        tool: String,
        /// The arguments the model supplied (parsed JSON, or a JSON string when
        /// the model's `arguments` were unparseable).
        arguments: Value,
        /// Argument validation against the tool's schema.
        args_check: ArgCheck,
    },
    /// The model answered in text without calling any tool.
    NoTool {
        /// A short excerpt of the text answer.
        excerpt: String,
    },
    /// The model called a tool name the server does **not** expose.
    HallucinatedTool {
        /// The hallucinated name.
        name: String,
        /// The arguments the model supplied (raw).
        arguments: Value,
    },
    /// An API-level failure after bounded retries (rate limit, 5xx, transport).
    ProviderError {
        /// A redacted, human-readable failure detail.
        detail: String,
    },
}

impl Outcome {
    /// A short taxonomy tag: `selected`, `no_tool`, `hallucinated_tool`, or
    /// `provider_error`.
    pub fn tag(&self) -> &'static str {
        match self {
            Outcome::Selected { .. } => "selected",
            Outcome::NoTool { .. } => "no_tool",
            Outcome::HallucinatedTool { .. } => "hallucinated_tool",
            Outcome::ProviderError { .. } => "provider_error",
        }
    }
}

/// Token usage a provider reported for one run, if any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Input/prompt tokens.
    pub input_tokens: Option<u64>,
    /// Output/completion tokens.
    pub output_tokens: Option<u64>,
    /// Total tokens (OpenAI reports this directly; for Anthropic it is the sum).
    pub total_tokens: Option<u64>,
}

/// The result of a single run.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// 1-based run index.
    pub index: usize,
    /// The classified outcome.
    pub outcome: Outcome,
    /// Wall-clock latency of the send (all attempts), in milliseconds.
    pub latency_ms: u128,
    /// Token usage, if the provider reported it.
    pub usage: Usage,
    /// The exact model version string from the response, if present.
    pub model_version: Option<String>,
    /// The raw provider response (redacted), for full-fidelity `--json` output.
    /// `Null` when no response body was obtained (transport failure).
    pub raw_response: Value,
}

/// The full report for one model's bench run set.
#[derive(Debug, Clone)]
pub struct BenchReport {
    /// Canonical model id.
    pub model_id: String,
    /// Provider dialect.
    pub provider: Provider,
    /// Concrete API model string sent on the wire.
    pub api_model: String,
    /// Sampling temperature.
    pub temperature: f64,
    /// `max_tokens` used.
    pub max_tokens: u32,
    /// Number of runs.
    pub runs: usize,
    /// The minimal system prompt constant used ([`BENCH_SYSTEM_PROMPT`]).
    pub system_prompt: &'static str,
    /// The exact request body sent every run (auth-free — the key is a header).
    pub rendered_request: Value,
    /// Per-run results, in order.
    pub results: Vec<RunResult>,
    /// The tool names the server exposed, for reference.
    pub server_tool_names: Vec<String>,
    /// The exact endpoint URL the runs were sent to. Reported verbatim so a
    /// `--base-url` run is never confused with a vendor-API run.
    pub endpoint: String,
    /// Whether the run sent **no** credential (`--no-auth` against a local or
    /// self-hosted endpoint). Recorded so output can say so plainly.
    pub keyless: bool,
}

impl BenchReport {
    /// Aggregate the per-run outcomes into a distribution.
    pub fn distribution(&self) -> Distribution {
        distribution_of(&self.results)
    }
}

/// Aggregate any slice of run results into a [`Distribution`].
///
/// Free-standing so every bench-shaped runner — the direct provider API path
/// ([`run_bench`]) and the MCP-sampling path alike — produces the *same*
/// distribution from the *same* code, rather than each rolling its own tally.
pub fn distribution_of(results: &[RunResult]) -> Distribution {
    let mut selected: Vec<(String, usize)> = Vec::new();
    let mut hallucinated: Vec<(String, usize)> = Vec::new();
    let mut no_tool = 0usize;
    let mut provider_error = 0usize;

    let bump = |v: &mut Vec<(String, usize)>, name: &str| {
        if let Some(entry) = v.iter_mut().find(|(n, _)| n == name) {
            entry.1 += 1;
        } else {
            v.push((name.to_string(), 1));
        }
    };

    for r in results {
        match &r.outcome {
            Outcome::Selected { tool, .. } => bump(&mut selected, tool),
            Outcome::HallucinatedTool { name, .. } => bump(&mut hallucinated, name),
            Outcome::NoTool { .. } => no_tool += 1,
            Outcome::ProviderError { .. } => provider_error += 1,
        }
    }

    // Deterministic order: descending count, ties by name ascending.
    let sort = |v: &mut Vec<(String, usize)>| {
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    };
    sort(&mut selected);
    sort(&mut hallucinated);

    Distribution {
        selected,
        hallucinated,
        no_tool,
        provider_error,
        total: results.len(),
    }
}

/// The aggregated outcome distribution across all runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Distribution {
    /// Selected tools with counts, sorted descending by count then name.
    pub selected: Vec<(String, usize)>,
    /// Hallucinated tool names with counts, same sort.
    pub hallucinated: Vec<(String, usize)>,
    /// Count of text-only answers.
    pub no_tool: usize,
    /// Count of provider errors.
    pub provider_error: usize,
    /// Total runs.
    pub total: usize,
}

impl Distribution {
    /// Whether the selection was consistent: every run selected a tool and they
    /// were all the same tool. A single-outcome distribution with any
    /// no-tool/hallucinated/error mixed in is *not* consistent.
    pub fn is_consistent(&self) -> bool {
        self.selected.len() == 1
            && self.selected[0].1 == self.total
            && self.hallucinated.is_empty()
            && self.no_tool == 0
            && self.provider_error == 0
    }

    /// A one-line takeaway summarizing stability.
    pub fn takeaway(&self) -> String {
        if self.total == 0 {
            return "no runs".to_string();
        }
        if self.is_consistent() {
            return format!(
                "consistent: `{}` on all {} run{}",
                self.selected[0].0,
                self.total,
                plural(self.total)
            );
        }
        // Count how many distinct *kinds* of outcome occurred.
        let distinct_selections = self.selected.len();
        if distinct_selections > 1 {
            return format!(
                "UNSTABLE: tool selection varied across runs ({} different tools) — see per-run detail",
                distinct_selections
            );
        }
        // One-or-zero selected tools but mixed with other outcomes.
        let mut parts = Vec::new();
        if !self.selected.is_empty() {
            parts.push(format!(
                "{} selected `{}`",
                self.selected[0].1, self.selected[0].0
            ));
        }
        if self.no_tool > 0 {
            parts.push(format!("{} answered without a tool", self.no_tool));
        }
        if !self.hallucinated.is_empty() {
            let n: usize = self.hallucinated.iter().map(|(_, c)| c).sum();
            parts.push(format!("{n} hallucinated a tool"));
        }
        if self.provider_error > 0 {
            parts.push(format!("{} provider error(s)", self.provider_error));
        }
        format!("UNSTABLE: {}", parts.join(", "))
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
// Request rendering (pure — snapshot- and property-testable)
// ---------------------------------------------------------------------------

/// Render the Anthropic Messages API request body for `tools` + `task`.
///
/// The server's tools map to Anthropic's `{name, description, input_schema}`
/// shape; the task becomes the sole user message; the minimal system prompt
/// rides in `system`. `tool_choice` is `{type: auto,
/// disable_parallel_tool_use: true}` so at most one tool is requested per turn.
pub fn render_anthropic_request(
    tools: &[Tool],
    task: &str,
    api_model: &str,
    temperature: f64,
    max_tokens: u32,
) -> Value {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(t.name));
            if let Some(d) = &t.description {
                m.insert("description".to_string(), json!(d));
            }
            m.insert("input_schema".to_string(), input_schema_or_empty(t));
            Value::Object(m)
        })
        .collect();

    json!({
        "model": api_model,
        "max_tokens": max_tokens,
        "temperature": temperature,
        "system": BENCH_SYSTEM_PROMPT,
        "tools": tools_json,
        "tool_choice": { "type": "auto", "disable_parallel_tool_use": true },
        "messages": [ { "role": "user", "content": task } ],
    })
}

/// Render the OpenAI Chat Completions request body for `tools` + `task`.
///
/// Each tool maps to `{type: function, function: {name, description,
/// parameters}}`; the system prompt and task are `system`/`user` messages;
/// `tool_choice` is `"auto"`.
pub fn render_openai_request(
    tools: &[Tool],
    task: &str,
    api_model: &str,
    temperature: f64,
) -> Value {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut func = Map::new();
            func.insert("name".to_string(), json!(t.name));
            if let Some(d) = &t.description {
                func.insert("description".to_string(), json!(d));
            }
            func.insert("parameters".to_string(), input_schema_or_empty(t));
            json!({ "type": "function", "function": Value::Object(func) })
        })
        .collect();

    json!({
        "model": api_model,
        "temperature": temperature,
        "tools": tools_json,
        "tool_choice": "auto",
        "messages": [
            { "role": "system", "content": BENCH_SYSTEM_PROMPT },
            { "role": "user", "content": task },
        ],
    })
}

/// A tool's `input_schema`, or a minimal empty-object schema when the server
/// omitted one (both providers require an object schema per tool).
fn input_schema_or_empty(tool: &Tool) -> Value {
    if tool.input_schema.is_object() {
        tool.input_schema.clone()
    } else {
        json!({ "type": "object", "properties": {} })
    }
}

/// Render the request body for a provider from explicit parts, with **no**
/// [`BenchConfig`] and no API key.
///
/// This is the key-free assembly `jig context` reuses to show the exact body
/// `jig bench` sends without ever executing a request or touching a key.
/// [`render_request`] delegates here so the bench send-path and the `context`
/// rendering can never drift.
pub fn render_request_parts(
    provider: Provider,
    tools: &[Tool],
    task: &str,
    api_model: &str,
    temperature: f64,
    max_tokens: u32,
) -> Value {
    match provider {
        Provider::Anthropic => {
            render_anthropic_request(tools, task, api_model, temperature, max_tokens)
        }
        Provider::OpenAI => render_openai_request(tools, task, api_model, temperature),
    }
}

/// Render the request body for either provider (dispatch helper).
pub fn render_request(provider: Provider, config: &BenchConfig, tools: &[Tool]) -> Value {
    render_request_parts(
        provider,
        tools,
        &config.task,
        &config.model.api_model,
        config.temperature,
        config.max_tokens,
    )
}

// ---------------------------------------------------------------------------
// Response classification (pure — property- and snapshot-testable)
// ---------------------------------------------------------------------------

/// The classification of one successful provider response: the outcome, the
/// model version string, and usage. Kept separate from HTTP so it can be
/// exercised over arbitrary JSON without a network.
#[derive(Debug, Clone)]
pub struct Classified {
    /// The classified outcome.
    pub outcome: Outcome,
    /// The exact model version string, if the response carried one.
    pub model_version: Option<String>,
    /// Token usage, if reported.
    pub usage: Usage,
}

/// Classify an Anthropic Messages API response. Total over arbitrary JSON:
/// never panics.
pub fn classify_anthropic(resp: &Value, server_tools: &HashSet<String>) -> Classified {
    let model_version = resp
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = Usage {
        input_tokens: usage_u64(resp, "usage", "input_tokens"),
        output_tokens: usage_u64(resp, "usage", "output_tokens"),
        total_tokens: None,
    };
    let usage = Usage {
        total_tokens: match (usage.input_tokens, usage.output_tokens) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        },
        ..usage
    };

    // Find the first tool_use content block.
    let tool_use = resp
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));

    let outcome = if let Some(block) = tool_use {
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let arguments = block.get("input").cloned().unwrap_or(json!({}));
        classify_tool_call(name, arguments, server_tools)
    } else {
        // No tool call: gather the text blocks as the answer excerpt.
        let text = anthropic_text(resp);
        Outcome::NoTool {
            excerpt: excerpt(&text),
        }
    };

    Classified {
        outcome,
        model_version,
        usage,
    }
}

/// Classify an OpenAI Chat Completions response. Total over arbitrary JSON.
pub fn classify_openai(resp: &Value, server_tools: &HashSet<String>) -> Classified {
    let model_version = resp
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = Usage {
        input_tokens: usage_u64(resp, "usage", "prompt_tokens"),
        output_tokens: usage_u64(resp, "usage", "completion_tokens"),
        total_tokens: usage_u64(resp, "usage", "total_tokens"),
    };

    let message = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"));

    // First tool call, if any.
    let tool_call = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .and_then(|calls| calls.first());

    let outcome = if let Some(call) = tool_call {
        let func = call.get("function");
        let name = func
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // OpenAI arguments are a JSON *string* the model emits — parse
        // defensively. A malformed string is args-Unparseable, never a panic.
        let raw_args = func
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match serde_json::from_str::<Value>(raw_args) {
            Ok(parsed) => classify_tool_call(name, parsed, server_tools),
            Err(e) => {
                if server_tools.contains(&name) {
                    Outcome::Selected {
                        tool: name,
                        arguments: Value::String(raw_args.to_string()),
                        args_check: ArgCheck::Unparseable {
                            detail: format!("model emitted non-JSON arguments: {e}"),
                        },
                    }
                } else {
                    Outcome::HallucinatedTool {
                        name,
                        arguments: Value::String(raw_args.to_string()),
                    }
                }
            }
        }
    } else {
        let text = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        Outcome::NoTool {
            excerpt: excerpt(text),
        }
    };

    Classified {
        outcome,
        model_version,
        usage,
    }
}

/// Fill in the real argument validation on a [`Outcome::Selected`] outcome, now
/// that the tool schemas are in hand.
///
/// The classifiers are pure over a response body and cannot see the server's
/// schemas, so they leave [`ArgCheck::Valid`] as a placeholder. Every runner
/// calls this exactly once per classified outcome; sharing it is what keeps the
/// direct-API path and the MCP-sampling path from drifting apart on what
/// "valid arguments" means. Outcomes that are not a selection, and selections
/// whose arguments were already found unparseable, are left untouched.
pub fn finalize_args_check(tools: &[Tool], outcome: &mut Outcome) {
    let Outcome::Selected {
        tool,
        arguments,
        args_check,
    } = outcome
    else {
        return;
    };
    if *args_check != ArgCheck::Valid {
        return;
    }
    if let Some(schema) = tools
        .iter()
        .find(|t| &t.name == tool)
        .map(|t| &t.input_schema)
    {
        *args_check = validate_args(schema, arguments);
    }
}

/// Turn a resolved tool name + arguments into a Selected or HallucinatedTool
/// outcome, validating args when the tool is real.
fn classify_tool_call(name: String, arguments: Value, server_tools: &HashSet<String>) -> Outcome {
    if server_tools.contains(&name) {
        Outcome::Selected {
            tool: name,
            args_check: ArgCheck::Valid, // placeholder; filled by run() with the schema
            arguments,
        }
    } else {
        Outcome::HallucinatedTool { name, arguments }
    }
}

/// Concatenate an Anthropic response's `text` content blocks.
fn anthropic_text(resp: &Value) -> String {
    resp.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn usage_u64(resp: &Value, obj: &str, key: &str) -> Option<u64> {
    resp.get(obj)
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
}

/// A short, single-line excerpt of a text answer.
fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= NO_TOOL_EXCERPT_CHARS {
        return flat;
    }
    let mut out: String = flat
        .chars()
        .take(NO_TOOL_EXCERPT_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Argument validation (a small, dependency-free JSON Schema subset)
// ---------------------------------------------------------------------------

/// Validate `args` against a tool's JSON Schema, checking the subset that
/// matters for tool-call correctness.
///
/// **Checks:** the top-level type is an object; every `required` property is
/// present; each present property whose schema names a `type` has a matching
/// JSON type (`string`/`number`/`integer`/`boolean`/`object`/`array`/`null`);
/// `enum` membership; and nested `object` properties recursively.
///
/// **Does not check:** `format`, numeric bounds (`minimum`/`maximum`),
/// string/array length, `pattern`, `additionalProperties`, `anyOf`/`oneOf`/
/// `allOf`, or `$ref`. This is a purpose-built validator, not a full JSON
/// Schema implementation — enough to catch the mistakes models actually make
/// (missing required field, wrong type, bad enum value) without a heavyweight
/// dependency.
pub fn validate_args(schema: &Value, args: &Value) -> ArgCheck {
    let mut errors = Vec::new();
    validate_object(schema, args, "", &mut errors);
    if errors.is_empty() {
        ArgCheck::Valid
    } else {
        ArgCheck::Invalid { errors }
    }
}

fn validate_object(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    // Only object schemas are validated structurally; a non-object schema (rare
    // at the top level) is treated as "anything goes".
    let Some(obj_schema) = schema.as_object() else {
        return;
    };
    let declared_type = obj_schema.get("type").and_then(Value::as_str);
    if declared_type == Some("object") || obj_schema.contains_key("properties") {
        let Some(map) = value.as_object() else {
            errors.push(format!(
                "{}: expected an object, got {}",
                at(path),
                json_type_name(value)
            ));
            return;
        };
        // Required fields.
        if let Some(required) = obj_schema.get("required").and_then(Value::as_array) {
            for req in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(req) {
                    errors.push(format!("{}: missing required field '{req}'", at(path)));
                }
            }
        }
        // Per-property checks.
        if let Some(props) = obj_schema.get("properties").and_then(Value::as_object) {
            for (name, prop_schema) in props {
                if let Some(v) = map.get(name) {
                    let child = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    validate_value(prop_schema, v, &child, errors);
                }
            }
        }
    }
}

fn validate_value(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(obj_schema) = schema.as_object() else {
        return;
    };

    // Enum membership takes precedence: if declared, the value must be one of
    // them (type is implied by the members).
    if let Some(variants) = obj_schema.get("enum").and_then(Value::as_array) {
        if !variants.iter().any(|v| v == value) {
            let allowed: Vec<String> = variants.iter().map(compact).collect();
            errors.push(format!(
                "{}: value {} is not one of [{}]",
                at(path),
                compact(value),
                allowed.join(", ")
            ));
        }
        return;
    }

    match obj_schema.get("type").and_then(Value::as_str) {
        Some("object") => validate_object(schema, value, path, errors),
        Some("array") => {
            if let Some(items) = value.as_array() {
                if let Some(item_schema) = obj_schema.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        validate_value(item_schema, item, &format!("{path}[{i}]"), errors);
                    }
                }
            } else {
                type_error(value, "array", path, errors);
            }
        }
        Some("string") => {
            if !value.is_string() {
                type_error(value, "string", path, errors);
            }
        }
        Some("integer") => {
            // JSON has no integer type; accept a whole number.
            let ok = value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false);
            if !ok {
                type_error(value, "integer", path, errors);
            }
        }
        Some("number") => {
            if !value.is_number() {
                type_error(value, "number", path, errors);
            }
        }
        Some("boolean") => {
            if !value.is_boolean() {
                type_error(value, "boolean", path, errors);
            }
        }
        Some("null") if !value.is_null() => {
            type_error(value, "null", path, errors);
        }
        // Null-and-is-null, unknown, or absent type: no structural check.
        _ => {}
    }
}

fn type_error(value: &Value, expected: &str, path: &str, errors: &mut Vec<String>) {
    errors.push(format!(
        "{}: expected {expected}, got {}",
        at(path),
        json_type_name(value)
    ));
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

fn at(path: &str) -> String {
    if path.is_empty() {
        "(root)".to_string()
    } else {
        path.to_string()
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Replace every occurrence of `secret` in `text` with `***`. Used so a
/// provider that echoes the key in an error body (or anywhere) never leaks it
/// into a report or error message. A blank secret is a no-op.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

/// Redact `secret` from every string anywhere inside a JSON value.
fn redact_value(value: Value, secret: &str) -> Value {
    if secret.is_empty() {
        return value;
    }
    match value {
        Value::String(s) => Value::String(s.replace(secret, "***")),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| redact_value(v, secret)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, redact_value(v, secret)))
                .collect(),
        ),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// The live runner (HTTP + bounded retry)
// ---------------------------------------------------------------------------

/// Run the bench: send the assembled request `config.runs` times, classifying
/// each response into the outcome taxonomy.
///
/// Sends are sequential. Each send retries on `429`/`5xx` up to a bounded
/// number of attempts (respecting `Retry-After` when present); on exhaustion the
/// run is recorded as [`Outcome::ProviderError`] rather than failing the whole
/// bench. A misbehaving provider is Jig's to degrade informatively — the same
/// discipline Jig applies to a misbehaving server.
pub async fn run_bench(tools: &[Tool], config: &BenchConfig) -> Result<BenchReport, BenchError> {
    let provider = config.model.provider;
    let server_tools: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    let rendered_request = render_request(provider, config, tools);

    let mut builder = reqwest::Client::builder();
    if let Some(dur) = config.timeout {
        builder = builder.timeout(dur);
    }
    let client = builder
        .build()
        .map_err(|e| BenchError::Client(e.to_string()))?;

    let endpoint = provider_endpoint(provider, config.base_url.as_deref());

    let mut results = Vec::with_capacity(config.runs);
    for index in 1..=config.runs {
        let started = Instant::now();
        let sent = send_with_retry(&client, provider, &endpoint, &rendered_request, config).await;
        let latency_ms = started.elapsed().as_millis();

        let result = match sent {
            Ok(resp_value) => {
                let resp_value = redact_value(resp_value, &config.api_key);
                let mut classified = match provider {
                    Provider::Anthropic => classify_anthropic(&resp_value, &server_tools),
                    Provider::OpenAI => classify_openai(&resp_value, &server_tools),
                };
                // Fill in real arg validation now that we have the schemas.
                finalize_args_check(tools, &mut classified.outcome);
                RunResult {
                    index,
                    outcome: classified.outcome,
                    latency_ms,
                    usage: classified.usage,
                    model_version: classified.model_version,
                    raw_response: resp_value,
                }
            }
            Err(detail) => RunResult {
                index,
                outcome: Outcome::ProviderError {
                    detail: redact(&detail, &config.api_key),
                },
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

    Ok(BenchReport {
        model_id: config.model.id.clone(),
        provider,
        api_model: config.model.api_model.clone(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        runs: config.runs,
        system_prompt: BENCH_SYSTEM_PROMPT,
        rendered_request,
        results,
        server_tool_names,
        endpoint,
        keyless: config.is_keyless(),
    })
}

/// Build the endpoint URL from the provider and an optional base-URL override.
///
/// Public because it is the single source of truth for "where did this bench
/// actually send its requests?" — the CLI reports it before a run so a user can
/// confirm the target before any traffic leaves the machine.
///
/// The join is **idempotent in the API version segment**. Every
/// OpenAI-compatible runtime documents its base URL *including* the version —
/// Ollama's is `http://localhost:11434/v1`, LM Studio's `http://localhost:1234/v1`
/// — while [`Provider::path`] also begins `/v1`. Naively concatenating produces
/// `…/v1/v1/chat/completions`, a 404 that looks like a broken server rather
/// than a fumbled URL. So a base already ending in the path's version segment
/// absorbs it, and both conventions reach the same endpoint:
///
/// ```
/// # use jig_core::bench::{provider_endpoint, Provider};
/// let with_v1 = provider_endpoint(Provider::OpenAI, Some("http://localhost:11434/v1"));
/// let bare = provider_endpoint(Provider::OpenAI, Some("http://localhost:11434"));
/// assert_eq!(with_v1, "http://localhost:11434/v1/chat/completions");
/// assert_eq!(with_v1, bare);
/// ```
pub fn provider_endpoint(provider: Provider, base_url: Option<&str>) -> String {
    let base = base_url
        .unwrap_or_else(|| provider.default_base_url())
        .trim_end_matches('/');
    let path = provider.path();
    // The leading segment of the path (`/v1`), if the base already carries it.
    let version = path
        .split('/')
        .nth(1)
        .map(|seg| format!("/{seg}"))
        .unwrap_or_default();
    if !version.is_empty() && base.ends_with(&version) {
        return format!("{}{}", base, &path[version.len()..]);
    }
    format!("{base}{path}")
}

/// One send with bounded retry. Returns the parsed JSON body on success, or a
/// human-readable (unredacted here — the caller redacts) failure detail.
async fn send_with_retry(
    client: &reqwest::Client,
    provider: Provider,
    endpoint: &str,
    body: &Value,
    config: &BenchConfig,
) -> Result<Value, String> {
    let mut last_detail = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        let mut req = client
            .post(endpoint)
            .json(body)
            .header("content-type", "application/json");
        // A blank key is the documented "keyless" mode: send no credential
        // header at all. Local runtimes (Ollama, LM Studio, llama.cpp, vLLM)
        // want no `Authorization`, and some reject a `Bearer ` with an empty
        // token outright — so omitting the header is the correct behaviour,
        // not a courtesy.
        if !config.api_key.is_empty() {
            req = match provider {
                Provider::Anthropic => req
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01"),
                Provider::OpenAI => {
                    req.header("authorization", format!("Bearer {}", config.api_key))
                }
            };
        } else if provider == Provider::Anthropic {
            // The version header is not a credential and the Messages API
            // requires it regardless.
            req = req.header("anthropic-version", "2023-06-01");
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp
                        .json::<Value>()
                        .await
                        .map_err(|e| format!("provider returned an unparseable body: {e}"));
                }
                // Retry on rate limit / server errors; give up on other 4xx.
                let retryable =
                    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                let retry_after = parse_retry_after(&resp);
                let snippet: String = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                last_detail = format!("HTTP {status}: {}", snippet.replace('\n', " "));
                if !retryable || attempt == MAX_ATTEMPTS {
                    return Err(last_detail);
                }
                let backoff = retry_after.unwrap_or_else(|| default_backoff(attempt));
                tokio::time::sleep(backoff.min(MAX_RETRY_AFTER)).await;
            }
            Err(e) => {
                last_detail = if e.is_timeout() {
                    format!("request timed out: {e}")
                } else if e.is_connect() {
                    format!("could not connect to provider at {endpoint}: {e}")
                } else {
                    format!("provider request failed: {e}")
                };
                // Transport errors are retried too (transient network blips).
                if attempt == MAX_ATTEMPTS {
                    return Err(last_detail);
                }
                tokio::time::sleep(default_backoff(attempt).min(MAX_RETRY_AFTER)).await;
            }
        }
    }
    Err(last_detail)
}

/// Parse a `Retry-After` header (delta-seconds form) into a duration.
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Exponential-ish default back-off when no `Retry-After` is supplied.
fn default_backoff(attempt: u32) -> Duration {
    Duration::from_millis(200 * (1u64 << (attempt.saturating_sub(1).min(4))))
}

// ---------------------------------------------------------------------------
// The MCP-sampling dialect (no credentials at all)
// ---------------------------------------------------------------------------

/// The response protocol appended to [`BENCH_SYSTEM_PROMPT`] when a bench runs
/// over MCP `sampling/createMessage` instead of a provider's function-calling
/// API.
///
/// # Why this exists
///
/// `sampling/createMessage` (MCP `2025-06-18`, *Client Features → Sampling*)
/// returns a **single content block** — `{role, content: {type, text}, model,
/// stopReason}`. It has no `tool_use` block and no `tool_calls` array, so the
/// model's selection cannot ride a structured channel the way it does on the
/// Anthropic and OpenAI dialects. The selection is therefore requested as a
/// JSON object in the text, and parsed back out by
/// [`classify_sampling_text`].
///
/// This is a real, documented difference from the direct-API path and is
/// labelled as such in every sampling-backed report. The *classification* that
/// follows is not forked: the parsed name and arguments are fed through the
/// same [`classify_anthropic`] path (hence the same hallucination check) and
/// the same [`finalize_args_check`] schema validation.
pub const SAMPLING_RESPONSE_PROTOCOL: &str = "\n\nAvailable tools are listed below as JSON. \
    Reply with a single JSON object and nothing else. To call a tool, reply \
    {\"tool\": \"<name>\", \"arguments\": { ... }}. If no tool fits, reply \
    {\"tool\": null, \"answer\": \"<your plain-text answer>\"}. Do not wrap the object in \
    prose or code fences.";

/// The model identity recorded when a sampling host reports no `model` string.
///
/// Sampling deliberately gives the server **no** control over model selection —
/// the client picks. Jig never guesses: a run whose host did not name its model
/// is recorded with this exact string rather than being attributed to whatever
/// model happened to be configured elsewhere.
pub const SAMPLING_MODEL_UNKNOWN: &str = "unknown — host-selected";

/// Build the `params` object of a `sampling/createMessage` request for a
/// bench run over the tools of an MCP server.
///
/// The shape follows MCP `2025-06-18` *Client Features → Sampling*: `messages`
/// (each `{role, content: {type: "text", text}}`), `systemPrompt`, `maxTokens`,
/// `temperature`, and `modelPreferences`.
///
/// `modelPreferences` deliberately carries **no `hints`**. Hints are the
/// mechanism for nudging a host toward a named model family; using them would
/// let a report imply a model that was never actually measured. Only the three
/// abstract priorities are sent, and the host's own choice is reported verbatim.
pub fn render_sampling_params(
    tools: &[Tool],
    task: &str,
    temperature: f64,
    max_tokens: u32,
) -> Value {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(t.name));
            if let Some(d) = &t.description {
                m.insert("description".to_string(), json!(d));
            }
            m.insert("inputSchema".to_string(), input_schema_or_empty(t));
            Value::Object(m)
        })
        .collect();

    let system = format!(
        "{BENCH_SYSTEM_PROMPT}{SAMPLING_RESPONSE_PROTOCOL}\n\nTools:\n{}",
        serde_json::to_string(&tools_json).unwrap_or_else(|_| "[]".to_string())
    );

    json!({
        "messages": [
            { "role": "user", "content": { "type": "text", "text": task } }
        ],
        "systemPrompt": system,
        "maxTokens": max_tokens,
        "temperature": temperature,
        "modelPreferences": {
            "costPriority": 0.3,
            "speedPriority": 0.5,
            "intelligencePriority": 0.8,
        },
    })
}

/// Classify the assistant text a host returned from `sampling/createMessage`
/// into the **existing** bench outcome taxonomy.
///
/// The text is expected to be the JSON object described by
/// [`SAMPLING_RESPONSE_PROTOCOL`]. Whatever it actually is, this is total and
/// never panics:
///
/// * a `{"tool": "<name>", "arguments": {...}}` object is rebuilt into the
///   Anthropic `tool_use` shape and run through [`classify_anthropic`], so an
///   unknown name lands as [`Outcome::HallucinatedTool`] by exactly the same
///   rule as on the direct-API path;
/// * `{"tool": null, ...}` is [`Outcome::NoTool`], carrying the `answer` when
///   one was supplied;
/// * text that is not the expected object at all is [`Outcome::NoTool`] with
///   the text as its excerpt — a model that ignored the protocol did not, in
///   fact, select a tool.
///
/// The returned [`Outcome::Selected`] still carries the placeholder
/// [`ArgCheck::Valid`]; the caller completes it with [`finalize_args_check`].
pub fn classify_sampling_text(text: &str, server_tools: &HashSet<String>) -> Outcome {
    let Some(obj) = extract_json_object(text) else {
        return Outcome::NoTool {
            excerpt: excerpt(text),
        };
    };

    match obj.get("tool").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => {
            let arguments = obj.get("arguments").cloned().unwrap_or_else(|| json!({}));
            // Route through the Anthropic classifier rather than re-deriving
            // "is this a real tool?" here — one rule, one place.
            let synthetic = json!({
                "content": [ { "type": "tool_use", "name": name, "input": arguments } ]
            });
            classify_anthropic(&synthetic, server_tools).outcome
        }
        _ => {
            let answer = obj.get("answer").and_then(Value::as_str).unwrap_or(text);
            Outcome::NoTool {
                excerpt: excerpt(answer),
            }
        }
    }
}

/// Pull the first balanced top-level JSON object out of `text`, tolerating the
/// code fences and surrounding prose real models add despite instructions.
/// `None` when nothing parses.
fn extract_json_object(text: &str) -> Option<Map<String, Value>> {
    // The common case: the whole (trimmed, de-fenced) text is the object.
    let trimmed = strip_code_fence(text.trim());
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(trimmed) {
        return Some(m);
    }
    // Otherwise scan for a balanced `{...}` span, respecting string literals so
    // a brace inside a quoted value does not throw the depth off.
    let bytes = trimmed.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return match serde_json::from_str::<Value>(&trimmed[start..=i]) {
                        Ok(Value::Object(m)) => Some(m),
                        _ => None,
                    };
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip a leading ```` ```json ```` / ```` ``` ```` fence and its closing
/// fence, if present.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // Drop the optional language tag on the opening fence line.
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    rest.trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim_end()
}

/// A bench report produced over MCP sampling rather than a provider API.
///
/// Deliberately *not* a [`BenchReport`]: that type's `model_id`/`provider`/
/// `api_model` fields assert facts a sampling run cannot know. The host chose
/// the model; all Jig can honestly record is what the host said it used, in
/// [`SamplingBenchReport::host_models`].
#[derive(Debug, Clone)]
pub struct SamplingBenchReport {
    /// The natural-language task that was benched.
    pub task: String,
    /// Number of `sampling/createMessage` requests issued.
    pub runs: usize,
    /// The requested sampling temperature. Advisory only — a host is free to
    /// ignore it, which is itself a reason this report never claims otherwise.
    pub temperature: f64,
    /// The requested `maxTokens`.
    pub max_tokens: u32,
    /// The minimal bench system prompt constant this run built on.
    pub system_prompt: &'static str,
    /// The exact `sampling/createMessage` params sent every run.
    pub rendered_request: Value,
    /// Per-run results, in order.
    pub results: Vec<RunResult>,
    /// The tool names the target server exposed.
    pub server_tool_names: Vec<String>,
    /// Every distinct model identity the host reported, in first-seen order.
    /// Contains [`SAMPLING_MODEL_UNKNOWN`] for runs the host did not name.
    pub host_models: Vec<String>,
}

impl SamplingBenchReport {
    /// Aggregate the per-run outcomes into a distribution — the *same*
    /// [`distribution_of`] the direct-API path uses, so the two are comparable.
    pub fn distribution(&self) -> Distribution {
        distribution_of(&self.results)
    }

    /// A one-line, non-overclaiming description of whose model was measured.
    pub fn model_label(&self) -> String {
        match self.host_models.len() {
            0 => SAMPLING_MODEL_UNKNOWN.to_string(),
            1 => self.host_models[0].clone(),
            _ => format!(
                "{} (host varied the model across runs)",
                self.host_models.join(", ")
            ),
        }
    }
}

/// Collect the distinct model identities across run results, in first-seen
/// order, substituting [`SAMPLING_MODEL_UNKNOWN`] where the host named none.
pub fn host_models_of(results: &[RunResult]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in results {
        let id = r
            .model_version
            .clone()
            .unwrap_or_else(|| SAMPLING_MODEL_UNKNOWN.to_string());
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, schema: Value) -> Tool {
        serde_json::from_value(json!({ "name": name, "inputSchema": schema })).unwrap()
    }

    fn server_tools(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ---- request rendering --------------------------------------------------

    #[test]
    fn anthropic_request_shape_matches_docs() {
        let tools = vec![tool(
            "search_docs",
            json!({ "type": "object", "properties": { "q": { "type": "string" } }, "required": ["q"] }),
        )];
        let req = render_anthropic_request(&tools, "find rate limits", "claude-x", 0.7, 512);
        assert_eq!(req["model"], "claude-x");
        assert_eq!(req["max_tokens"], 512);
        assert_eq!(req["temperature"], 0.7);
        assert_eq!(req["system"], BENCH_SYSTEM_PROMPT);
        assert_eq!(req["tool_choice"]["type"], "auto");
        assert_eq!(req["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(req["tools"][0]["name"], "search_docs");
        assert_eq!(req["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(req["messages"][0]["role"], "user");
        assert_eq!(req["messages"][0]["content"], "find rate limits");
    }

    #[test]
    fn openai_request_shape_matches_docs() {
        let tools = vec![tool(
            "search_docs",
            json!({ "type": "object", "properties": {} }),
        )];
        let req = render_openai_request(&tools, "find rate limits", "gpt-x", 1.0);
        assert_eq!(req["model"], "gpt-x");
        assert_eq!(req["tool_choice"], "auto");
        assert_eq!(req["tools"][0]["type"], "function");
        assert_eq!(req["tools"][0]["function"]["name"], "search_docs");
        assert_eq!(req["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(req["messages"][0]["role"], "system");
        assert_eq!(req["messages"][0]["content"], BENCH_SYSTEM_PROMPT);
        assert_eq!(req["messages"][1]["role"], "user");
    }

    // ---- Anthropic classification ------------------------------------------

    #[test]
    fn anthropic_tool_use_is_selected() {
        let resp = json!({
            "model": "claude-sonnet-4-5",
            "stop_reason": "tool_use",
            "content": [ { "type": "tool_use", "id": "t1", "name": "search_docs", "input": { "q": "rate limits" } } ],
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });
        let c = classify_anthropic(&resp, &server_tools(&["search_docs"]));
        assert_eq!(c.model_version.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(c.usage.input_tokens, Some(100));
        assert_eq!(c.usage.total_tokens, Some(120));
        match c.outcome {
            Outcome::Selected { tool, .. } => assert_eq!(tool, "search_docs"),
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_text_only_is_no_tool() {
        let resp = json!({
            "model": "claude-x",
            "stop_reason": "end_turn",
            "content": [ { "type": "text", "text": "I can't help with that." } ],
            "usage": { "input_tokens": 10, "output_tokens": 8 }
        });
        let c = classify_anthropic(&resp, &server_tools(&["search_docs"]));
        match c.outcome {
            Outcome::NoTool { excerpt } => assert!(excerpt.contains("can't help")),
            other => panic!("expected no_tool, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_unknown_tool_is_hallucinated() {
        let resp = json!({
            "content": [ { "type": "tool_use", "name": "delete_everything", "input": {} } ]
        });
        let c = classify_anthropic(&resp, &server_tools(&["search_docs"]));
        assert!(matches!(c.outcome, Outcome::HallucinatedTool { .. }));
    }

    // ---- OpenAI classification ---------------------------------------------

    #[test]
    fn openai_tool_call_is_selected_and_args_parsed() {
        let resp = json!({
            "model": "gpt-4o-2024",
            "choices": [ { "message": { "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "search_docs", "arguments": "{\"q\":\"rate\"}" } }
            ] }, "finish_reason": "tool_calls" } ],
            "usage": { "prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60 }
        });
        let c = classify_openai(&resp, &server_tools(&["search_docs"]));
        assert_eq!(c.model_version.as_deref(), Some("gpt-4o-2024"));
        assert_eq!(c.usage.total_tokens, Some(60));
        match c.outcome {
            Outcome::Selected {
                tool, arguments, ..
            } => {
                assert_eq!(tool, "search_docs");
                assert_eq!(arguments["q"], "rate");
            }
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn openai_malformed_args_is_unparseable_not_panic() {
        let resp = json!({
            "choices": [ { "message": { "tool_calls": [
                { "function": { "name": "search_docs", "arguments": "{not json" } }
            ] } } ]
        });
        let c = classify_openai(&resp, &server_tools(&["search_docs"]));
        match c.outcome {
            Outcome::Selected { args_check, .. } => {
                assert!(matches!(args_check, ArgCheck::Unparseable { .. }));
            }
            other => panic!("expected selected+unparseable, got {other:?}"),
        }
    }

    #[test]
    fn openai_text_only_is_no_tool() {
        let resp = json!({
            "choices": [ { "message": { "content": "No suitable tool.", "role": "assistant" }, "finish_reason": "stop" } ]
        });
        let c = classify_openai(&resp, &server_tools(&["search_docs"]));
        assert!(matches!(c.outcome, Outcome::NoTool { .. }));
    }

    // ---- argument validation -----------------------------------------------

    #[test]
    fn validate_accepts_correct_args() {
        let schema = json!({
            "type": "object",
            "properties": {
                "party": {
                    "type": "object",
                    "properties": {
                        "size": { "type": "integer" },
                        "seating": { "type": "string", "enum": ["indoor", "outdoor", "bar"] }
                    },
                    "required": ["size"]
                },
                "date": { "type": "string" }
            },
            "required": ["party", "date"]
        });
        let args = json!({ "party": { "size": 4, "seating": "outdoor" }, "date": "2026-01-01" });
        assert_eq!(validate_args(&schema, &args), ArgCheck::Valid);
    }

    #[test]
    fn validate_flags_missing_required_wrong_type_and_bad_enum() {
        let schema = json!({
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
            "required": ["party", "date"]
        });
        // Missing `date`; size is a string not integer; seating not in enum.
        let args = json!({ "party": { "size": "four", "seating": "rooftop" } });
        let check = validate_args(&schema, &args);
        match check {
            ArgCheck::Invalid { errors } => {
                let joined = errors.join(" | ");
                assert!(joined.contains("missing required field 'date'"), "{joined}");
                assert!(joined.contains("expected integer"), "{joined}");
                assert!(joined.contains("not one of"), "{joined}");
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn validate_integer_accepts_whole_float() {
        let schema = json!({ "type": "object", "properties": { "n": { "type": "integer" } } });
        assert_eq!(
            validate_args(&schema, &json!({ "n": 3.0 })),
            ArgCheck::Valid
        );
        assert!(matches!(
            validate_args(&schema, &json!({ "n": 3.5 })),
            ArgCheck::Invalid { .. }
        ));
    }

    // ---- distribution / takeaway -------------------------------------------

    fn report_with(outcomes: Vec<Outcome>) -> BenchReport {
        let results = outcomes
            .into_iter()
            .enumerate()
            .map(|(i, o)| RunResult {
                index: i + 1,
                outcome: o,
                latency_ms: 0,
                usage: Usage::default(),
                model_version: None,
                raw_response: Value::Null,
            })
            .collect::<Vec<_>>();
        let runs = results.len();
        BenchReport {
            model_id: "m".into(),
            provider: Provider::Anthropic,
            api_model: "m-1".into(),
            temperature: 1.0,
            max_tokens: 1024,
            runs,
            system_prompt: BENCH_SYSTEM_PROMPT,
            rendered_request: Value::Null,
            results,
            server_tool_names: vec![],
            endpoint: provider_endpoint(Provider::Anthropic, None),
            keyless: false,
        }
    }

    fn selected(tool: &str) -> Outcome {
        Outcome::Selected {
            tool: tool.into(),
            arguments: json!({}),
            args_check: ArgCheck::Valid,
        }
    }

    #[test]
    fn distribution_counts_and_sorts() {
        let report = report_with(vec![
            selected("search_docs"),
            selected("search_docs"),
            selected("fetch_page"),
            selected("search_docs"),
            Outcome::NoTool {
                excerpt: "x".into(),
            },
        ]);
        let d = report.distribution();
        assert_eq!(d.total, 5);
        assert_eq!(d.selected[0], ("search_docs".to_string(), 3));
        assert_eq!(d.selected[1], ("fetch_page".to_string(), 1));
        assert_eq!(d.no_tool, 1);
        assert!(!d.is_consistent());
        assert!(d.takeaway().starts_with("UNSTABLE"));
    }

    #[test]
    fn consistent_when_all_same_tool() {
        let report = report_with(vec![selected("a"), selected("a"), selected("a")]);
        let d = report.distribution();
        assert!(d.is_consistent());
        assert!(d.takeaway().starts_with("consistent"));
    }

    // ---- redaction ----------------------------------------------------------

    #[test]
    fn redact_removes_secret_from_text_and_json() {
        assert_eq!(redact("key=sk-abc123 end", "sk-abc123"), "key=*** end");
        let v = json!({ "error": "bad key sk-abc123", "nested": ["sk-abc123"] });
        let r = redact_value(v, "sk-abc123");
        assert_eq!(r["error"], "bad key ***");
        assert_eq!(r["nested"][0], "***");
    }

    // ---- MCP sampling dialect ------------------------------------------------

    #[test]
    fn sampling_params_match_the_2025_06_18_shape() {
        let tools = vec![tool(
            "search_docs",
            json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
        )];
        let p = render_sampling_params(&tools, "find rate limits", 0.4, 512);
        // Spec shape: messages[].content is an object with a `type`.
        assert_eq!(p["messages"][0]["role"], "user");
        assert_eq!(p["messages"][0]["content"]["type"], "text");
        assert_eq!(p["messages"][0]["content"]["text"], "find rate limits");
        assert_eq!(p["maxTokens"], 512);
        assert_eq!(p["temperature"], 0.4);
        assert_eq!(p["modelPreferences"]["intelligencePriority"], 0.8);
        // Never steer the host toward a named model — that would let a report
        // imply a model that was not actually measured.
        assert!(
            p["modelPreferences"].get("hints").is_none(),
            "modelPreferences must carry no hints"
        );
        // The tool surface and the response protocol ride in the system prompt.
        let system = p["systemPrompt"].as_str().unwrap();
        assert!(system.starts_with(BENCH_SYSTEM_PROMPT));
        assert!(system.contains("search_docs"));
    }

    #[test]
    fn sampling_selection_reuses_the_shared_taxonomy() {
        let known = server_tools(&["search_docs"]);
        let out = classify_sampling_text(
            r#"{"tool": "search_docs", "arguments": {"q": "rate limits"}}"#,
            &known,
        );
        match out {
            Outcome::Selected {
                tool, arguments, ..
            } => {
                assert_eq!(tool, "search_docs");
                assert_eq!(arguments["q"], "rate limits");
            }
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn sampling_unknown_tool_is_hallucinated_like_the_api_path() {
        let out = classify_sampling_text(
            r#"{"tool": "delete_everything", "arguments": {}}"#,
            &server_tools(&["search_docs"]),
        );
        assert!(matches!(out, Outcome::HallucinatedTool { .. }));
    }

    #[test]
    fn sampling_null_tool_and_off_protocol_text_are_both_no_tool() {
        let known = server_tools(&["search_docs"]);
        match classify_sampling_text(r#"{"tool": null, "answer": "Nothing fits."}"#, &known) {
            Outcome::NoTool { excerpt } => assert_eq!(excerpt, "Nothing fits."),
            other => panic!("expected no_tool, got {other:?}"),
        }
        // A model that ignored the protocol entirely did not select a tool.
        match classify_sampling_text("I would probably use search_docs here.", &known) {
            Outcome::NoTool { excerpt } => assert!(excerpt.contains("search_docs")),
            other => panic!("expected no_tool, got {other:?}"),
        }
    }

    #[test]
    fn sampling_tolerates_code_fences_and_surrounding_prose() {
        let known = server_tools(&["search_docs"]);
        let fenced = "```json\n{\"tool\": \"search_docs\", \"arguments\": {\"q\": \"a\"}}\n```";
        assert!(matches!(
            classify_sampling_text(fenced, &known),
            Outcome::Selected { .. }
        ));
        let chatty =
            "Sure! {\"tool\": \"search_docs\", \"arguments\": {\"q\": \"} not a brace\"}} \
                      — hope that helps.";
        assert!(matches!(
            classify_sampling_text(chatty, &known),
            Outcome::Selected { .. }
        ));
    }

    #[test]
    fn sampling_args_are_validated_by_the_shared_validator() {
        let tools = vec![tool(
            "search_docs",
            json!({ "type": "object", "properties": { "q": { "type": "string" } }, "required": ["q"] }),
        )];
        let mut out = classify_sampling_text(
            r#"{"tool": "search_docs", "arguments": {}}"#,
            &server_tools(&["search_docs"]),
        );
        finalize_args_check(&tools, &mut out);
        match out {
            Outcome::Selected { args_check, .. } => {
                assert!(matches!(args_check, ArgCheck::Invalid { .. }));
            }
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn host_models_records_unknown_rather_than_guessing() {
        let results = vec![
            RunResult {
                index: 1,
                outcome: selected("a"),
                latency_ms: 0,
                usage: Usage::default(),
                model_version: Some("host-model-x".into()),
                raw_response: Value::Null,
            },
            RunResult {
                index: 2,
                outcome: selected("a"),
                latency_ms: 0,
                usage: Usage::default(),
                model_version: None,
                raw_response: Value::Null,
            },
        ];
        let models = host_models_of(&results);
        assert_eq!(models, vec!["host-model-x", SAMPLING_MODEL_UNKNOWN]);
    }

    #[test]
    fn keyless_config_is_reported_as_such() {
        let mut config = BenchConfig {
            model: BenchModel::resolve("gpt-4o").unwrap(),
            task: "t".into(),
            runs: 1,
            temperature: 1.0,
            max_tokens: 16,
            timeout: None,
            base_url: Some("http://localhost:11434/v1".into()),
            api_key: String::new(),
        };
        assert!(config.is_keyless());
        config.api_key = "sk-x".into();
        assert!(!config.is_keyless());
    }

    #[test]
    fn provider_endpoint_uses_override_and_default() {
        assert_eq!(
            provider_endpoint(Provider::Anthropic, None),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            provider_endpoint(Provider::OpenAI, Some("http://127.0.0.1:9000/")),
            "http://127.0.0.1:9000/v1/chat/completions"
        );
    }

    /// The recipes in the README are the ones users paste. Every local runtime
    /// documents its base URL *with* the `/v1`, so that form must not double it.
    #[test]
    fn base_url_join_is_idempotent_in_the_version_segment() {
        for base in [
            "http://localhost:11434/v1",  // Ollama
            "http://localhost:11434/v1/", // …with a trailing slash
            "http://localhost:11434",     // …and without the version at all
        ] {
            assert_eq!(
                provider_endpoint(Provider::OpenAI, Some(base)),
                "http://localhost:11434/v1/chat/completions",
                "base {base} joined wrong"
            );
        }
        assert_eq!(
            provider_endpoint(Provider::Anthropic, Some("https://gateway.corp/v1")),
            "https://gateway.corp/v1/messages"
        );
        // A path that merely *contains* v1 elsewhere is untouched.
        assert_eq!(
            provider_endpoint(Provider::OpenAI, Some("https://gw.corp/v1/openai")),
            "https://gw.corp/v1/openai/v1/chat/completions"
        );
    }
}
