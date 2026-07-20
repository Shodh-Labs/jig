//! The **opt-in semantic description judge** — a model reading the tool
//! descriptions and answering three fixed questions a deterministic heuristic
//! cannot.
//!
//! # The gap this closes
//!
//! [`check`](crate::check)'s description-quality dimension is deterministic:
//! length bands, placeholder text, missing parameter descriptions. Those are
//! real defects and they are cheap, reproducible and free. But no regular
//! expression can tell whether a description *states what the tool does* — the
//! single most common failure in the wild. The survey behind this milestone
//! (arXiv:2602.14878) found 97.1% of 856 tool descriptions carried at least one
//! smell and 56% never stated their purpose at all. A heuristic scores those
//! descriptions as fine.
//!
//! # The honesty contract
//!
//! Asking a model closes the gap and opens a hole: model output is not
//! reproducible, and a number derived from it is not a measurement. So this
//! module is fenced off from the score by construction, not by convention:
//!
//! 1. **Off by default.** Nothing here runs without `jig check --judge`.
//! 2. **Never scored.** A [`JudgeReport`] is not an input to
//!    [`evaluate`](crate::check::evaluate). It cannot reach the composite, a
//!    dimension score, the badge, `--min-score`, or the report card's grade —
//!    those are computed from a [`CheckInput`](crate::check::CheckInput) that
//!    has no judge field at all. Judged output is **outside rubric-v1.3**.
//! 3. **Pinned and recorded.** [`JUDGE_PROMPT_VERSION`], the verbatim
//!    [`JUDGE_SYSTEM_PROMPT`], the temperature, the endpoint, and the model id
//!    *as the provider reported it* ([`JudgeReport::reported_model`], never the
//!    id we asked for) all ride in every report and in `--json`. A judged
//!    verdict whose prompt version is unknown is worthless.
//! 4. **Graceful absence.** Any failure — no key, no endpoint, a 500, a
//!    timeout — is a [`JudgeError`] the caller renders as one line. `check`
//!    still prints its report and still exits with its usual code.
//! 5. **Defensive parsing.** The model will sometimes answer in prose. That is
//!    an [`ToolVerdict::Unparseable`] verdict, never a panic and never a guess.
//!
//! # Reuse, not a fork
//!
//! The judge does not own an HTTP client, a retry policy, a redaction rule or a
//! keyless-mode branch. It sends through
//! [`bench::send_provider_request`] — the
//! same function `jig bench` and `jig eval` send through — so a `--base-url`
//! (Ollama, LM Studio, a gateway) with `--no-auth` judges exactly as well as a
//! vendor key, by the same code.

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::bench::{
    self, build_provider_client, provider_endpoint, redact, BenchError, BenchModel, Provider, Usage,
};
use crate::protocol::Tool;

/// The version of the judge prompt below.
///
/// **Bump this whenever [`JUDGE_SYSTEM_PROMPT`] changes by so much as a
/// comma.** Two judged runs are comparable only if they were asked the same
/// question, and the only way a reader can know that is if the question carries
/// a version. It is emitted in the human section and in `--json`.
pub const JUDGE_PROMPT_VERSION: &str = "judge-prompt-v1";

/// The **verbatim, pinned** system prompt the judge sends. Echoed in full in
/// `--json` so a judged verdict is never a black box.
///
/// The three questions are fixed and deliberately narrow — each is answerable
/// from the description text alone, without running the tool, without the
/// server's source, and without knowledge of the domain.
pub const JUDGE_SYSTEM_PROMPT: &str = "You are auditing the tool descriptions of one MCP \
(Model Context Protocol) server. You are not calling the tools and you are not judging the \
server's implementation — only whether an agent reading these descriptions could choose and \
call the tools correctly.\n\n\
For every tool in the list, answer exactly three questions:\n\
1. states_purpose — Does the description say what the tool actually does?\n\
2. distinguishes_siblings — Does it say when to use this tool rather than another tool in this \
same list? If the list has only one tool, answer \"unclear\" and say that there is no sibling to \
distinguish it from.\n\
3. parameters_sufficient — Are the parameter descriptions enough for a caller to fill every \
parameter correctly without guessing?\n\n\
Answer each question with a verdict of exactly \"yes\", \"no\", or \"unclear\", plus one \
sentence of rationale (at most 160 characters). Use \"unclear\" when the description is \
ambiguous rather than plainly good or plainly bad. Judge only what the text says; do not \
assume undocumented behaviour.\n\n\
Reply with a single JSON object and nothing else, in this exact shape:\n\
{\"judgements\": [{\"tool\": \"<name>\", \"states_purpose\": {\"verdict\": \"yes\", \
\"rationale\": \"...\"}, \"distinguishes_siblings\": {\"verdict\": \"no\", \"rationale\": \
\"...\"}, \"parameters_sufficient\": {\"verdict\": \"unclear\", \"rationale\": \"...\"}}]}\n\n\
Include exactly one entry per tool, in the order given. Do not wrap the object in prose or \
code fences.";

