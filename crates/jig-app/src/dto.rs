//! Serializable views of `jig-core`'s analysis types.
//!
//! Almost nothing in `jig-core`'s analysis tree (`check`, `context`, `tokens`,
//! `discovery`) derives `Serialize` — those types are engine internals, and the
//! CLI hand-writes its `--json` shapes. This module does the same job for the
//! webview, and deliberately **mirrors the CLI's key names exactly**
//! (`crates/jig-cli/src/check.rs::render_json`) so that a number shown in the
//! app and the same number from `jig check --json` can never drift apart.
//!
//! Everything here is a pure function of its input, so all of it is testable
//! without a webview or a server.

use jig_core::check::{ContextCap, ContextProvenance, DimensionScore, Finding, Report, Severity};
use jig_core::context::ContextView;
use jig_core::discovery::{DiscoveredTransport, Discovery};
use jig_core::{Implementation, JigError};
use serde::Serialize;
use serde_json::Value;

/// Round to two decimals, matching the CLI's `round2`.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The composite -> letter ladder. Mirrors `jig-cli/src/report.rs::grade` and
/// `jig-core`'s own banding: `A >= 90 · B 80-89 · C 70-79 · D 60-69 · F < 60`.
pub fn grade(score: u32) -> char {
    match score {
        90..=u32::MAX => 'A',
        80..=89 => 'B',
        70..=79 => 'C',
        60..=69 => 'D',
        _ => 'F',
    }
}

/// The band a *dimension* score falls in: teal >=75, amber >=50, red below.
/// Mirrors `jig-cli/src/report.rs::band_var`. Returned as a token name the
/// stylesheet resolves, so the thresholds live in Rust and never in the UI.
pub fn band(score: f64) -> &'static str {
    if score >= 75.0 {
        "ok"
    } else if score >= 50.0 {
        "warn"
    } else {
        "bad"
    }
}

/// The band for a *composite*, keyed off the letter grade rather than the
/// number — again matching the report card, where A/B are teal, C/D amber,
/// F red.
pub fn grade_band(g: char) -> &'static str {
    match g {
        'A' | 'B' => "ok",
        'C' | 'D' => "warn",
        _ => "bad",
    }
}

/// The CLI's severity tag: `high` | `medium` | `low` | `info`.
fn severity_tag(s: Severity) -> &'static str {
    match s {
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Info => "info",
    }
}

