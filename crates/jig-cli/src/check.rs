//! `jig check` — the one-command **report card**.
//!
//! Runs a single connect-inspect-budget session, feeds the collected data to the
//! pure scoring engine in [`jig_core::check`], and renders a scored verdict: a
//! composite grade, per-dimension lines with ✓/⚠/✗, and a ranked "Top fixes"
//! to-do list. All rendering is a pure function of a [`CheckReport`], so the
//! human report is snapshot-locked.
//!
//! Flags beyond the shared connection ones: `--min-score <n>` (exit nonzero when
//! the composite is below `n` — the CI gate), `--badge` (emit shields.io
//! endpoint JSON), and `--percentiles <file>` (override the ecosystem dataset
//! path, default `data/percentiles.json`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use jig_core::check::{ContextProvenance, DimensionScore, Finding, Severity};
use jig_core::{
    self, badge_color, evaluate, CheckInput, CheckReport, JigError, Observations, Percentiles,
    PollutionSite, ProtocolTap,
};
use serde_json::{json, Value};

use crate::{emit, emit_line, warn_non_protocol_output, write_tap_if_requested, Target};

/// The default ecosystem-percentiles path, relative to the working directory.
const DEFAULT_PERCENTILES_PATH: &str = "data/percentiles.json";

/// Run `jig check`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    as_json: bool,
    badge: bool,
    min_score: Option<f64>,
    percentiles_path: Option<PathBuf>,
    tap_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    // Load the optional ecosystem dataset. An explicit --percentiles that does
    // not exist is a user error; the default path silently falling back to
    // absolute bands is the normal case.
    let percentiles = load_percentiles(percentiles_path.as_deref())?;

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        percentiles.as_ref(),
        as_json,
        badge,
        min_score,
        timeout_secs,
        max_message_bytes,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

/// Resolve the percentiles dataset: an explicit path must exist and parse; the
/// default path is best-effort (absent → `None` → absolute bands).
fn load_percentiles(explicit: Option<&Path>) -> Result<Option<Percentiles>, String> {
    match explicit {
        Some(path) => match Percentiles::load(path) {
            Ok(Some(p)) => Ok(Some(p)),
            Ok(None) => Err(format!(
                "--percentiles {} has no usable `context_cost_tokens.samples`",
                path.display()
            )),
            Err(e) => Err(format!(
                "failed to read --percentiles {}: {e}",
                path.display()
            )),
        },
        None => Ok(Percentiles::load(DEFAULT_PERCENTILES_PATH).unwrap_or(None)),
    }
}