/// The judge's sampling temperature: **0.0**, the most reproducible setting a
/// provider offers. Recorded in every report anyway, because "most
/// reproducible" is not "reproducible".
pub const JUDGE_TEMPERATURE: f64 = 0.0;

/// `max_tokens` for a judge response. Three short rationales per tool over a
/// typical surface fits comfortably; the Anthropic Messages API requires the
/// field regardless.
pub const JUDGE_MAX_TOKENS: u32 = 4096;

/// The maximum rationale length kept from the model, in characters. A model
/// that ignores the one-sentence instruction is truncated, not trusted.
const RATIONALE_MAX_CHARS: usize = 200;

/// Failures that leave the judge **unavailable**. Every one of these is
/// rendered by the caller as a single line and then ignored: `check` proceeds,
/// prints its deterministic report, and exits with its usual code.
///
/// Note what is *not* here: a model that answered in prose, or answered about
/// only some of the tools. That is not a failure of the judge, it is data about
/// the model — see [`ToolVerdict::Unparseable`] and [`ToolVerdict::NotJudged`].
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// The server exposes no tools, so there is nothing to judge.
    #[error("the server exposes no tools")]
    NoTools,
    /// The HTTP client could not be constructed.
    #[error("{0}")]
    Client(String),
    /// The provider request failed after the shared bounded retry.
    #[error("{0}")]
    Provider(String),
}

impl From<BenchError> for JudgeError {
    fn from(e: BenchError) -> Self {
        JudgeError::Client(e.to_string())
    }
}

/// Configuration for one judge pass over a server's whole tool surface.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    /// The resolved judge model.
    pub model: BenchModel,
    /// Sampling temperature. Always recorded.
    pub temperature: f64,
    /// `max_tokens` for the response.
    pub max_tokens: u32,
    /// Per-request timeout. `None` waits indefinitely.
    pub timeout: Option<Duration>,
    /// An OpenAI-compatible base URL override (Ollama, LM Studio, a gateway).
    pub base_url: Option<String>,
    /// The API key. **Empty means keyless** — no credential header is sent.
    pub api_key: String,
}

impl JudgeConfig {
    /// Whether this pass sends no credential at all.
    pub fn is_keyless(&self) -> bool {
        self.api_key.is_empty()
    }
}

/// One of the three fixed questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Question {
    /// Does the description say what the tool does?
    StatesPurpose,
    /// Does it say when to use this tool rather than a sibling?
    DistinguishesSiblings,
    /// Are the parameter descriptions sufficient to fill them correctly?
    ParametersSufficient,
}

impl Question {
    /// Every question, in the order they are asked and rendered.
    pub fn all() -> [Question; 3] {
        [
            Question::StatesPurpose,
            Question::DistinguishesSiblings,
            Question::ParametersSufficient,
        ]
    }

    /// The JSON key this question uses in the prompt and in `--json`.
    pub fn key(self) -> &'static str {
        match self {
            Question::StatesPurpose => "states_purpose",
            Question::DistinguishesSiblings => "distinguishes_siblings",
            Question::ParametersSufficient => "parameters_sufficient",
        }
    }

    /// The short human label rendered in the report section.
    pub fn label(self) -> &'static str {
        match self {
            Question::StatesPurpose => "states its purpose",
            Question::DistinguishesSiblings => "distinguishes siblings",
            Question::ParametersSufficient => "parameters sufficient",
        }
    }
}

