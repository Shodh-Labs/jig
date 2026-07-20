//! CLI plumbing and rendering for the **opt-in description judge**
//! (`jig check --judge`).
//!
//! The engine is [`jig_core::judge`]; this module owns flag handling, model and
//! key resolution (shared with `jig bench` via [`crate::bench::Endpoint`], so
//! `--base-url` + `--no-auth` against a local Ollama works identically), and
//! the two renderers.
//!
//! Both renderers are pure functions of a [`JudgeOutcome`], so the human
//! section and the `judged` JSON key are snapshot-locked.
//!
//! # The line this module must never cross
//!
//! Everything here appends. The judged section is rendered *after* the
//! deterministic report and the `judged` key is *added* to a document that was
//! already complete. Nothing in this file can read, let alone write, a
//! dimension score, the composite, the badge or the `--min-score` gate — the
//! judge simply is not one of their inputs. See
//! `judge_does_not_move_a_single_byte_of_the_deterministic_report` in
//! `tests/check_integration.rs`, which asserts that against a live mock.

use std::time::Duration;

use jig_core::judge::{
    JudgeConfig, JudgeReport, Question, ToolVerdict, JUDGE_MAX_TOKENS, JUDGE_TEMPERATURE,
};
use jig_core::{BenchModel, Provider};
use serde_json::{json, Value};

/// The result of an attempted judge pass: a report, or the one-line reason the
/// judge was unavailable.
///
/// `Err` is *not* a check failure. It is rendered as a single line and the
/// check proceeds to its usual exit code — honesty rule 4, graceful absence.
pub(crate) type JudgeOutcome = Result<JudgeReport, String>;

/// The `--judge` flag family, as clap parsed it.
#[derive(Debug, Clone, Default)]
pub(crate) struct JudgeOptions {
    /// `--judge`: opt in. **Everything else here is inert when this is false.**
    pub enabled: bool,
    /// `--judge-model <ID>`: the model id to judge with.
    pub model: Option<String>,
    /// `--api-model <STRING>`: the concrete model string on the wire.
    pub api_model: Option<String>,
    /// `--base-url` / `--no-auth`, shared with `jig bench`.
    pub endpoint: crate::bench::Endpoint,
}

/// Resolve the model + credential and run one judge pass.
///
/// Every failure mode — an unknown model id, a missing key, `--no-auth` without
/// `--base-url`, a provider 500, a timeout — comes back as `Err(reason)` for
/// the caller to print in one line.
pub(crate) async fn run(
    tools: &[jig_core::Tool],
    options: &JudgeOptions,
    timeout_secs: u64,
) -> JudgeOutcome {
    let model_id = options
        .model
        .clone()
        .unwrap_or_else(|| options.endpoint.default_model().to_string());
    let mut model = BenchModel::resolve(&model_id).map_err(|e| e.to_string())?;
    if let Some(api_model) = &options.api_model {
        model = model.with_api_model(api_model.clone());
    }
    // Key resolution is `jig bench`'s, verbatim: a vendor key from the
    // environment, or nothing at all under `--no-auth --base-url`.
    let api_key = options.endpoint.resolve_key(model.provider)?;

    let config = JudgeConfig {
        model,
        temperature: JUDGE_TEMPERATURE,
        max_tokens: JUDGE_MAX_TOKENS,
        timeout: (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs)),
        base_url: options.endpoint.effective_base_url(),
        api_key,
    };
    jig_core::judge::run_judge(tools, &config)
        .await
        .map_err(|e| e.to_string())
}

/// The sentence attached to every judged rendering, human and JSON alike.
///
/// The sibling caveat is not decoration. `distinguishes_siblings` is judged
/// against *this server's* tools only, but a real client loads several servers
/// at once — so a `yes` means "distinct here", never "safe from collision".
/// Without that sentence the verdict is the feature's largest overclaim.
pub(crate) const OUTSIDE_RUBRIC_NOTE: &str =
    "Judged output is outside rubric-v1.3: it is reported, never scored, and \
     changed nothing above. `distinguishes siblings` compares this server's \
     tools only — a client loading several servers can still see collisions \
     this judge cannot observe.";

/// The one line printed when the judge could not run.
fn unavailable_line(reason: &str) -> String {
    format!(
        "Description judge: unavailable — {reason}.\n  \
         The report above is the deterministic score and is unaffected.\n"
    )
}

// ---------------------------------------------------------------------------
// Human section
// ---------------------------------------------------------------------------