/// Build the error for a server that failed to connect/handshake. For a stdio
/// target this is a *startup failure*; when the ecosystem dataset carries a
/// `startup_failure_rate` we append one line of cohort context so the failure
/// reads against the wider ecosystem rather than in isolation. Silent (just the
/// raw error) when the target is HTTP or the dataset lacks the field.
fn startup_failure_message(
    target: &Target,
    err: String,
    percentiles: Option<&Percentiles>,
) -> String {
    let is_stdio = matches!(target, Target::Stdio { .. });
    match percentiles.and_then(Percentiles::startup_failure_note) {
        Some(note) if is_stdio => format!("{err}\n  {note}"),
        _ => err,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    target: &Target,
    tap: ProtocolTap,
    percentiles: Option<&Percentiles>,
    as_json: bool,
    badge: bool,
    min_score: Option<f64>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = match target
        .connect(tap.clone(), timeout_secs, max_message_bytes)
        .await
    {
        Ok(client) => client,
        // A stdio server that dies before/during the handshake is a startup
        // failure. Add one line of ecosystem cohort context when the dataset
        // carries it (silent otherwise) — our strongest census signal.
        Err(e) => return Err(startup_failure_message(target, e, percentiles)),
    };

    // The one session: time the list operation, then read the surface. A list
    // that the server accepts but never answers is a timeout, not a hard error —
    // capture it as an observation and score with an empty surface.
    let t0 = Instant::now();
    let (tools, list_timed_out) = match client.list_tools().await {
        Ok(tools) => (tools, false),
        Err(JigError::Timeout { .. }) => (Vec::new(), true),
        Err(e) => return Err(format!("tools/list failed: {e}")),
    };
    let list_latency = Some(t0.elapsed());

    // Error-code correctness on unknown methods (conformance: negative).
    let unknown_method = client.probe_unknown_method().await;

    let server = client.server_info().clone();
    let protocol_version = client.protocol_version().to_string();
    let capabilities = client.capabilities().clone();
    let instructions = client.instructions().map(str::to_string);

    // Clean shutdown is itself an observed robustness signal.
    let clean_shutdown = client.shutdown().await.is_ok();

    // Pollution is whatever non-protocol noise the tap captured this session.
    // The detailed view carries the byte offset + first bytes of the first
    // offending line so the finding can point at the exact break.
    let polluting = tap.non_protocol_inbound_detailed();
    let pollution_lines = polluting.len();
    let first_pollution = polluting.first().map(|l| PollutionSite {
        offset: l.offset,
        line: l.raw.clone(),
    });

    let input = CheckInput {
        server_name: server.name.clone(),
        server_version: server.version.clone(),
        protocol_version,
        capabilities,
        instructions,
        tools,
        observations: Observations {
            pollution_lines,
            first_pollution,
            list_timed_out,
            list_latency,
            clean_shutdown,
            // Child stderr volume is not plumbed through the client; left
            // unobserved rather than assumed.
            stderr_noise_bytes: None,
            unknown_method,
        },
    };

    let report = evaluate(&input, percentiles);

    if badge {
        emit_line(&render_badge(&report));
    } else if as_json {
        emit(&render_json(&report));
    } else {
        emit(&render_human(&report));
    }

    // The CI gate: exit nonzero when the composite is below the floor.
    if let Some(min) = min_score {
        if (report.composite_rounded() as f64) < min {
            if !as_json && !badge {
                eprintln!(
                    "jig: score {} is below --min-score {}",
                    report.composite_rounded(),
                    trim_float(min)
                );
            }
            return Ok(ExitCode::from(1));
        }
    }

    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Human report
// ---------------------------------------------------------------------------

/// The status glyph for a dimension score: ✓ ≥85, ⚠ 60–84, ✗ <60, · excluded.
fn glyph(score: Option<f64>) -> char {
    match score {
        None => '·',
        Some(s) if s >= 85.0 => '✓',
        Some(s) if s >= 60.0 => '⚠',
        Some(_) => '✗',
    }
}

/// The letter grade for a composite score.
fn grade(score: u32) -> char {
    match score {
        90..=u32::MAX => 'A',
        80..=89 => 'B',
        70..=79 => 'C',
        60..=69 => 'D',
        _ => 'F',
    }
}

/// Render the full human report — pure over the [`CheckReport`], so it is
/// snapshot-lockable.
pub(crate) fn render_human(report: &CheckReport) -> String {
    let mut s = String::new();

    // Header.
    s.push_str(&format!(
        "jig check · {} v{}\n",
        report.server_name, report.server_version
    ));
    s.push_str(&format!(
        "protocol {} · {}\n\n",
        report.protocol_version, report.rubric_version
    ));

    // Big composite score.
    let composite = report.composite_rounded();
    s.push_str(&format!(
        "  {}  {} / 100   grade {}\n\n",
        glyph(Some(report.composite)),
        composite,
        grade(composite)
    ));

    // Per-dimension lines, in rubric order.
    let label_width = report
        .dimensions
        .iter()
        .map(|d| d.dimension.label().chars().count())
        .max()
        .unwrap_or(0);
    for d in &report.dimensions {
        s.push_str(&dimension_line(d, label_width));
    }

    // Tool-set advisor: emergent, cross-tool problems the per-tool dimensions
    // can't see. Rendered only when it has something to say.
    if let Some(section) = crate::advisor_view::render_section(&report.advisor) {
        s.push('\n');
        s.push_str(&section);
    }

    // Top fixes.
    let fixes = report.top_fixes(3);
    if !fixes.is_empty() {
        s.push_str("\nTop fixes\n");
        for (i, f) in fixes.iter().enumerate() {
            s.push_str(&format!(
                "  {}. [{}] {}\n     → {}\n",
                i + 1,
                f.dimension.key(),
                f.message,
                f.fix
            ));
        }
    }

    // Footer: heuristic + provenance caveats.
    s.push('\n');
    s.push_str(&footer(report));
    s
}

/// One aligned dimension line: `⚠  Schema hygiene       94   <summary>`.
fn dimension_line(d: &DimensionScore, label_width: usize) -> String {
    let score = match d.score {
        Some(v) => format!("{:>3}", v.round() as i64),
        None => "  –".to_string(),
    };
    format!(
        "  {}  {:<label_width$}  {}   {}\n",
        glyph(d.score),
        d.dimension.label(),
        score,
        d.summary,
        label_width = label_width,
    )
}

/// The caveat footer: which dimensions are heuristic and how context cost was
/// scored.
fn footer(report: &CheckReport) -> String {
    let mut notes: Vec<String> = Vec::new();
    if report.has_heuristic_dimension() {
        notes.push("Description quality is heuristic (deterministic, no LLM).".to_string());
    }
    match &report.context_provenance {
        ContextProvenance::Percentile {
            percentile,
            n,
            collected,
        } => {
            let when = collected
                .as_deref()
                .map(|c| format!(", collected {c}"))
                .unwrap_or_default();
            notes.push(format!(
                "Context cost scored against {n} ecosystem servers{when} — {}th percentile.",
                percentile
            ));
        }
        ContextProvenance::AbsoluteBands => {
            notes.push(
                "Context cost scored with absolute bands (no ecosystem dataset available)."
                    .to_string(),
            );
        }
    }
    notes.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Badge (shields.io endpoint JSON)
// ---------------------------------------------------------------------------

/// Render the shields.io endpoint JSON for the composite score.
fn render_badge(report: &CheckReport) -> String {
    let score = report.composite_rounded();
    let doc = json!({
        "schemaVersion": 1,
        "label": "jig score",
        "message": score.to_string(),
        "color": badge_color(score),
    });
    serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// JSON report
// ---------------------------------------------------------------------------

/// Render the full machine-readable report: per-dimension scores + weights,
/// every finding, the composite, the rubric version, and percentile provenance.
fn render_json(report: &CheckReport) -> String {
    let dimensions: Vec<Value> = report.dimensions.iter().map(dimension_json).collect();
    let top_fixes: Vec<Value> = report.top_fixes(3).into_iter().map(finding_json).collect();

    let doc = json!({
        "server": { "name": report.server_name, "version": report.server_version },
        "protocolVersion": report.protocol_version,
        "rubricVersion": report.rubric_version,
        "composite": report.composite_rounded(),
        "compositeExact": round2(report.composite),
        "grade": grade(report.composite_rounded()).to_string(),
        "toolCount": report.tool_count,
        "contextCost": {
            "totalTokens": report.total_tokens,
            "model": "gpt-4o",
            "provenance": provenance_json(&report.context_provenance),
        },
        "dimensions": dimensions,
        "advisor": report.advisor.iter().map(finding_json).collect::<Vec<_>>(),
        "topFixes": top_fixes,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

fn dimension_json(d: &DimensionScore) -> Value {
    json!({
        "dimension": d.dimension.key(),
        "label": d.dimension.label(),
        "score": d.score.map(|v| v.round() as i64),
        "scoreExact": d.score.map(round2),
        "weight": d.weight,
        "heuristic": d.heuristic,
        "applicable": d.score.is_some(),
        "summary": d.summary,
        "findings": d.findings.iter().map(finding_json).collect::<Vec<_>>(),
    })
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "dimension": f.dimension.key(),
        "severity": severity_tag(f.severity),
        "message": f.message,
        "fix": f.fix,
        "points": round2(f.points),
    })
}

fn provenance_json(p: &ContextProvenance) -> Value {
    match p {
        ContextProvenance::Percentile {
            percentile,
            n,
            collected,
        } => json!({
            "type": "percentile",
            "percentile": percentile,
            "n": n,
            "collected": collected,
        }),
        ContextProvenance::AbsoluteBands => json!({ "type": "absolute_bands" }),
    }
}

fn severity_tag(s: Severity) -> &'static str {
    s.tag()
}

/// Round to 2 decimals for stable JSON.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Format a float without a trailing `.0` for whole numbers (for the gate msg).
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::check::{Dimension, MetricSamples};
    use jig_core::Tool;
    use serde_json::json;

    fn tool(name: &str, desc: Option<&str>, schema: Value) -> Tool {
        let mut m = serde_json::Map::new();
        m.insert("name".to_string(), json!(name));
        if let Some(d) = desc {
            m.insert("description".to_string(), json!(d));
        }
        m.insert("inputSchema".to_string(), schema);
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    /// The mock-server surface as fixture data, kept in sync with the mock's
    /// `handle_tools_list`. Deterministic (fixed latency) so the report is
    /// snapshot-stable.
    fn mock_input() -> CheckInput {
        CheckInput {
            server_name: "jig-mock-server".to_string(),
            server_version: "0.1.0".to_string(),
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({ "tools": {} }),
            instructions: Some("A toy MCP server for exercising Jig.".to_string()),
            tools: vec![
                tool(
                    "echo",
                    Some("Echo the provided text straight back."),
                    json!({ "type": "object", "properties": { "text": { "type": "string", "description": "Text to echo." } }, "required": ["text"] }),
                ),
                tool(
                    "make_reservation",
                    Some("Book a table. Demonstrates a nested object argument and an enum."),
                    json!({ "type": "object", "properties": {
                        "party": { "type": "object", "properties": { "size": { "type": "integer", "minimum": 1 }, "seating": { "type": "string", "enum": ["indoor", "outdoor", "bar"] } }, "required": ["size"] },
                        "date": { "type": "string", "description": "ISO-8601 date." }
                    }, "required": ["party", "date"] }),
                ),
                tool(
                    "always_fails",
                    Some("A tool that always reports an error, for testing error paths."),
                    json!({ "type": "object", "properties": {} }),
                ),
            ],
            observations: Observations {
                pollution_lines: 0,
                list_latency: Some(std::time::Duration::from_millis(12)),
                clean_shutdown: true,
                unknown_method: jig_core::UnknownMethodProbe::Errored(-32601),
                ..Default::default()
            },
        }
    }

    /// A degraded surface: stdout pollution + an off-spec capability + a missing
    /// param type — the deductions the integration test asserts on.
    fn degraded_input() -> CheckInput {
        let mut input = mock_input();
        input.capabilities = json!({ "tools": {}, "tasks": {} });
        input.observations.pollution_lines = 1;
        input.observations.first_pollution = Some(jig_core::PollutionSite {
            offset: Some(128),
            line: "[info] listening on :3000".to_string(),
        });
        // Break echo's `text` param: no type, no description.
        input.tools[0] = tool(
            "echo",
            Some("Echo the provided text straight back."),
            json!({ "type": "object", "properties": { "text": {} } }),
        );
        input
    }

    /// A tool titled, described (non-terse, non-verbose) and annotated, with no
    /// parameters — so it draws *no* per-tool dimension finding and any advisor
    /// finding stands alone in the report.
    fn clean_tool(name: &str, desc: &str) -> Tool {
        let mut m = serde_json::Map::new();
        m.insert("name".to_string(), json!(name));
        m.insert("title".to_string(), json!(format!("The {name} tool")));
        m.insert("description".to_string(), json!(desc));
        m.insert(
            "inputSchema".to_string(),
            json!({ "type": "object", "properties": {}, "annotations": {} }),
        );
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    /// A fixture that fires every analyzer-1 rule plus the accuracy cliff: a
    /// synonym collision (`get_status`/`fetch_status`), a generic-subset pair
    /// (`get_user`/`get_user_info`), a description near-duplicate pair, and 31
    /// tools total (past the ~30 cliff). The 25 filler tools carry unique,
    /// non-overlapping descriptions so they draw no advisories of their own.
    fn advisor_input() -> CheckInput {
        let mut tools = vec![
            clean_tool(
                "get_status",
                "Return the current status of the primary job.",
            ),
            clean_tool(
                "fetch_status",
                "Retrieve the present status of the primary job.",
            ),
            clean_tool("get_user", "Return the user account record by identifier."),
            clean_tool(
                "get_user_info",
                "Return the user account record plus contact number.",
            ),
            clean_tool(
                "list_reports",
                "alpha bravo charlie delta echo foxtrot golf hotel unique1",
            ),
            clean_tool(
                "list_summaries",
                "alpha bravo charlie delta echo foxtrot golf hotel unique2",
            ),
        ];
        for i in 0..25 {
            tools.push(clean_tool(
                &format!("filler_{i:02}"),
                &format!("uniqueverb{i} uniquenoun{i} uniqueadj{i} uniqueadverb{i} distinctly"),
            ));
        }
        CheckInput {
            tools,
            ..mock_input()
        }
    }

    #[test]
    fn human_report_advisor_snapshot() {
        let report = evaluate(&advisor_input(), None);
        // The advisor fired all four expected classes.
        assert!(report
            .advisor
            .iter()
            .any(|f| f.message.contains("cannot reliably distinguish")));
        assert!(report.advisor.iter().any(|f| f.message.contains("generic")));
        assert!(report.advisor.iter().any(|f| f.message.contains("overlap")));
        assert!(report
            .advisor
            .iter()
            .any(|f| f.message.contains("tools exposed")));
        insta::assert_snapshot!("check_human_advisor", render_human(&report));
    }

    #[test]
    fn glyph_thresholds() {
        assert_eq!(glyph(Some(100.0)), '✓');
        assert_eq!(glyph(Some(85.0)), '✓');
        assert_eq!(glyph(Some(84.0)), '⚠');
        assert_eq!(glyph(Some(60.0)), '⚠');
        assert_eq!(glyph(Some(59.0)), '✗');
        assert_eq!(glyph(None), '·');
    }

    #[test]
    fn grade_bands() {
        assert_eq!(grade(95), 'A');
        assert_eq!(grade(85), 'B');
        assert_eq!(grade(72), 'C');
        assert_eq!(grade(61), 'D');
        assert_eq!(grade(40), 'F');
    }

    #[test]
    fn badge_json_is_shields_endpoint() {
        let report = evaluate(&mock_input(), None);
        let badge = render_badge(&report);
        let v: Value = serde_json::from_str(&badge).unwrap();
        assert_eq!(v["schemaVersion"], 1);
        assert_eq!(v["label"], "jig score");
        assert!(v["message"].is_string());
        assert_eq!(v["color"], "brightgreen"); // clean server grades A
    }

    #[test]
    fn human_report_clean_snapshot() {
        let report = evaluate(&mock_input(), None);
        insta::assert_snapshot!("check_human_clean", render_human(&report));
    }

    #[test]
    fn human_report_degraded_snapshot() {
        let report = evaluate(&degraded_input(), None);
        insta::assert_snapshot!("check_human_degraded", render_human(&report));
    }

    #[test]
    fn human_report_percentile_snapshot() {
        let p = Percentiles {
            context_cost_tokens: MetricSamples {
                samples: vec![50.0, 100.0, 150.0, 5000.0, 90000.0],
            },
            collected: Some("2026-07-19".to_string()),
            census_date: Some("2026-07-19".to_string()),
            startup_failure_rate: None,
        };
        let report = evaluate(&mock_input(), Some(&p));
        insta::assert_snapshot!("check_human_percentile", render_human(&report));
    }

    #[test]
    fn json_report_snapshot() {
        let report = evaluate(&mock_input(), None);
        insta::assert_snapshot!("check_json", render_json(&report));
    }

    #[test]
    fn degraded_report_shows_the_right_findings() {
        let report = evaluate(&degraded_input(), None);
        let json = render_json(&report);
        assert!(json.contains("non-protocol line"));
        assert!(json.contains("tasks"));
        assert!(json.contains("missing a type"));
        // The pollution fix ranks first (highest weighted impact).
        let fixes = report.top_fixes(3);
        assert_eq!(fixes[0].dimension, Dimension::Protocol);
    }
}