/// A model's answer to one question. [`Answer::Unparseable`] is what an
/// unrecognized verdict string becomes — Jig never maps an answer it does not
/// recognize onto one it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The model said yes.
    Yes,
    /// The model said no.
    No,
    /// The model said the description is ambiguous.
    Unclear,
    /// The model returned something other than yes/no/unclear.
    Unparseable,
}

impl Answer {
    /// The lowercase tag used in `--json`.
    pub fn tag(self) -> &'static str {
        match self {
            Answer::Yes => "yes",
            Answer::No => "no",
            Answer::Unclear => "unclear",
            Answer::Unparseable => "unparseable",
        }
    }

    /// The glyph used in the human section: `✓` yes, `✗` no, `?` unclear, `!`
    /// unparseable.
    pub fn glyph(self) -> char {
        match self {
            Answer::Yes => '✓',
            Answer::No => '✗',
            Answer::Unclear => '?',
            Answer::Unparseable => '!',
        }
    }

    /// Parse a verdict string. Total; anything unrecognized is
    /// [`Answer::Unparseable`].
    pub fn parse(raw: &str) -> Answer {
        match raw.trim().trim_matches('.').to_ascii_lowercase().as_str() {
            "yes" | "y" | "true" => Answer::Yes,
            "no" | "n" | "false" => Answer::No,
            "unclear" | "unknown" | "n/a" | "na" | "partial" => Answer::Unclear,
            _ => Answer::Unparseable,
        }
    }
}

/// One answered question: the verdict plus the model's one-line rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    /// The verdict.
    pub answer: Answer,
    /// The model's rationale, flattened to one line and truncated.
    pub rationale: String,
}

/// What the judge concluded about one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVerdict {
    /// The model answered all three questions in the requested shape.
    Judged {
        /// Does the description say what the tool does?
        states_purpose: Judgement,
        /// Does it say when to use this tool rather than a sibling?
        distinguishes_siblings: Judgement,
        /// Are the parameter descriptions sufficient?
        parameters_sufficient: Judgement,
    },
    /// The model produced output for this tool that was not the requested
    /// structure — prose instead of JSON, a wrong-shaped entry, a verdict that
    /// was not yes/no/unclear. Recorded as-is; never guessed at.
    Unparseable {
        /// What went wrong, and (where useful) an excerpt of what came back.
        detail: String,
    },
    /// The model's response was structurally fine but simply did not mention
    /// this tool. A partial answer is partial data, not an error.
    NotJudged,
}

impl ToolVerdict {
    /// A short tag for `--json` and tables.
    pub fn tag(&self) -> &'static str {
        match self {
            ToolVerdict::Judged { .. } => "judged",
            ToolVerdict::Unparseable { .. } => "unparseable",
            ToolVerdict::NotJudged => "not_judged",
        }
    }

    /// The judgement for `question`, when this tool was actually judged.
    pub fn answer(&self, question: Question) -> Option<&Judgement> {
        match self {
            ToolVerdict::Judged {
                states_purpose,
                distinguishes_siblings,
                parameters_sufficient,
            } => Some(match question {
                Question::StatesPurpose => states_purpose,
                Question::DistinguishesSiblings => distinguishes_siblings,
                Question::ParametersSufficient => parameters_sufficient,
            }),
            _ => None,
        }
    }
}

/// The judge's verdict on one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolJudgement {
    /// The tool name, taken from the **server's** surface, never from the
    /// model's reply — a model cannot rename a tool by hallucinating.
    pub tool: String,
    /// What the judge concluded.
    pub verdict: ToolVerdict,
}