/// Render the human judged section — pure over the outcome, so it is
/// snapshot-lockable.
///
/// The section is titled and separated so no reader can mistake it for part of
/// the graded report, and it restates its provenance (model *as reported*,
/// prompt version, temperature, endpoint) on its own header line.
pub(crate) fn render_section(outcome: &JudgeOutcome) -> String {
    let report = match outcome {
        Ok(r) => r,
        Err(reason) => return unavailable_line(reason),
    };

    let mut s = String::new();
    s.push_str("Description judge (opt-in · never scored)\n");
    s.push_str(&format!(
        "  model {} · prompt {} · temperature {} · {}{}\n",
        report.model_label(),
        report.prompt_version,
        trim_float(report.temperature),
        report.endpoint,
        if report.keyless { " (keyless)" } else { "" },
    ));

    let width = Question::all()
        .iter()
        .map(|q| q.label().chars().count())
        .max()
        .unwrap_or(0);

    // A reply that was prose (or otherwise not the requested object) fails
    // *every* tool for the same reason. Printing that reason once, above the
    // tool list, is the same information without N copies of the excerpt.
    if let Some(detail) = whole_reply_unparseable(report) {
        s.push_str("\n  ! the model's reply was not the requested structure, so no tool\n");
        s.push_str("    could be judged. Nothing was guessed.\n");
        s.push_str(&format!("    {detail}\n"));
        s.push_str(&format!("\n  {OUTSIDE_RUBRIC_NOTE}\n"));
        return s;
    }

    for j in &report.judgements {
        s.push_str(&format!("\n  {}\n", j.tool));
        match &j.verdict {
            ToolVerdict::Judged { .. } => {
                for q in Question::all() {
                    let Some(a) = j.verdict.answer(q) else {
                        continue;
                    };
                    let line = format!(
                        "    {} {:<width$}  {:<7}  {}",
                        a.answer.glyph(),
                        q.label(),
                        a.answer.tag(),
                        a.rationale,
                        width = width,
                    );
                    s.push_str(line.trim_end());
                    s.push('\n');
                }
            }
            ToolVerdict::Unparseable { detail } => {
                s.push_str(&format!("    ! unparseable — {detail}\n"));
            }
            ToolVerdict::NotJudged => {
                s.push_str("    · not judged — the model's reply did not cover this tool\n");
            }
        }
    }

    s.push_str(&format!("\n  {OUTSIDE_RUBRIC_NOTE}\n"));
    s
}

/// The shared detail when *every* tool came back unparseable for the *same*
/// reason — i.e. the reply as a whole was not the requested structure. `None`
/// when the tools disagree, which is the per-tool case worth listing per tool.
///
/// This only ever collapses the *presentation*. The `judged` JSON key still
/// carries one explicit verdict per tool.
fn whole_reply_unparseable(report: &JudgeReport) -> Option<&str> {
    let first = match report.judgements.first()?.verdict {
        ToolVerdict::Unparseable { ref detail } => detail.as_str(),
        _ => return None,
    };
    report
        .judgements
        .iter()
        .all(|j| matches!(&j.verdict, ToolVerdict::Unparseable { detail } if detail == first))
        .then_some(first)
}

/// Format a float without a trailing `.0` for whole numbers.
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ---------------------------------------------------------------------------
// The `judged` JSON key
// ---------------------------------------------------------------------------

/// The value of the top-level `judged` key.
///
/// It carries everything needed to reproduce (or dismiss) the verdict: the
/// pinned prompt version, the verbatim system prompt and user message, the
/// exact request body, the temperature, the endpoint, the model **the provider
/// reported**, the raw response, and `"scored": false` stated outright.
pub(crate) fn render_json(outcome: &JudgeOutcome) -> Value {
    let report = match outcome {
        Ok(r) => r,
        Err(reason) => {
            return json!({
                "available": false,
                "reason": reason,
                "scored": false,
                "rubricNote": OUTSIDE_RUBRIC_NOTE,
            })
        }
    };

    json!({
        "available": true,
        "scored": false,
        "rubricNote": OUTSIDE_RUBRIC_NOTE,
        "promptVersion": report.prompt_version,
        "systemPrompt": report.system_prompt,
        "userPrompt": report.user_prompt,
        "renderedRequest": report.rendered_request,
        "provider": provider_tag(report.provider),
        "requestedModel": report.requested_model,
        // Null when the provider named no model. Never backfilled from
        // `requestedModel`: an unknown judge model is recorded as unknown.
        "reportedModel": report.reported_model,
        "temperature": report.temperature,
        "endpoint": report.endpoint,
        "keyless": report.keyless,
        "usage": {
            "inputTokens": report.usage.input_tokens,
            "outputTokens": report.usage.output_tokens,
            "totalTokens": report.usage.total_tokens,
        },
        "responseText": report.response_text,
        "rawResponse": report.raw_response,
        "summary": {
            "toolsJudged": report.judged_count(),
            "toolsNotJudged": report.not_judged_count(),
            "toolsUnparseable": report.unparseable_count(),
            "questions": Question::all().iter().map(|q| {
                let (yes, no, unclear, unparseable) = report.tally(*q);
                json!({
                    "question": q.key(),
                    "label": q.label(),
                    "yes": yes,
                    "no": no,
                    "unclear": unclear,
                    "unparseable": unparseable,
                })
            }).collect::<Vec<_>>(),
        },
        "tools": report.judgements.iter().map(tool_json).collect::<Vec<_>>(),
    })
}