/// Severity collapsed to the report card's three pill classes — `Low` and
/// `Info` share one, as in `report.rs::sev_class`.
fn severity_class(s: Severity) -> &'static str {
    match s {
        Severity::High => "h",
        Severity::Medium => "m",
        Severity::Low | Severity::Info => "l",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingDto {
    pub dimension: String,
    /// The stable machine-readable finding class (`jig_core::FindingCode`).
    pub code: String,
    pub severity: String,
    /// The pill class the stylesheet uses (`h`/`m`/`l`).
    pub severity_class: String,
    pub message: String,
    pub fix: String,
    pub points: f64,
}

pub fn finding_dto(f: &Finding) -> FindingDto {
    FindingDto {
        dimension: f.dimension.key().to_string(),
        code: f.code.as_str().to_string(),
        severity: severity_tag(f.severity).to_string(),
        severity_class: severity_class(f.severity).to_string(),
        message: f.message.clone(),
        fix: f.fix.clone(),
        points: round2(f.points),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DimensionDto {
    pub dimension: String,
    pub label: String,
    pub score: Option<i64>,
    pub score_exact: Option<f64>,
    pub weight: u32,
    pub heuristic: bool,
    pub applicable: bool,
    /// `ok` / `warn` / `bad`, or `None` when the dimension does not apply.
    pub band: Option<String>,
    pub summary: String,
    pub findings: Vec<FindingDto>,
}

pub fn dimension_dto(d: &DimensionScore) -> DimensionDto {
    DimensionDto {
        dimension: d.dimension.key().to_string(),
        label: d.dimension.label().to_string(),
        score: d.score.map(|v| v.round() as i64),
        score_exact: d.score.map(round2),
        weight: d.weight,
        heuristic: d.heuristic,
        applicable: d.score.is_some(),
        band: d.score.map(|v| band(v).to_string()),
        summary: d.summary.clone(),
        findings: d.findings.iter().map(finding_dto).collect(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCapDto {
    pub cap: f64,
    pub uncapped: f64,
    pub context_score: f64,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceDto {
    #[serde(rename = "type")]
    pub kind: String,
    pub percentile: Option<u32>,
    pub n: Option<usize>,
    pub collected: Option<String>,
    pub bundled: Option<bool>,
    /// The one-line prose the report card shows under the context bill. Built
    /// here so the app and the HTML report say the same thing.
    pub prose: String,
}

pub fn provenance_dto(p: &ContextProvenance) -> ProvenanceDto {
    match p {
        ContextProvenance::Percentile {
            percentile,
            n,
            collected,
            bundled,
        } => {
            let census = if *bundled {
                "jig's bundled census"
            } else {
                "jig's ecosystem census"
            };
            let when = collected
                .as_ref()
                .map(|c| format!(", collected {c}"))
                .unwrap_or_default();
            let comparison = if *percentile >= 50 {
                format!("heavier than ~{percentile}%")
            } else {
                format!("lighter than ~{}%", 100 - percentile)
            };
            ProvenanceDto {
                kind: "percentile".to_string(),
                percentile: Some(*percentile),
                n: Some(*n),
                collected: collected.clone(),
                bundled: Some(*bundled),
                prose: format!(
                    "Injected before the user's first word. Against {census} of {n} public servers{when}, this is {comparison} of the measured ecosystem."
                ),
            }
        }
        ContextProvenance::AbsoluteBands => ProvenanceDto {
            kind: "absolute_bands".to_string(),
            percentile: None,
            n: None,
            collected: None,
            bundled: None,
            prose: "Injected before the user's first word. Scored on documented absolute token bands — no ecosystem census was loaded for a percentile comparison.".to_string(),
        },
    }
}

/// One bar in the per-tool cost chart, pre-normalised against the heaviest tool.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCostDto {
    pub name: String,
    pub tokens: usize,
    /// Width as a percentage of the heaviest tool, matching the HTML report's
    /// max-normalised chart (not the identity scaling the dimension bars use).
    pub fill_pct: f64,
}

/// The median tool cost, defined exactly as the report card and the advisor
/// define it: the lower-middle element for an even count.
pub fn median_tokens(sorted_desc: &[usize]) -> usize {
    if sorted_desc.is_empty() {
        return 0;
    }
    let mut v: Vec<usize> = sorted_desc.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    }
}

/// The number of tools the chart shows, matching `report.rs::CHART_TOOLS`.
pub const CHART_TOOLS: usize = 12;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartDto {
    pub tools: Vec<ToolCostDto>,
    pub shown: usize,
    pub total: usize,
    pub median: usize,
    /// Position of the median reference line, as a percentage of the track.
    pub median_pct: f64,
    /// `top tool is N× median`, or `None` when the median is zero.
    pub top_ratio: Option<f64>,
}

/// Build the per-tool chart: sorted by tokens descending, ties by name
/// ascending, truncated to [`CHART_TOOLS`] — the report card's exact rules.
pub fn chart_dto(per_tool: &[(String, usize)]) -> ChartDto {
    let mut ranked: Vec<(String, usize)> = per_tool.to_vec();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let max = ranked.first().map(|(_, t)| *t).unwrap_or(0).max(1) as f64;
    let all_tokens: Vec<usize> = ranked.iter().map(|(_, t)| *t).collect();
    let median = median_tokens(&all_tokens);
    let shown = ranked.len().min(CHART_TOOLS);

    let tools = ranked
        .iter()
        .take(CHART_TOOLS)
        .map(|(name, tok)| ToolCostDto {
            name: name.clone(),
            tokens: *tok,
            fill_pct: (*tok as f64 / max) * 100.0,
        })
        .collect();

    let top_ratio = if median > 0 {
        ranked
            .first()
            .map(|(_, t)| round2(*t as f64 / median as f64))
    } else {
        None
    };

    ChartDto {
        tools,
        shown,
        total: ranked.len(),
        median,
        median_pct: (median as f64 / max) * 100.0,
        top_ratio,
    }
}

/// The graded verdict, in the shape the Report-card pane renders.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDto {
    pub server: ServerDto,
    pub protocol_version: String,
    pub rubric_version: String,
    pub composite: u32,
    pub composite_exact: f64,
    pub grade: String,
    /// `ok` / `warn` / `bad` for the hero number.
    pub grade_band: String,
    pub tool_count: usize,
    pub total_tokens: usize,
    pub provenance: ProvenanceDto,
    pub context_cap: Option<ContextCapDto>,
    pub dimensions: Vec<DimensionDto>,
    pub advisor: Vec<FindingDto>,
    pub top_fixes: Vec<FindingDto>,
    pub chart: ChartDto,
    /// The footer's honesty notes, assembled by the same rules as the HTML
    /// report — the app must be exactly as candid about its own limits.
    pub honesty_notes: Vec<String>,
    /// True when the advisor fired the tool-count callout, gating the second
    /// callout exactly as `report.rs::render_callouts` does.
    pub tool_count_callout: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDto {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

impl ServerDto {
    pub fn from_implementation(i: &Implementation) -> Self {
        Self {
            name: i.name.clone(),
            version: i.version.clone(),
            title: i.title.clone(),
        }
    }
}

/// The HTML report shows five top fixes; the terminal and JSON show three. The
/// app is a reading surface like the report card, so it takes five.
pub const TOP_FIXES: usize = 5;

/// Assemble the footer's honesty notes, mirroring `report.rs::render_footer`.
pub fn honesty_notes(report: &Report) -> Vec<String> {
    let mut notes = Vec::new();
    if report.has_heuristic_dimension() {
        notes
            .push("Description quality is deterministic heuristics (no LLM judgment).".to_string());
    }
    match &report.context_provenance {
        ContextProvenance::Percentile {
            n,
            collected,
            bundled,
            ..
        } => {
            let which = if *bundled {
                "jig's bundled census"
            } else {
                "an ecosystem dataset"
            };
            let when = collected
                .as_ref()
                .map(|c| format!(", collected {c}"))
                .unwrap_or_default();
            notes.push(format!(
                "Context cost was scored against {which} (n={n}{when}); a percentile is only as trustworthy as its sample."
            ));
        }
        ContextProvenance::AbsoluteBands => notes.push(
            "Context cost was scored with documented absolute bands (no ecosystem census loaded)."
                .to_string(),
        ),
    }
    notes.push(format!(
        "Protocol compliance and robustness reflect only what was observed in one session. Rubric weights are fitted to a 63-server fleet census ({}).",
        report.rubric_version
    ));
    notes
}

pub fn report_dto(report: &Report) -> ReportDto {
    let composite = report.composite_rounded();
    let g = grade(composite);
    ReportDto {
        server: ServerDto {
            name: report.server_name.clone(),
            version: report.server_version.clone(),
            title: None,
        },
        protocol_version: report.protocol_version.clone(),
        rubric_version: report.rubric_version.to_string(),
        composite,
        composite_exact: round2(report.composite),
        grade: g.to_string(),
        grade_band: grade_band(g).to_string(),
        tool_count: report.tool_count,
        total_tokens: report.total_tokens,
        provenance: provenance_dto(&report.context_provenance),
        context_cap: report
            .context_cap
            .as_ref()
            .map(|c: &ContextCap| ContextCapDto {
                cap: c.cap,
                uncapped: round2(c.uncapped),
                context_score: round2(c.context_score),
                explanation: c.explanation.clone(),
            }),
        dimensions: report.dimensions.iter().map(dimension_dto).collect(),
        advisor: report.advisor.iter().map(finding_dto).collect(),
        top_fixes: report
            .top_fixes(TOP_FIXES)
            .into_iter()
            .map(finding_dto)
            .collect(),
        chart: chart_dto(&report.per_tool_tokens),
        honesty_notes: honesty_notes(report),
        tool_count_callout: report
            .advisor
            .iter()
            .any(|f| f.message.contains("tools exposed")),
    }
}

// ---------------------------------------------------------------------------
// Context & budget
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolContextDto {
    pub name: String,
    pub description: Option<String>,
    pub tokens: usize,
    pub schema_lines: Vec<String>,
    /// Share of the tools' total token cost, for the inline bar.
    pub share_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDto {
    pub model_id: String,
    pub api_model: String,
    pub tokenizer: String,
    /// `exact` or `~approx`, the tag `tokens::Exactness` already defines.
    pub exactness: String,
    pub system_tokens: usize,
    pub tools_tokens: usize,
    pub total_tokens: usize,
    /// Present when the server sent `instructions`. Note these are counted but
    /// deliberately excluded from `total_tokens` — bench does not send them —
    /// which the UI states rather than hiding.
    pub instructions_tokens: Option<usize>,
    pub tools: Vec<ToolContextDto>,
    /// The exact provider request body, for the raw view.
    pub body: Value,
}

pub fn context_dto(view: &ContextView) -> ContextDto {
    let denom = view.tools_tokens.max(1) as f64;
    ContextDto {
        model_id: view.model_id.clone(),
        api_model: view.api_model.clone(),
        tokenizer: view.tokenizer.clone(),
        exactness: view.exactness.tag().to_string(),
        system_tokens: view.system_tokens,
        tools_tokens: view.tools_tokens,
        total_tokens: view.total_tokens,
        instructions_tokens: view.instructions.as_ref().map(|i| i.tokens),
        tools: view
            .tools
            .iter()
            .map(|t| ToolContextDto {
                name: t.name.clone(),
                description: t.description.clone(),
                tokens: t.tokens,
                schema_lines: t.schema_lines.clone(),
                share_pct: (t.tokens as f64 / denom) * 100.0,
            })
            .collect(),
        body: view.body.clone(),
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredServerDto {
    pub name: String,
    pub source: String,
    pub source_file: String,
    /// `stdio` or `http`.
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub disabled: bool,
    /// A one-line `stdio: cmd args` / `http: url` summary, from core.
    pub summary: String,
    /// Env var **names** only. Values are never sent to the webview: they are
    /// real API keys read off the user's disk, and the webview has no business
    /// holding them.
    pub env_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryDto {
    pub entries: Vec<DiscoveredServerDto>,
    pub warnings: Vec<String>,
}

pub fn discovery_dto(d: &Discovery) -> DiscoveryDto {
    DiscoveryDto {
        entries: d
            .entries
            .iter()
            .map(|e| {
                let (transport, command, args, url) = match &e.transport {
                    DiscoveredTransport::Stdio { command, args } => {
                        ("stdio", Some(command.clone()), args.clone(), None)
                    }
                    DiscoveredTransport::Http { url } => {
                        ("http", None, Vec::new(), Some(url.clone()))
                    }
                };
                DiscoveredServerDto {
                    name: e.name.clone(),
                    source: e.source.slug().to_string(),
                    source_file: e.source_file.display().to_string(),
                    transport: transport.to_string(),
                    command,
                    args,
                    url,
                    disabled: e.disabled,
                    summary: e.transport_summary(),
                    // `env_display` redacts values; we keep only the keys.
                    env_keys: e.env_display().into_iter().map(|(k, _)| k).collect(),
                }
            })
            .collect(),
        warnings: d.warnings.clone(),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Render a [`JigError`] the way the CLI does — the specific, actionable
/// sentence, never a generic failure.
///
/// One deliberate divergence: `MessageTooLarge`'s `Display` names the CLI flag
/// `--max-message-bytes`, which does not exist in a GUI. The app points at its
/// own control instead. Every other variant is passed through verbatim so the
/// two surfaces read identically.
pub fn error_message(e: &JigError) -> String {
    match e {
        JigError::MessageTooLarge { limit } => format!(
            "inbound message exceeded the maximum size of {limit} bytes (raise the limit in Connect > Advanced, or set it to 0 to disable it)"
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::check::{Dimension, FindingCode};

    #[test]
    fn grade_bands_match_the_documented_ladder() {
        assert_eq!(grade(100), 'A');
        assert_eq!(grade(90), 'A');
        assert_eq!(grade(89), 'B');
        assert_eq!(grade(80), 'B');
        assert_eq!(grade(79), 'C');
        assert_eq!(grade(70), 'C');
        assert_eq!(grade(69), 'D');
        assert_eq!(grade(60), 'D');
        assert_eq!(grade(59), 'F');
        assert_eq!(grade(0), 'F');
    }

    #[test]
    fn dimension_bands_use_the_report_cards_75_50_thresholds() {
        // Deliberately different from the grade ladder — do not unify them.
        assert_eq!(band(75.0), "ok");
        assert_eq!(band(74.9), "warn");
        assert_eq!(band(50.0), "warn");
        assert_eq!(band(49.9), "bad");
    }

    #[test]
    fn grade_band_keys_off_the_letter_not_the_number() {
        assert_eq!(grade_band('A'), "ok");
        assert_eq!(grade_band('B'), "ok");
        assert_eq!(grade_band('C'), "warn");
        assert_eq!(grade_band('D'), "warn");
        assert_eq!(grade_band('F'), "bad");
    }

    #[test]
    fn median_takes_the_lower_middle_on_an_even_count() {
        // Matches the advisor's and the report card's definition.
        assert_eq!(median_tokens(&[10, 20, 30, 40]), 25);
        assert_eq!(median_tokens(&[10, 20, 30]), 20);
        assert_eq!(median_tokens(&[]), 0);
        assert_eq!(median_tokens(&[7]), 7);
    }

    #[test]
    fn chart_sorts_by_tokens_desc_then_name_asc_and_truncates() {
        let per_tool: Vec<(String, usize)> =
            (0..20).map(|i| (format!("tool_{i:02}"), 100 - i)).collect();
        let chart = chart_dto(&per_tool);
        assert_eq!(chart.total, 20);
        assert_eq!(chart.shown, CHART_TOOLS);
        assert_eq!(chart.tools.len(), CHART_TOOLS);
        assert_eq!(chart.tools[0].name, "tool_00");
        assert_eq!(chart.tools[0].tokens, 100);
        // The heaviest tool defines the full-width bar.
        assert!((chart.tools[0].fill_pct - 100.0).abs() < 1e-9);
    }

    #[test]
    fn chart_breaks_token_ties_by_name_ascending() {
        let per_tool = vec![
            ("zebra".to_string(), 50),
            ("alpha".to_string(), 50),
            ("mid".to_string(), 50),
        ];
        let chart = chart_dto(&per_tool);
        let names: Vec<&str> = chart.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn an_empty_chart_does_not_divide_by_zero() {
        let chart = chart_dto(&[]);
        assert_eq!(chart.total, 0);
        assert_eq!(chart.median, 0);
        assert!(chart.top_ratio.is_none());
        assert!(chart.median_pct.is_finite());
    }

    #[test]
    fn severity_tags_and_classes_match_the_cli_and_the_report_card() {
        assert_eq!(severity_tag(Severity::High), "high");
        assert_eq!(severity_tag(Severity::Info), "info");
        // Low and Info deliberately share a pill class.
        assert_eq!(severity_class(Severity::Low), "l");
        assert_eq!(severity_class(Severity::Info), "l");
        assert_eq!(severity_class(Severity::High), "h");
        assert_eq!(severity_class(Severity::Medium), "m");
    }

    #[test]
    fn absolute_bands_provenance_says_no_census_was_loaded() {
        let p = provenance_dto(&ContextProvenance::AbsoluteBands);
        assert_eq!(p.kind, "absolute_bands");
        assert!(p.percentile.is_none());
        assert!(p.prose.contains("absolute token bands"));
    }

    #[test]
    fn percentile_provenance_flips_its_comparison_at_the_midpoint() {
        let heavy = provenance_dto(&ContextProvenance::Percentile {
            percentile: 80,
            n: 300,
            collected: Some("2026-05".to_string()),
            bundled: true,
        });
        assert!(heavy.prose.contains("heavier than ~80%"), "{}", heavy.prose);
        assert!(heavy.prose.contains("collected 2026-05"));

        let light = provenance_dto(&ContextProvenance::Percentile {
            percentile: 20,
            n: 300,
            collected: None,
            bundled: false,
        });
        assert!(light.prose.contains("lighter than ~80%"), "{}", light.prose);
    }

    #[test]
    fn finding_dto_carries_the_fix_because_the_fix_is_the_product() {
        let f = Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolStdoutPollution,
            severity: Severity::High,
            message: "stdout pollution".to_string(),
            fix: "log to stderr".to_string(),
            points: 12.345,
            rank_points: None,
            pinned: true,
        };
        let dto = finding_dto(&f);
        assert_eq!(dto.dimension, "protocol");
        assert_eq!(dto.code, "protocol.stdout_pollution");
        assert_eq!(dto.severity, "high");
        assert_eq!(dto.severity_class, "h");
        assert_eq!(dto.fix, "log to stderr");
        assert_eq!(dto.points, 12.35, "points are rounded to 2dp like the CLI");
    }

    #[test]
    fn message_too_large_is_rewritten_for_a_gui_and_others_are_verbatim() {
        let too_big = JigError::MessageTooLarge { limit: 1024 };
        let msg = error_message(&too_big);
        assert!(msg.contains("1024"));
        assert!(
            !msg.contains("--max-message-bytes"),
            "the app must not name a CLI flag: {msg}"
        );
        assert!(
            msg.contains("Connect"),
            "it must name the app's own control"
        );

        // Every other variant keeps the CLI's exact wording.
        let server = JigError::Server {
            code: -32601,
            message: "method not found".to_string(),
            data: None,
        };
        assert_eq!(error_message(&server), server.to_string());
        let transport = JigError::Transport("broken pipe".to_string());
        assert_eq!(error_message(&transport), "transport error: broken pipe");
    }
}