/// The full result of one judge pass.
#[derive(Debug, Clone)]
pub struct JudgeReport {
    /// The pinned prompt version ([`JUDGE_PROMPT_VERSION`]).
    pub prompt_version: &'static str,
    /// The verbatim system prompt ([`JUDGE_SYSTEM_PROMPT`]).
    pub system_prompt: &'static str,
    /// The verbatim user message: the tool surface as it was presented.
    pub user_prompt: String,
    /// The exact request body that was sent (auth-free — the key is a header).
    pub rendered_request: Value,
    /// The provider dialect used.
    pub provider: Provider,
    /// The model id Jig **asked** for.
    pub requested_model: String,
    /// The model id the **provider reported**. `None` when the provider named
    /// no model — recorded as unknown rather than assumed to be the requested
    /// one.
    pub reported_model: Option<String>,
    /// The sampling temperature used.
    pub temperature: f64,
    /// The exact endpoint the request went to.
    pub endpoint: String,
    /// Whether the pass sent no credential at all.
    pub keyless: bool,
    /// Per-tool verdicts, in the server's tool order.
    pub judgements: Vec<ToolJudgement>,
    /// The assistant text the provider returned, redacted and flattened.
    pub response_text: String,
    /// The raw provider response, redacted, for `--json`.
    pub raw_response: Value,
    /// Token usage, if the provider reported it.
    pub usage: Usage,
}

impl JudgeReport {
    /// The model identity to display: what the provider reported, or an
    /// explicit "unreported" marker plus what we asked for. Never silently the
    /// requested id.
    pub fn model_label(&self) -> String {
        match &self.reported_model {
            Some(m) => m.clone(),
            None => format!(
                "unreported by provider (requested {})",
                self.requested_model
            ),
        }
    }

    /// Tally answers across every judged tool for one question: `(yes, no,
    /// unclear, unparseable)`.
    pub fn tally(&self, question: Question) -> (usize, usize, usize, usize) {
        let mut t = (0usize, 0usize, 0usize, 0usize);
        for j in &self.judgements {
            match j.verdict.answer(question).map(|a| &a.answer) {
                Some(Answer::Yes) => t.0 += 1,
                Some(Answer::No) => t.1 += 1,
                Some(Answer::Unclear) => t.2 += 1,
                Some(Answer::Unparseable) => t.3 += 1,
                None => {}
            }
        }
        t
    }

    /// How many tools the model actually judged.
    pub fn judged_count(&self) -> usize {
        self.judgements
            .iter()
            .filter(|j| matches!(j.verdict, ToolVerdict::Judged { .. }))
            .count()
    }

    /// How many tools the model's reply did not cover.
    pub fn not_judged_count(&self) -> usize {
        self.judgements
            .iter()
            .filter(|j| matches!(j.verdict, ToolVerdict::NotJudged))
            .count()
    }

    /// How many tools came back in an unusable shape.
    pub fn unparseable_count(&self) -> usize {
        self.judgements
            .iter()
            .filter(|j| matches!(j.verdict, ToolVerdict::Unparseable { .. }))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering (pure)
// ---------------------------------------------------------------------------

/// Render the user message: the tool surface, exactly as the judge sees it.
///
/// Only the three things the questions are about are presented — name,
/// description, and input schema. Nothing about how the server scored, so the
/// judge cannot be anchored by Jig's own deterministic verdict.
pub fn render_tool_surface(tools: &[Tool]) -> String {
    let list: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(t.name));
            m.insert(
                "description".to_string(),
                match &t.description {
                    Some(d) => json!(d),
                    None => Value::Null,
                },
            );
            m.insert(
                "inputSchema".to_string(),
                if t.input_schema.is_object() {
                    t.input_schema.clone()
                } else {
                    json!({ "type": "object", "properties": {} })
                },
            );
            Value::Object(m)
        })
        .collect();
    format!(
        "Tools ({}):\n{}",
        tools.len(),
        serde_json::to_string_pretty(&Value::Array(list)).unwrap_or_else(|_| "[]".to_string())
    )
}

/// Render the provider request body for a judge pass. Pure, key-free, and
/// snapshot-testable — this exact value is echoed in `--json`.
///
/// Note there is no `tools` array: the judge is asked to *read* descriptions
/// and answer in text, not to select a tool. That is the whole difference from
/// a [`crate::bench`] request.
pub fn render_judge_request(
    provider: Provider,
    tools: &[Tool],
    api_model: &str,
    temperature: f64,
    max_tokens: u32,
) -> Value {
    let user = render_tool_surface(tools);
    match provider {
        Provider::Anthropic => json!({
            "model": api_model,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "system": JUDGE_SYSTEM_PROMPT,
            "messages": [ { "role": "user", "content": user } ],
        }),
        Provider::OpenAI => json!({
            "model": api_model,
            "temperature": temperature,
            "messages": [
                { "role": "system", "content": JUDGE_SYSTEM_PROMPT },
                { "role": "user", "content": user },
            ],
        }),
    }
}