fn tool_json(j: &jig_core::judge::ToolJudgement) -> Value {
    let mut doc = json!({ "tool": j.tool, "verdict": j.verdict.tag() });
    let map = doc.as_object_mut().expect("object literal");
    match &j.verdict {
        ToolVerdict::Judged { .. } => {
            for q in Question::all() {
                if let Some(a) = j.verdict.answer(q) {
                    map.insert(
                        q.key().to_string(),
                        json!({ "verdict": a.answer.tag(), "rationale": a.rationale }),
                    );
                }
            }
        }
        ToolVerdict::Unparseable { detail } => {
            map.insert("detail".to_string(), json!(detail));
        }
        ToolVerdict::NotJudged => {
            map.insert(
                "detail".to_string(),
                json!("the model's reply did not cover this tool"),
            );
        }
    }
    doc
}

fn provider_tag(p: Provider) -> &'static str {
    p.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::judge::{parse_judgements, JUDGE_PROMPT_VERSION, JUDGE_SYSTEM_PROMPT};
    use jig_core::Usage;

    /// A report fixture covering all three verdict shapes at once: a judged
    /// tool, a tool the model skipped, and a tool whose entry was malformed.
    fn fixture() -> JudgeReport {
        let text = r#"{"judgements":[
            {"tool":"echo",
             "states_purpose":{"verdict":"yes","rationale":"Says it returns the input text verbatim."},
             "distinguishes_siblings":{"verdict":"no","rationale":"Never contrasts with make_reservation."},
             "parameters_sufficient":{"verdict":"yes","rationale":"`text` is described."}},
            {"tool":"always_fails",
             "states_purpose":{"verdict":"unclear","rationale":"Says it fails, not what it would do."}}
        ]}"#;
        let names = ["echo", "make_reservation", "always_fails"].map(str::to_string);
        JudgeReport {
            prompt_version: JUDGE_PROMPT_VERSION,
            system_prompt: JUDGE_SYSTEM_PROMPT,
            user_prompt: "Tools (3): […]".to_string(),
            rendered_request: json!({ "model": "mock-judge-1" }),
            provider: Provider::OpenAI,
            requested_model: "gpt-4o".to_string(),
            reported_model: Some("mock-judge-1".to_string()),
            temperature: JUDGE_TEMPERATURE,
            endpoint: "http://127.0.0.1:0/judge_ok/v1/chat/completions".to_string(),
            keyless: true,
            judgements: parse_judgements(text, &names),
            response_text: "{…}".to_string(),
            raw_response: json!({ "model": "mock-judge-1" }),
            usage: Usage {
                input_tokens: Some(42),
                output_tokens: Some(7),
                total_tokens: Some(49),
            },
        }
    }

    #[test]
    fn human_section_snapshot() {
        insta::assert_snapshot!("judge_human", render_section(&Ok(fixture())));
    }

    #[test]
    fn human_section_unavailable_snapshot() {
        insta::assert_snapshot!(
            "judge_human_unavailable",
            render_section(&Err(
                "ANTHROPIC_API_KEY is not set (pass --base-url + --no-auth for a local model)"
                    .to_string()
            ))
        );
    }

    #[test]
    fn json_shape_snapshot() {
        let doc = serde_json::to_string_pretty(&render_json(&Ok(fixture()))).unwrap();
        insta::assert_snapshot!("judge_json", doc);
    }

    #[test]
    fn json_unavailable_states_the_reason_and_that_nothing_was_scored() {
        let v = render_json(&Err("provider request failed".to_string()));
        assert_eq!(v["available"], false);
        assert_eq!(v["scored"], false);
        assert_eq!(v["reason"], "provider request failed");
    }

    /// The judged JSON must never claim a model the provider did not name.
    #[test]
    fn an_unreported_model_stays_null_and_is_labelled_as_such() {
        let mut report = fixture();
        report.reported_model = None;
        assert!(render_json(&Ok(report.clone()))["reportedModel"].is_null());
        assert!(render_section(&Ok(report)).contains("unreported by provider"));
    }

    #[test]
    fn every_rendering_states_it_is_outside_the_rubric() {
        let human = render_section(&Ok(fixture()));
        assert!(human.contains("never scored"));
        assert!(human.contains(OUTSIDE_RUBRIC_NOTE));
        assert_eq!(render_json(&Ok(fixture()))["scored"], false);
    }
}
