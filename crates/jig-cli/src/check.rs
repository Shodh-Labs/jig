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
//! endpoint JSON), `--percentiles <file>` (override the ecosystem dataset; the
//! census is otherwise bundled into the binary, and `none` opts out to absolute
//! bands), and `--report <file>`/`--no-report` (the HTML report card, written by
//! default in human mode).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use jig_core::check::{
    fleet_spread, ContextProvenance, DimensionScore, DimensionSpread, Finding, Severity,
};
use jig_core::{
    self, badge_color, evaluate, grade_startup, BootTiming, CheckInput, CheckReport, JigError,
    Observations, Percentiles, PollutionSite, ProtocolTap, StartupVerdict,
};
use serde_json::{json, Value};

use crate::judge_view::{JudgeOptions, JudgeOutcome};
use crate::{emit, emit_line, warn_non_protocol_output, write_tap_if_requested, Target};

/// The `--percentiles` sentinel that opts out of percentile scoring entirely,
/// forcing the documented absolute bands (no ecosystem comparison).
const PERCENTILES_NONE_SENTINEL: &str = "none";

/// Run `jig check`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    target: &Target,
    as_json: bool,
    badge: bool,
    min_score: Option<f64>,
    percentiles_path: Option<PathBuf>,
    report_path: Option<PathBuf>,
    no_report: bool,
    tap_path: Option<&Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
    no_prewarm: bool,
    judge: JudgeOptions,
) -> Result<ExitCode, String> {
    // Load the ecosystem dataset. With no --percentiles the bundled census is
    // used; `--percentiles none` opts out; an explicit missing file is an error.
    let percentiles = load_percentiles(percentiles_path.as_deref())?;

    let tap = ProtocolTap::new();
    let result = run_inner(
        target,
        tap.clone(),
        percentiles.as_ref(),
        as_json,
        badge,
        min_score,
        report_path,
        no_report,
        timeout_secs,
        max_message_bytes,
        no_prewarm,
        judge,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

/// Decide where the HTML report should be written, if anywhere.
///
/// * An explicit `--report <file>` always writes there (any output mode).
/// * `--no-report` suppresses it.
/// * Otherwise the human mode writes `./jig-report-<server>.html`; the machine
///   modes (`--json`/`--badge`) write nothing unless a path was given.
fn report_destination(
    report_path: Option<PathBuf>,
    no_report: bool,
    as_json: bool,
    badge: bool,
    server_name: &str,
) -> Option<PathBuf> {
    if let Some(p) = report_path {
        return Some(p);
    }
    if no_report || as_json || badge {
        return None;
    }
    Some(PathBuf::from(crate::report::ReportMeta::default_filename(
        server_name,
    )))
}

/// Render and write the HTML report to `dest`, then announce it: a clean
/// `report: <path>` line on stdout in human mode (so it reads as part of the
/// terminal output), or a stderr note in machine mode (never corrupting the JSON
/// on stdout). A write failure is a stderr warning, never a check failure.
fn write_report(report: &CheckReport, target: &Target, dest: &Path, human: bool) {
    let meta = crate::report::ReportMeta {
        transport: target.transport_label().to_string(),
        command_line: target.check_command_line(),
        date: crate::report::today_utc(),
        jig_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let html = crate::report::render(report, &meta);
    if let Err(e) = std::fs::write(dest, html) {
        eprintln!(
            "jig: warning: could not write report to {}: {e}",
            dest.display()
        );
        return;
    }
    let shown = display_path(dest);
    if human {
        emit_line(&format!("report: {shown}"));
    } else {
        eprintln!("jig: wrote report to {shown}");
    }
}

/// A friendly display path: a bare relative filename gets a `./` prefix so it is
/// obviously a path, otherwise the path is shown as-is.
fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if path.is_relative() && !s.contains('/') && !s.contains('\\') {
        format!("./{s}")
    } else {
        s.into_owned()
    }
}

/// Resolve the percentiles dataset. With no `--percentiles`, the census bundled
/// into the binary is used (so `npx`/installed runs still score against the
/// ecosystem). `--percentiles none` opts out to absolute bands. An explicit file
/// path must exist and parse — an explicit missing/unusable file is a hard error.
pub(crate) fn load_percentiles(explicit: Option<&Path>) -> Result<Option<Percentiles>, String> {
    match explicit {
        // The opt-out sentinel: force absolute bands, no ecosystem comparison.
        Some(path) if path == Path::new(PERCENTILES_NONE_SENTINEL) => Ok(None),
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
        // Default: the census embedded at compile time.
        None => Ok(jig_core::bundled_percentiles()),
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

/// Run the one check session against `target` and score it.
///
/// This is the whole of `jig check` minus presentation: connect, time the list,
/// probe the unknown-method error code, read the surface, shut down cleanly,
/// fold in whatever pollution the tap saw, and hand the result to the pure
/// scoring engine. Factored out because `jig serve` exposes the very same
/// measurement as the `check_server` MCP tool — the CLI verb and the MCP tool
/// must grade identically or the score means nothing.
pub(crate) async fn observe_and_evaluate(
    target: &Target,
    tap: &ProtocolTap,
    percentiles: Option<&Percentiles>,
    timeout_secs: u64,
    max_message_bytes: u64,
    no_prewarm: bool,
) -> Result<(CheckReport, Vec<jig_core::Tool>), String> {
    // SOP 25: populate the npx cache *before* timing the launch, so the boot
    // number is the server's and not the registry's. Non-npx commands and
    // `--no-prewarm` skip straight through. See [`jig_core::boot`].
    // `rubric-v1.4`: the pass also returns the measured launcher floor — a null
    // program timed through the same `npx` path on a warm cache — which is
    // subtracted from boot so the graded number is the server's.
    let (install, launcher) = match target {
        Target::Stdio { program, args, .. } if !no_prewarm => {
            crate::startup::prewarm(program, args).await
        }
        _ => (None, None),
    };

    // Boot is launch -> `initialize` response, with the cache already warm.
    let boot_start = Instant::now();
    let client = match target
        .connect(tap.clone(), timeout_secs, max_message_bytes)
        .await
    {
        Ok(client) => client,
        // A stdio server that dies before/during the handshake is a startup
        // failure. Add one line of ecosystem cohort context when the dataset
        // carries it, and — SOP 26 — re-launch once to grade *how* it failed,
        // because "names the missing variable" and "hangs forever" are wildly
        // different products that this message previously rendered identically.
        Err(e) => {
            let base = startup_failure_message(target, e, percentiles);
            return Err(match target {
                Target::Stdio { program, args, env } => {
                    match crate::startup::probe_credential_failure(program, args, env).await {
                        Some(observation) => {
                            let verdict = grade_startup(&observation);
                            match verdict.finding() {
                                Some(f) => format!(
                                    "{base}
  {}
  → {}",
                                    verdict.line(),
                                    f.fix
                                ),
                                None => format!(
                                    "{base}
  {}",
                                    verdict.line()
                                ),
                            }
                        }
                        None => base,
                    }
                }
                Target::Http { .. } => base,
            });
        }
    };
    let boot = Some(boot_start.elapsed());
    let timing = BootTiming {
        install,
        boot,
        prewarm_skipped: no_prewarm && matches!(target, Target::Stdio { .. }),
        launcher,
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

    // Child stderr volume, read *before* shutdown while the transport (and its
    // drain task) still exist. `None` on HTTP, where there is no child stderr to
    // observe — an unknown volume, never an assumed zero. Informational only:
    // the stdio transport designates stderr for logging, so a server that writes
    // there is behaving correctly and this is reported, never scored.
    let stderr_noise_bytes = client.stderr_volume().map(|v| v.bytes);

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

    // The judge needs the surface the score was computed from — the *same*
    // tools, not a second listing — so it is returned alongside the report.
    // It is only ever read; nothing downstream can feed it back into scoring.
    let judged_tools = tools.clone();

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
            stderr_noise_bytes,
            unknown_method,
            // The server started, so there is no credential failure to grade
            // (SOP 26 applies only to the failure path above).
            startup: StartupVerdict::NotObserved,
            timing,
        },
    };

    Ok((evaluate(&input, percentiles), judged_tools))
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    target: &Target,
    tap: ProtocolTap,
    percentiles: Option<&Percentiles>,
    as_json: bool,
    badge: bool,
    min_score: Option<f64>,
    report_path: Option<PathBuf>,
    no_report: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
    no_prewarm: bool,
    judge: JudgeOptions,
) -> Result<ExitCode, String> {
    let (report, tools) = observe_and_evaluate(
        target,
        &tap,
        percentiles,
        timeout_secs,
        max_message_bytes,
        no_prewarm,
    )
    .await?;

    // Honesty rule 1: no judge call unless --judge was passed. The pass runs
    // *after* the report is fully computed, and its result is only ever
    // rendered — `report` is never rebuilt, re-scored, or consulted again.
    let judged: Option<JudgeOutcome> = if judge.enabled {
        Some(crate::judge_view::run(&tools, &judge, timeout_secs).await)
    } else {
        None
    };

    if badge {
        // The badge is the composite and nothing else; a judged run emits the
        // identical badge, which is the point.
        emit_line(&render_badge(&report));
    } else if as_json {
        emit(&render_json_with_judge(&report, judged.as_ref()));
    } else {
        emit(&render_human(&report));
        if let Some(outcome) = &judged {
            emit(&format!(
                "
{}",
                crate::judge_view::render_section(outcome)
            ));
        }
        // For an HTTP target, surface a compact, informational OAuth-conformance
        // section. The auth dimension is NOT scored into the rubric-v1.2 composite
        // in this milestone; it is a heads-up only. The probe reuses the session
        // tap, so its HTTP traffic is captured alongside the rest.
        if let Target::Http { url, headers } = target {
            let summary = crate::auth::check_summary(url, headers, &tap, timeout_secs).await;
            emit(&summary);
        }
    }

    // The shareable HTML report card: written by default in human mode (and on
    // demand via --report in any mode). Announced after the terminal output.
    if let Some(dest) =
        report_destination(report_path, no_report, as_json, badge, &report.server_name)
    {
        write_report(&report, target, &dest, !as_json && !badge);
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

/// The letter grade for a composite score: `A >= 90 · B 80–89 · C 70–79 ·
/// D 60–69 · F < 60` (`rubric-v1.1` — v1 documented `F < 40`, leaving 40–59
/// unbanded).
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

    // The context-cost cap (rubric-v1.2) is never applied silently: if the
    // composite was bounded, say so on its own line, right under the score.
    if let Some(cap) = &report.context_cap {
        s.push_str(&format!(
            "  ⓘ  {} (would have scored {})\n\n",
            cap.explanation,
            cap.uncapped.round() as i64
        ));
    }

    // The protocol-compliance ceiling (rubric-v1.3), on the same terms: a
    // bounded composite always states the ceiling and the defect that caused it.
    if let Some(cap) = &report.protocol_cap {
        s.push_str(&format!(
            "  ⓘ  {} (would have scored {})\n\n",
            cap.explanation,
            cap.uncapped.round() as i64
        ));
    }

    // The install/boot split (rubric-v1.3, SOP 25). Shown whenever a boot time
    // was measured, so the number that *is* graded is never confused with the
    // cold-start figure that is not.
    if report.timing.boot.is_some() {
        s.push_str(&format!(
            "  {}

",
            report.timing.line()
        ));
    }

    // Per-dimension lines, in rubric order.
    let label_width = report
        .dimensions
        .iter()
        .map(|d| d.dimension.label().chars().count())
        .max()
        .unwrap_or(0);
    let spread_width = report
        .dimensions
        .iter()
        .map(|d| spread_cell(fleet_spread(d.dimension)).chars().count())
        .max()
        .unwrap_or(0);
    for d in &report.dimensions {
        s.push_str(&dimension_line(d, label_width, spread_width));
    }

    // Tool-set advisor: emergent, cross-tool problems the per-tool dimensions
    // can't see. Rendered only when it has something to say.
    if let Some(section) = crate::advisor_view::render_section(&report.advisor) {
        s.push('\n');
        s.push_str(&section);
    }

    // Tool poisoning / prompt injection (rubric-v1.3). Unscored, but rendered
    // above "Top fixes" because it is a trust finding, not a quality one.
    if let Some(section) =
        crate::advisor_view::render_titled_section("Tool poisoning (unscored)", &report.injection)
    {
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

/// The fleet-spread cell for a dimension line: `[43·87·97]`, the census-v2
/// p25 · median · p75 (`rubric-v1.5`).
///
/// Rounded to whole numbers, because this is orientation and not arithmetic —
/// the exact decimals are in `--json` and in `data/dimension-spread.json`. Empty
/// when no spread is bundled for the dimension, so the line degrades to its
/// pre-`v1.5` shape rather than showing a hole.
fn spread_cell(spread: Option<DimensionSpread>) -> String {
    match spread {
        Some(s) => format!(
            "[{}·{}·{}]",
            s.p25.round() as i64,
            s.median.round() as i64,
            s.p75.round() as i64
        ),
        None => String::new(),
    }
}

/// One aligned dimension line:
/// `⚠  Schema hygiene       94  [96·99·100]  <summary>`.
///
/// The fleet spread sits between the score and the summary rather than at the
/// end of the line: the summary is variable-length, so anything after it would
/// be ragged, and the whole value of the spread is being able to read it *down*
/// the column against the scores beside it.
fn dimension_line(d: &DimensionScore, label_width: usize, spread_width: usize) -> String {
    let score = match d.score {
        Some(v) => format!("{:>3}", v.round() as i64),
        None => "  –".to_string(),
    };
    let spread = spread_cell(fleet_spread(d.dimension));
    // Padded by character count, not byte length — `·` is two bytes.
    let padding = " ".repeat(spread_width.saturating_sub(spread.chars().count()));
    format!(
        "  {}  {:<label_width$}  {}  {spread}{padding}  {}\n",
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
            bundled,
        } => {
            if *bundled {
                let when = collected
                    .as_deref()
                    .map(|c| format!(" {c}"))
                    .unwrap_or_default();
                notes.push(format!(
                    "Context cost scored against the bundled census{when} (n={n}) — {percentile}th percentile."
                ));
            } else {
                let when = collected
                    .as_deref()
                    .map(|c| format!(", collected {c}"))
                    .unwrap_or_default();
                notes.push(format!(
                    "Context cost scored against {n} ecosystem servers{when} — {percentile}th percentile."
                ));
            }
        }
        ContextProvenance::AbsoluteBands => {
            notes.push(
                "Context cost scored with absolute bands (no ecosystem dataset available)."
                    .to_string(),
            );
        }
    }
    // The fleet-spread legend (`rubric-v1.5`). Without it the bracket is three
    // unexplained numbers; with it, the reader can see which dimensions actually
    // separate servers — which is the entire point of showing it.
    if let Some(n) = report
        .dimensions
        .iter()
        .filter_map(|d| fleet_spread(d.dimension))
        .map(|s| s.n)
        .max()
    {
        notes.push(format!(
            "[p25·median·p75] is where {n} measured servers scored on that dimension — \
             a dimension whose three numbers sit together separates servers little. \
             Not scored."
        ));
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
pub(crate) fn render_json(report: &CheckReport) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&check_doc(report)).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Render the machine-readable report with an optional `judged` key attached.
///
/// When `judged` is `None` — the default, no `--judge` — the emitted bytes are
/// exactly [`render_json`]'s. When it is `Some`, a single top-level `judged`
/// key is **added** and nothing else is touched: the composite, every dimension
/// score, the grade and every finding are already fixed by the time this runs.
pub(crate) fn render_json_with_judge(
    report: &CheckReport,
    judged: Option<&JudgeOutcome>,
) -> String {
    let mut doc = check_doc(report);
    if let (Some(outcome), Some(map)) = (judged, doc.as_object_mut()) {
        map.insert(
            "judged".to_string(),
            crate::judge_view::render_json(outcome),
        );
    }
    format!(
        "{}
",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

/// The machine-readable check document, before any judged section is attached.
///
/// Split out from [`render_json`] so `--judge` can add its own top-level key
/// without the deterministic document being re-derived (or re-ordered) on the
/// judged path. Every byte here is a pure function of the [`CheckReport`], and
/// the [`CheckReport`] is computed from observations the judge never touches —
/// which is why `--judge` cannot move a single value in this object.
fn check_doc(report: &CheckReport) -> Value {
    let dimensions: Vec<Value> = report.dimensions.iter().map(dimension_json).collect();
    let top_fixes: Vec<Value> = report.top_fixes(3).into_iter().map(finding_json).collect();

    json!({
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
        // Present (non-null) only when the composite was actually bounded by
        // context cost — see `rubric-v1.2`.
        "contextCap": report.context_cap.as_ref().map(|c| json!({
            "cap": c.cap,
            "uncapped": round2(c.uncapped),
            "contextScore": round2(c.context_score),
            "explanation": c.explanation,
        })),
        // Present (non-null) only when a HIGH-severity protocol defect actually
        // bounded the composite - see `rubric-v1.3`.
        "protocolCap": report.protocol_cap.as_ref().map(|c| json!({
            "cap": c.cap,
            "uncapped": round2(c.uncapped),
            "highPoints": round2(c.high_points),
            "protocolScore": round2(c.protocol_score),
            "explanation": c.explanation,
        })),
        // The install/boot split (`rubric-v1.3`, SOP 25). `install` is null
        // when there was nothing to pre-warm.
        //
        // `rubric-v1.4` adds the two numbers that make the graded figure
        // checkable: `launcherSeconds`, the measured cost of a null program
        // through the same `npx` path, and `serverBootSeconds`, the difference —
        // which is what is actually scored. `bootSeconds` is retained unchanged
        // so a consumer can still see the raw launch.
        "timing": {
            "installSeconds": report.timing.install.map(|d| round2(d.as_secs_f64())),
            "bootSeconds": report.timing.boot.map(|d| round2(d.as_secs_f64())),
            "launcherSeconds": report.timing.launcher.map(|d| round2(d.as_secs_f64())),
            "serverBootSeconds": report.timing.server_boot().map(|d| round2(d.as_secs_f64())),
            "prewarmSkipped": report.timing.prewarm_skipped,
            "scored": "serverBoot",
        },
        "dimensions": dimensions,
        "advisor": report.advisor.iter().map(finding_json).collect::<Vec<_>>(),
        // Tool-poisoning findings (`rubric-v1.3`). Reported, never scored.
        "injection": report.injection.iter().map(finding_json).collect::<Vec<_>>(),
        "topFixes": top_fixes,
    })
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
        // Where the measured fleet sat on this dimension (`rubric-v1.5`), so a
        // consumer can tell a score of 100 that beat the field from one that
        // merely matched it. Purely additive: it is bundled reference data, not
        // a property of *this* server, and it enters no score.
        "fleetSpread": fleet_spread(d.dimension).map(|s| json!({
            "p25": s.p25,
            "median": s.median,
            "p75": s.p75,
        })),
        "findings": d.findings.iter().map(finding_json).collect::<Vec<_>>(),
    })
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "dimension": f.dimension.key(),
        // The stable machine-readable class. Additive since `rubric-v1.5`;
        // consumers should key off this rather than parsing `message`.
        "code": f.code.as_str(),
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
            bundled,
        } => json!({
            "type": "percentile",
            "percentile": percentile,
            "n": n,
            "collected": collected,
            "bundled": bundled,
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
            json!({ "type": "object", "properties": {} }),
        );
        // A real annotation, as a *sibling* of `inputSchema` — where MCP puts
        // it. A `readOnlyHint` inside the schema would not annotate anything.
        m.insert("annotations".to_string(), json!({ "readOnlyHint": true }));
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
            bundled: false,
        };
        let report = evaluate(&mock_input(), Some(&p));
        insta::assert_snapshot!("check_human_percentile", render_human(&report));
    }

    #[test]
    fn json_report_snapshot() {
        let report = evaluate(&mock_input(), None);
        insta::assert_snapshot!("check_json", render_json(&report));
    }

    /// `--json` carries the stable machine-readable class alongside the prose.
    /// This is the field consumers key off instead of normalizing `message`, so
    /// its presence on *every* finding — dimension, advisor and injection — is
    /// the contract, and a couple of exact strings are pinned here too.
    #[test]
    fn the_json_report_carries_a_code_on_every_finding() {
        let report = evaluate(&degraded_input(), None);
        let v: Value = serde_json::from_str(&render_json(&report)).expect("valid JSON");

        let mut codes: Vec<String> = Vec::new();
        let mut collect = |arr: &Value| {
            for f in arr.as_array().into_iter().flatten() {
                let code = f["code"]
                    .as_str()
                    .unwrap_or_else(|| panic!("finding has no `code`: {f}"));
                assert!(!code.is_empty(), "finding has an empty `code`: {f}");
                // Additive only: the prose fields are still there.
                assert!(f["message"].is_string() && f["fix"].is_string());
                codes.push(code.to_string());
            }
        };
        for d in v["dimensions"].as_array().expect("dimensions") {
            collect(&d["findings"]);
        }
        collect(&v["topFixes"]);
        collect(&v["advisor"]);
        collect(&v["injection"]);

        assert!(!codes.is_empty(), "the degraded fixture emits findings");
        assert!(
            codes.iter().any(|c| c == "protocol.stdout_pollution"),
            "expected the pollution class, got {codes:?}"
        );
        assert!(
            codes.iter().any(|c| c == "protocol.offspec_capability"),
            "expected the off-spec capability class, got {codes:?}"
        );
        // Every code names its dimension, so a consumer can group without a
        // lookup table.
        for c in &codes {
            assert!(c.contains('.'), "`{c}` is not `<dimension>.<class>`");
        }
    }

    /// A census in which the server under test is heavier than every sample, so
    /// its context sub-score is catastrophic and the `rubric-v1.2` cap binds.
    fn capping_percentiles() -> Percentiles {
        Percentiles {
            context_cost_tokens: MetricSamples {
                samples: vec![1.0; 40],
            },
            collected: Some("2026-07-19".to_string()),
            census_date: Some("2026-07-19".to_string()),
            startup_failure_rate: None,
            bundled: true,
        }
    }

    /// The cap is never silent: the human report states it, names the token
    /// count, and shows what the server would otherwise have scored.
    #[test]
    fn human_report_states_the_context_cap() {
        let p = capping_percentiles();
        let report = evaluate(&mock_input(), Some(&p));
        let cap = report
            .context_cap
            .as_ref()
            .expect("a p100 server must be capped");
        let text = render_human(&report);
        assert!(
            text.contains(&cap.explanation),
            "the human report must carry the cap explanation verbatim:\n{text}"
        );
        assert!(text.contains("composite capped at 60 by context cost"));
        assert!(text.contains("would have scored"));
        // The rendered score is the capped one — and since `rubric-v1.5` the
        // ceiling a heavy surface can impose alone stops at the D band.
        assert!(text.contains("60 / 100   grade D"));
        assert!(
            text.contains("D floor"),
            "the floor must be stated where it bound:\n{text}"
        );
    }

    /// The cap is machine-readable too, and absent (null) when it did not fire.
    #[test]
    fn json_report_carries_the_context_cap() {
        let p = capping_percentiles();
        let capped: Value = serde_json::from_str(&render_json(&evaluate(&mock_input(), Some(&p))))
            .expect("valid JSON");
        assert_eq!(capped["composite"], 60);
        assert_eq!(capped["grade"], "D");
        assert_eq!(capped["contextCap"]["cap"], 60.0);
        assert!(capped["contextCap"]["uncapped"].as_f64().unwrap() > 60.0);
        assert!(capped["contextCap"]["explanation"]
            .as_str()
            .unwrap()
            .contains("composite capped at 60"));

        let clean: Value =
            serde_json::from_str(&render_json(&evaluate(&mock_input(), None))).expect("valid JSON");
        assert!(
            clean["contextCap"].is_null(),
            "an uncapped server reports contextCap: null"
        );
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

    // ---- check_doc: the machine-readable document, tested directly ----------

    /// The optional ceilings are emitted as JSON `null` — not omitted, and not
    /// fabricated — whenever they did not bind, so a consumer can rely on the key
    /// always being present. A clean server trips neither cap.
    #[test]
    fn check_doc_emits_null_caps_when_neither_ceiling_binds() {
        let report = evaluate(&mock_input(), None);
        let doc = check_doc(&report);
        assert!(doc["contextCap"].is_null(), "clean server: no context cap");
        assert!(
            doc["protocolCap"].is_null(),
            "clean server: no protocol cap"
        );
        // The composite in the doc is the rounded figure, and the grade agrees.
        assert_eq!(doc["composite"], report.composite_rounded());
        assert_eq!(doc["grade"], "A");
        assert_eq!(doc["rubricVersion"], "rubric-v1.5");
        assert_eq!(doc["contextCost"]["model"], "gpt-4o");
    }

    /// The fleet spread reaches `--json` as an additive per-dimension object
    /// (`rubric-v1.5`), carrying the exact decimals from
    /// `data/dimension-spread.json` — the human line rounds, the machine
    /// document does not.
    #[test]
    fn check_doc_carries_the_fleet_spread_for_every_dimension() {
        let doc = check_doc(&evaluate(&mock_input(), None));
        let dimensions = doc["dimensions"].as_array().expect("dimensions array");
        assert_eq!(dimensions.len(), 5);
        for d in dimensions {
            let spread = &d["fleetSpread"];
            assert!(
                !spread.is_null(),
                "{} carries no fleet spread",
                d["dimension"]
            );
            let (p25, median, p75) = (
                spread["p25"].as_f64().unwrap(),
                spread["median"].as_f64().unwrap(),
                spread["p75"].as_f64().unwrap(),
            );
            assert!(p25 <= median && median <= p75, "{spread} is out of order");
        }
        // Pinned against the dataset: protocol separates nobody, context cost
        // separates everybody. These are the two numbers the `rubric-v1.5`
        // weight change rests on, so they are asserted by value.
        let by_key = |key: &str| {
            dimensions
                .iter()
                .find(|d| d["dimension"] == key)
                .unwrap()
                .clone()
        };
        assert_eq!(by_key("protocol")["fleetSpread"]["p25"], 100.0);
        assert_eq!(by_key("protocol")["fleetSpread"]["p75"], 100.0);
        assert_eq!(by_key("context_cost")["fleetSpread"]["p25"], 43.1);
        assert_eq!(by_key("context_cost")["fleetSpread"]["median"], 87.1);
        assert_eq!(by_key("context_cost")["fleetSpread"]["p75"], 96.6);
        assert_eq!(by_key("robustness")["fleetSpread"]["median"], 95.3);
    }

    /// The spread is context, not verdict: it appears beside every dimension in
    /// the human report under a legend that says what it is, and never alters a
    /// score. The dimension weights are the `rubric-v1.5` set in the machine
    /// document too, so a consumer can re-derive the composite.
    #[test]
    fn the_human_report_explains_the_fleet_spread_and_the_json_weights_agree() {
        let report = evaluate(&mock_input(), None);
        let text = render_human(&report);
        assert!(text.contains("[100·100·100]"), "protocol spread:\n{text}");
        assert!(
            text.contains("[p25·median·p75] is where 63 measured servers scored"),
            "the legend must explain the bracket:\n{text}"
        );
        assert!(text.contains("Not scored."), "{text}");

        let doc = check_doc(&report);
        let weight = |key: &str| {
            doc["dimensions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["dimension"] == key)
                .unwrap()["weight"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(weight("protocol"), 15);
        assert_eq!(weight("context_cost"), 25);
        assert_eq!(weight("schema_hygiene"), 20);
        assert_eq!(weight("description_quality"), 15);
        assert_eq!(weight("robustness"), 25);
    }

    /// When the context cap actually binds, `check_doc` fills the `contextCap`
    /// object (cap, uncapped, explanation) — while `protocolCap` stays null,
    /// because a heavy surface is not a protocol defect.
    #[test]
    fn check_doc_populates_the_context_cap_only_when_it_binds() {
        let report = evaluate(&mock_input(), Some(&capping_percentiles()));
        let doc = check_doc(&report);
        let cap = &doc["contextCap"];
        assert!(!cap.is_null(), "a p100 server must be capped");
        assert_eq!(cap["cap"], 60.0);
        assert!(cap["uncapped"].as_f64().unwrap() > 60.0);
        assert!(cap["explanation"]
            .as_str()
            .unwrap()
            .contains("composite capped at 60"));
        // The context cap is not a protocol defect: that ceiling stays absent.
        assert!(doc["protocolCap"].is_null());
    }

    /// The `rubric-v1.4` timing split is serialized in full: the raw launch, the
    /// measured launcher floor, and the *scored* figure — boot minus floor. This
    /// pins the subtraction that makes the graded number checkable.
    #[test]
    fn check_doc_serializes_the_timing_split_including_the_launcher_floor() {
        let mut input = mock_input();
        input.observations.timing = BootTiming {
            install: Some(std::time::Duration::from_millis(7_400)),
            boot: Some(std::time::Duration::from_millis(900)),
            prewarm_skipped: false,
            launcher: Some(std::time::Duration::from_millis(600)),
        };
        let doc = check_doc(&evaluate(&input, None));
        let timing = &doc["timing"];
        assert_eq!(timing["installSeconds"], 7.4);
        assert_eq!(timing["bootSeconds"], 0.9);
        assert_eq!(timing["launcherSeconds"], 0.6);
        // The scored figure is the difference: 0.9s launch − 0.6s shim = 0.3s.
        assert_eq!(timing["serverBootSeconds"], 0.3);
        assert_eq!(timing["scored"], "serverBoot");
        assert_eq!(timing["prewarmSkipped"], false);
    }

    /// A non-npx server measures no launcher floor, so nothing is subtracted and
    /// the scored boot equals the raw boot — the `rubric-v1.3` behaviour, which
    /// `check_doc` must preserve when `launcher` is `None`.
    #[test]
    fn check_doc_scores_the_whole_boot_when_no_launcher_floor_was_measured() {
        let mut input = mock_input();
        input.observations.timing = BootTiming {
            install: None,
            boot: Some(std::time::Duration::from_millis(1_300)),
            prewarm_skipped: false,
            launcher: None,
        };
        let timing = check_doc(&evaluate(&input, None))["timing"].clone();
        assert!(timing["installSeconds"].is_null());
        assert!(timing["launcherSeconds"].is_null());
        assert_eq!(timing["bootSeconds"], 1.3);
        assert_eq!(timing["serverBootSeconds"], timing["bootSeconds"]);
    }

    // ---- observe_and_evaluate: the startup-failure path, tested directly ----

    /// A stdio target whose program cannot be spawned yields a *bare* startup
    /// failure: the connect error, with no credential-UX verdict appended.
    /// `probe_credential_failure` returns `None` for a process that never ran, so
    /// the message must not be dressed up with a verdict the rule cannot reach.
    #[tokio::test]
    async fn observe_and_evaluate_reports_a_bare_failure_when_the_program_cannot_spawn() {
        let target = crate::Target::Stdio {
            program: "jig-nonexistent-program-zzz".to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let tap = ProtocolTap::new();
        let err = observe_and_evaluate(&target, &tap, None, 1, 1 << 20, true)
            .await
            .expect_err("a missing program cannot be checked");
        assert!(err.contains("failed to connect"), "unexpected error: {err}");
        assert!(
            !err.contains("credential UX"),
            "a program that never spawned must draw no credential verdict: {err}"
        );
    }
}