// ---------------------------------------------------------------------------
// Response parsing (pure, total — never panics)
// ---------------------------------------------------------------------------

/// Pull the assistant text out of a provider response in either dialect.
/// Total over arbitrary JSON: an unrecognized body yields an empty string.
pub fn judge_response_text(resp: &Value, provider: Provider) -> String {
    match provider {
        Provider::Anthropic => bench::anthropic_text(resp),
        Provider::OpenAI => resp
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// Parse the judge's reply into one verdict per tool, keyed by the **server's**
/// tool names.
///
/// Total over arbitrary text. The failure modes, in order:
///
/// * no JSON object anywhere in the reply (the model wrote prose) — every tool
///   gets [`ToolVerdict::Unparseable`] carrying an excerpt of what came back;
/// * a JSON object with no usable `judgements` array — likewise;
/// * an entry whose questions are missing or wrong-shaped — that *one* tool is
///   [`ToolVerdict::Unparseable`];
/// * no entry for a tool — that tool is [`ToolVerdict::NotJudged`].
///
/// Entries naming a tool the server does not expose are discarded: a model
/// cannot add a tool to the surface by mentioning one.
pub fn parse_judgements(text: &str, tool_names: &[String]) -> Vec<ToolJudgement> {
    let all_unparseable = |detail: String| -> Vec<ToolJudgement> {
        tool_names
            .iter()
            .map(|name| ToolJudgement {
                tool: name.clone(),
                verdict: ToolVerdict::Unparseable {
                    detail: detail.clone(),
                },
            })
            .collect()
    };

    let Some(obj) = bench::extract_json_object(text) else {
        return all_unparseable(format!(
            "the model did not return a JSON object; it replied: {}",
            bench::excerpt(text)
        ));
    };
    let Some(entries) = obj.get("judgements").and_then(Value::as_array) else {
        return all_unparseable(format!(
            "the model's JSON object has no `judgements` array; it replied: {}",
            bench::excerpt(text)
        ));
    };

    tool_names
        .iter()
        .map(|name| {
            let entry = entries.iter().find(|e| {
                e.get("tool").and_then(Value::as_str).map(str::trim) == Some(name.as_str())
            });
            let verdict = match entry {
                None => ToolVerdict::NotJudged,
                Some(e) => parse_entry(e),
            };
            ToolJudgement {
                tool: name.clone(),
                verdict,
            }
        })
        .collect()
}

/// Parse one `judgements[]` entry into a verdict. An entry missing any of the
/// three questions is unparseable as a whole — a partial answer about one tool
/// cannot be presented as a complete one.
fn parse_entry(entry: &Value) -> ToolVerdict {
    let question = |q: Question| -> Option<Judgement> {
        let node = entry.get(q.key())?;
        // Tolerate both the requested `{verdict, rationale}` object and a bare
        // verdict string, which models emit when they compress the shape.
        let (raw_verdict, rationale) = match node {
            Value::String(s) => (s.as_str(), String::new()),
            Value::Object(_) => (
                node.get("verdict").and_then(Value::as_str)?,
                node.get("rationale")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default(),
            ),
            _ => return None,
        };
        Some(Judgement {
            answer: Answer::parse(raw_verdict),
            rationale,
        })
    };

    match (
        question(Question::StatesPurpose),
        question(Question::DistinguishesSiblings),
        question(Question::ParametersSufficient),
    ) {
        (Some(states_purpose), Some(distinguishes_siblings), Some(parameters_sufficient)) => {
            ToolVerdict::Judged {
                states_purpose,
                distinguishes_siblings,
                parameters_sufficient,
            }
        }
        _ => ToolVerdict::Unparseable {
            detail: format!(
                "the entry did not carry all three questions as {{verdict, rationale}}: {}",
                bench::excerpt(&entry.to_string())
            ),
        },
    }
}

/// Flatten a rationale to one line and bound its length.
fn one_line(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= RATIONALE_MAX_CHARS {
        return flat;
    }
    let mut out: String = flat
        .chars()
        .take(RATIONALE_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// The live pass
// ---------------------------------------------------------------------------

/// Run one judge pass over `tools`.
///
/// A **single** request carries the whole tool surface, because question 2 —
/// "does it say when to use this rather than a sibling?" — is unanswerable
/// per-tool in isolation. The model must see the siblings to judge the
/// distinction.
///
/// # Errors
///
/// Returns [`JudgeError`] when there is nothing to judge, the client cannot be
/// built, or the provider request fails after the shared bounded retry. Every
/// one of these leaves `check` free to print its report and exit normally — the
/// judge is an addition, never a gate.
pub async fn run_judge(tools: &[Tool], config: &JudgeConfig) -> Result<JudgeReport, JudgeError> {
    if tools.is_empty() {
        return Err(JudgeError::NoTools);
    }
    let provider = config.model.provider;
    let rendered_request = render_judge_request(
        provider,
        tools,
        &config.model.api_model,
        config.temperature,
        config.max_tokens,
    );
    let client = build_provider_client(config.timeout)?;
    let endpoint = provider_endpoint(provider, config.base_url.as_deref());

    let raw = bench::send_provider_request(
        &client,
        provider,
        &endpoint,
        &rendered_request,
        &config.api_key,
    )
    .await
    .map_err(|detail| JudgeError::Provider(redact(&detail, &config.api_key)))?;

    let raw_response = bench::redact_value(raw, &config.api_key);
    let response_text = redact(
        &judge_response_text(&raw_response, provider),
        &config.api_key,
    );
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

    Ok(JudgeReport {
        prompt_version: JUDGE_PROMPT_VERSION,
        system_prompt: JUDGE_SYSTEM_PROMPT,
        user_prompt: render_tool_surface(tools),
        judgements: parse_judgements(&response_text, &tool_names),
        provider,
        requested_model: config.model.api_model.clone(),
        reported_model: raw_response
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        temperature: config.temperature,
        endpoint,
        keyless: config.is_keyless(),
        usage: match provider {
            Provider::Anthropic => {
                let i = bench::usage_u64(&raw_response, "usage", "input_tokens");
                let o = bench::usage_u64(&raw_response, "usage", "output_tokens");
                Usage {
                    input_tokens: i,
                    output_tokens: o,
                    total_tokens: match (i, o) {
                        (Some(i), Some(o)) => Some(i + o),
                        _ => None,
                    },
                }
            }
            Provider::OpenAI => Usage {
                input_tokens: bench::usage_u64(&raw_response, "usage", "prompt_tokens"),
                output_tokens: bench::usage_u64(&raw_response, "usage", "completion_tokens"),
                total_tokens: bench::usage_u64(&raw_response, "usage", "total_tokens"),
            },
        },
        response_text,
        rendered_request,
        raw_response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: Option<&str>) -> Tool {
        let mut m = Map::new();
        m.insert("name".to_string(), json!(name));
        if let Some(d) = desc {
            m.insert("description".to_string(), json!(d));
        }
        m.insert(
            "inputSchema".to_string(),
            json!({ "type": "object", "properties": {} }),
        );
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const WELL_FORMED: &str = r#"{"judgements":[
        {"tool":"echo","states_purpose":{"verdict":"yes","rationale":"Says it returns the input text."},
         "distinguishes_siblings":{"verdict":"no","rationale":"Never contrasts with make_reservation."},
         "parameters_sufficient":{"verdict":"yes","rationale":"text is described."}},
        {"tool":"make_reservation","states_purpose":{"verdict":"unclear","rationale":"Book a table where?"},
         "distinguishes_siblings":{"verdict":"no","rationale":"No guidance versus echo."},
         "parameters_sufficient":{"verdict":"no","rationale":"seating enum is undocumented."}}
    ]}"#;

    #[test]
    fn well_formed_judgements_parse() {
        let out = parse_judgements(WELL_FORMED, &names(&["echo", "make_reservation"]));
        assert_eq!(out.len(), 2);
        let echo = &out[0].verdict;
        assert_eq!(
            echo.answer(Question::StatesPurpose).map(|j| &j.answer),
            Some(&Answer::Yes)
        );
        assert_eq!(
            echo.answer(Question::DistinguishesSiblings)
                .map(|j| &j.answer),
            Some(&Answer::No)
        );
        assert_eq!(
            out[1]
                .verdict
                .answer(Question::ParametersSufficient)
                .map(|j| j.rationale.as_str()),
            Some("seating enum is undocumented.")
        );
    }

    #[test]
    fn code_fenced_and_prose_wrapped_json_still_parses() {
        let wrapped = format!("Sure! Here you go:\n```json\n{WELL_FORMED}\n```\nHope that helps.");
        let out = parse_judgements(&wrapped, &names(&["echo", "make_reservation"]));
        assert!(matches!(out[0].verdict, ToolVerdict::Judged { .. }));
    }

    #[test]
    fn prose_instead_of_json_is_unparseable_never_a_guess() {
        let out = parse_judgements(
            "I think these descriptions are mostly fine, honestly.",
            &names(&["echo"]),
        );
        assert_eq!(out.len(), 1);
        match &out[0].verdict {
            ToolVerdict::Unparseable { detail } => {
                assert!(detail.contains("did not return a JSON object"), "{detail}");
                assert!(detail.contains("mostly fine"), "{detail}");
            }
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn object_without_judgements_array_is_unparseable() {
        let out = parse_judgements(r#"{"result": "all good"}"#, &names(&["echo"]));
        assert!(matches!(out[0].verdict, ToolVerdict::Unparseable { .. }));
    }

    #[test]
    fn a_tool_the_model_skipped_is_not_judged_not_invented() {
        let out = parse_judgements(WELL_FORMED, &names(&["echo", "make_reservation", "third"]));
        assert_eq!(out[2].tool, "third");
        assert_eq!(out[2].verdict, ToolVerdict::NotJudged);
    }

    #[test]
    fn an_entry_missing_a_question_is_unparseable_for_that_tool_only() {
        let text = r#"{"judgements":[
            {"tool":"echo","states_purpose":{"verdict":"yes","rationale":"ok"}},
            {"tool":"other","states_purpose":{"verdict":"yes","rationale":"a"},
             "distinguishes_siblings":{"verdict":"yes","rationale":"b"},
             "parameters_sufficient":{"verdict":"yes","rationale":"c"}}
        ]}"#;
        let out = parse_judgements(text, &names(&["echo", "other"]));
        assert!(matches!(out[0].verdict, ToolVerdict::Unparseable { .. }));
        assert!(matches!(out[1].verdict, ToolVerdict::Judged { .. }));
    }

    #[test]
    fn an_unrecognized_verdict_word_is_unparseable_not_coerced() {
        let text = r#"{"judgements":[{"tool":"echo",
            "states_purpose":{"verdict":"mostly","rationale":"hmm"},
            "distinguishes_siblings":{"verdict":"yes","rationale":"b"},
            "parameters_sufficient":{"verdict":"yes","rationale":"c"}}]}"#;
        let out = parse_judgements(text, &names(&["echo"]));
        assert_eq!(
            out[0]
                .verdict
                .answer(Question::StatesPurpose)
                .map(|j| &j.answer),
            Some(&Answer::Unparseable)
        );
    }

    #[test]
    fn an_entry_naming_an_unknown_tool_is_discarded() {
        let text = r#"{"judgements":[{"tool":"ghost",
            "states_purpose":{"verdict":"yes","rationale":"a"},
            "distinguishes_siblings":{"verdict":"yes","rationale":"b"},
            "parameters_sufficient":{"verdict":"yes","rationale":"c"}}]}"#;
        let out = parse_judgements(text, &names(&["echo"]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool, "echo");
        assert_eq!(out[0].verdict, ToolVerdict::NotJudged);
    }

    #[test]
    fn a_bare_verdict_string_is_accepted_with_an_empty_rationale() {
        let text = r#"{"judgements":[{"tool":"echo",
            "states_purpose":"yes","distinguishes_siblings":"no","parameters_sufficient":"unclear"}]}"#;
        let out = parse_judgements(text, &names(&["echo"]));
        let j = out[0].verdict.answer(Question::StatesPurpose).unwrap();
        assert_eq!(j.answer, Answer::Yes);
        assert_eq!(j.rationale, "");
    }

    #[test]
    fn rationale_is_flattened_and_bounded() {
        let long = "x ".repeat(400);
        let text = format!(
            r#"{{"judgements":[{{"tool":"echo",
            "states_purpose":{{"verdict":"yes","rationale":"{long}"}},
            "distinguishes_siblings":{{"verdict":"yes","rationale":"line\nbreak"}},
            "parameters_sufficient":{{"verdict":"yes","rationale":"c"}}}}]}}"#
        );
        let out = parse_judgements(&text, &names(&["echo"]));
        let v = &out[0].verdict;
        let r = &v.answer(Question::StatesPurpose).unwrap().rationale;
        assert!(r.chars().count() <= RATIONALE_MAX_CHARS, "{}", r.len());
        assert_eq!(
            v.answer(Question::DistinguishesSiblings).unwrap().rationale,
            "line break"
        );
    }

    #[test]
    fn empty_text_is_unparseable_not_a_panic() {
        let out = parse_judgements("", &names(&["echo"]));
        assert!(matches!(out[0].verdict, ToolVerdict::Unparseable { .. }));
    }

    #[test]
    fn request_carries_the_pinned_prompt_and_no_tools_array() {
        let tools = vec![tool("echo", Some("Echo it back."))];
        let anth = render_judge_request(Provider::Anthropic, &tools, "claude-x", 0.0, 4096);
        assert_eq!(anth["system"], JUDGE_SYSTEM_PROMPT);
        assert_eq!(anth["temperature"], 0.0);
        assert!(anth.get("tools").is_none(), "the judge selects no tool");
        assert!(anth["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Echo it back."));

        let oai = render_judge_request(Provider::OpenAI, &tools, "gpt-x", 0.0, 4096);
        assert_eq!(oai["messages"][0]["content"], JUDGE_SYSTEM_PROMPT);
        assert!(oai.get("tools").is_none());
    }

    #[test]
    fn a_tool_without_a_description_is_shown_as_null_not_omitted() {
        let surface = render_tool_surface(&[tool("bare", None)]);
        assert!(surface.contains("\"description\": null"), "{surface}");
    }

    #[test]
    fn response_text_extraction_is_total_over_both_dialects() {
        assert_eq!(
            judge_response_text(
                &json!({ "content": [{ "type": "text", "text": "hi" }] }),
                Provider::Anthropic
            ),
            "hi"
        );
        assert_eq!(
            judge_response_text(
                &json!({ "choices": [{ "message": { "content": "hi" } }] }),
                Provider::OpenAI
            ),
            "hi"
        );
        assert_eq!(
            judge_response_text(&json!({ "nonsense": 1 }), Provider::OpenAI),
            ""
        );
        assert_eq!(judge_response_text(&Value::Null, Provider::Anthropic), "");
    }

    #[test]
    fn tally_counts_only_judged_tools() {
        let report = JudgeReport {
            prompt_version: JUDGE_PROMPT_VERSION,
            system_prompt: JUDGE_SYSTEM_PROMPT,
            user_prompt: String::new(),
            rendered_request: Value::Null,
            provider: Provider::OpenAI,
            requested_model: "gpt-x".to_string(),
            reported_model: None,
            temperature: JUDGE_TEMPERATURE,
            endpoint: "http://x/v1/chat/completions".to_string(),
            keyless: true,
            judgements: parse_judgements(WELL_FORMED, &names(&["echo", "make_reservation", "z"])),
            response_text: String::new(),
            raw_response: Value::Null,
            usage: Usage::default(),
        };
        assert_eq!(report.judged_count(), 2);
        assert_eq!(report.not_judged_count(), 1);
        assert_eq!(report.unparseable_count(), 0);
        // states_purpose: yes=1 (echo), unclear=1 (make_reservation).
        assert_eq!(report.tally(Question::StatesPurpose), (1, 0, 1, 0));
        // distinguishes_siblings: no=2.
        assert_eq!(report.tally(Question::DistinguishesSiblings), (0, 2, 0, 0));
        assert!(report.model_label().contains("unreported by provider"));
    }
}
