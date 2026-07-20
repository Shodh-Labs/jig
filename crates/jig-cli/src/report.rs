//! The **shareable HTML report card** — `jig check`'s Lighthouse-style artifact.
//!
//! [`render`] turns a finished [`CheckReport`] (plus a little session
//! [`ReportMeta`]) into a single self-contained HTML document: inline CSS, no
//! external assets, and **zero JavaScript** — every bar of the per-tool token
//! chart is a static `<div>` generated here in Rust, so the file renders
//! identically offline and is trivially snapshot-lockable.
//!
//! # Safety
//!
//! A report is opened in a browser, and its content includes **server-supplied
//! strings** (tool names, and finding text that embeds them). Every such string
//! is routed through [`html_escape`] before it reaches the document, so a hostile
//! server that names a tool `<script>…</script>` gets inert, escaped text — never
//! script execution. The XSS-escape guarantee is locked by a unit test.
//!
//! # Filenames
//!
//! The default output name is `jig-report-<sanitized-server-name>.html` in the
//! working directory. [`sanitize_server_name`] reduces the server name to
//! lowercase `[a-z0-9-]`, so a server name can never traverse paths or inject
//! shell metacharacters into the filename.

use std::time::{SystemTime, UNIX_EPOCH};

use jig_core::check::{ContextProvenance, DimensionScore, Finding, Severity};
use jig_core::CheckReport;

/// The session facts the report header needs that the [`CheckReport`] itself does
/// not carry: how we connected, the exact command, when, and the tool version.
pub(crate) struct ReportMeta {
    /// The transport label shown in the header (e.g. `stdio`, `streamable-http`).
    pub transport: String,
    /// The exact command line that produced this report (already reconstructed).
    pub command_line: String,
    /// The date the report was generated, `YYYY-MM-DD`.
    pub date: String,
    /// The `jig` binary version.
    pub jig_version: String,
}

impl ReportMeta {
    /// The default filename for `report`'s server: `jig-report-<name>.html`.
    pub(crate) fn default_filename(server_name: &str) -> String {
        format!("jig-report-{}.html", sanitize_server_name(server_name))
    }
}

/// How many prioritized fixes to list in the report's "Top fixes" section.
const TOP_FIXES: usize = 5;
/// How many tools to show in the per-tool token chart.
const CHART_TOOLS: usize = 12;

// ---------------------------------------------------------------------------
// Filename sanitization
// ---------------------------------------------------------------------------

/// Reduce a server name to a filesystem-safe slug: lowercase, keep ASCII
/// alphanumerics, map every other run to a single `-`, trim leading/trailing
/// `-`. An empty result falls back to `server`, so the filename is never a bare
/// `jig-report-.html` and can never traverse paths (`/`, `\`, `.` all collapse).
pub(crate) fn sanitize_server_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "server".to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// Escape the five HTML-significant characters so any server-supplied string is
/// inert text in the document.
pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Escape `s` for HTML, then render backtick-delimited spans (`` `name` ``, the
/// convention jig's finding text uses for tool names) as `<span class="mono">`.
/// Escaping runs **first**, so any markup inside the backticks is already inert
/// before the span wrapper is added — this can never re-introduce live markup.
/// An unmatched trailing backtick is emitted as a literal.
fn render_inline(s: &str) -> String {
    let escaped = html_escape(s);
    let mut out = String::with_capacity(escaped.len());
    let mut in_code = false;
    for ch in escaped.chars() {
        if ch == '`' {
            if in_code {
                out.push_str("</span>");
            } else {
                out.push_str("<span class=\"mono\">");
            }
            in_code = !in_code;
        } else {
            out.push(ch);
        }
    }
    if in_code {
        // Unbalanced backtick: close the span so the document stays well-formed.
        out.push_str("</span>");
    }
    out
}

// ---------------------------------------------------------------------------
// Small formatting helpers
// ---------------------------------------------------------------------------

/// Insert thousands separators: `12345` -> `12,345`.
fn commas(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let first = bytes.len() % 3;
    for (i, b) in bytes.iter().enumerate() {
        if i != 0 && i >= first && (i - first).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// The letter grade for a composite score (mirrors `check`'s human renderer).
fn grade(score: u32) -> char {
    match score {
        90..=u32::MAX => 'A',
        80..=89 => 'B',
        70..=79 => 'C',
        60..=69 => 'D',
        _ => 'F',
    }
}

/// The CSS band color for a dimension bar: teal ≥75, amber ≥50, red below.
fn band_var(score: f64) -> &'static str {
    if score >= 75.0 {
        "var(--ok)"
    } else if score >= 50.0 {
        "var(--warn)"
    } else {
        "var(--bad)"
    }
}

/// The CSS band color for the composite hero, by letter grade: A/B teal, C/D
/// amber, F red.
fn grade_var(g: char) -> &'static str {
    match g {
        'A' | 'B' => "var(--ok)",
        'C' | 'D' => "var(--warn)",
        _ => "var(--bad)",
    }
}

/// The severity pill class (`h`/`m`/`l`) for a finding.
fn sev_class(sev: Severity) -> &'static str {
    match sev {
        Severity::High => "h",
        Severity::Medium => "m",
        Severity::Low | Severity::Info => "l",
    }
}

/// Today's date as `YYYY-MM-DD` (UTC). Pure arithmetic on the Unix epoch (no
/// external date crate), via Howard Hinnant's `civil_from_days`.
pub(crate) fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a count of days since the Unix epoch into a `(year, month, day)`
/// civil date. Howard Hinnant's algorithm; valid across the whole `i64` range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Render the full self-contained HTML report for `report`. Pure over its inputs
/// (no I/O, no clock — [`ReportMeta::date`] is supplied), so it is snapshot-safe.
pub(crate) fn render(report: &CheckReport, meta: &ReportMeta) -> String {
    let composite = report.composite_rounded();
    let g = grade(composite);

    let mut s = String::with_capacity(8 * 1024);
    // A complete, standards-mode standalone document: the file is written to
    // disk and opened directly, so it carries its own doctype, charset and
    // viewport rather than relying on a host page to supply them.
    s.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    s.push_str("<meta charset=\"utf-8\">\n");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    s.push_str("<title>Jig Report Card — ");
    s.push_str(&html_escape(&report.server_name));
    s.push_str(" v");
    s.push_str(&html_escape(&report.server_version));
    s.push_str("</title>\n");
    s.push_str(STYLE);
    s.push_str("\n</head>\n<body>\n<main>\n");

    render_header(&mut s, report, meta);
    render_hero(&mut s, report, composite, g);
    render_callouts(&mut s, report);
    render_chart(&mut s, report);
    render_advisor(&mut s, report);
    render_top_fixes(&mut s, report);
    render_footer(&mut s, report, meta);

    s.push_str("</main>\n</body>\n</html>\n");
    s
}

fn render_header(s: &mut String, report: &CheckReport, meta: &ReportMeta) {
    s.push_str("  <header>\n");
    s.push_str("    <div class=\"eyebrow\">jig · MCP server report card</div>\n");
    s.push_str(&format!(
        "    <h1>{} v{}</h1>\n",
        html_escape(&report.server_name),
        html_escape(&report.server_version)
    ));
    s.push_str(&format!(
        "    <div class=\"meta\">{transport} · protocol {proto} · {tools} tool{plural} · \
         graded {date} · {rubric} · jig {jig}<br>\n    <span class=\"mono\">{cmd}</span></div>\n",
        transport = html_escape(&meta.transport),
        proto = html_escape(&report.protocol_version),
        tools = report.tool_count,
        plural = if report.tool_count == 1 { "" } else { "s" },
        date = html_escape(&meta.date),
        rubric = html_escape(report.rubric_version),
        jig = html_escape(&meta.jig_version),
        cmd = html_escape(&meta.command_line),
    ));
    s.push_str("  </header>\n");
}

fn render_hero(s: &mut String, report: &CheckReport, composite: u32, g: char) {
    let gvar = grade_var(g);
    s.push_str("  <div class=\"hero\">\n");
    s.push_str("    <div class=\"score\">\n");
    s.push_str(&format!(
        "      <div class=\"n\" style=\"color:{gvar}\">{composite}</div>\n"
    ));
    s.push_str(&format!(
        "      <div class=\"g\" style=\"background:{gvar}\">grade {g}</div>\n"
    ));
    s.push_str("      <div class=\"sub\">out of 100 · composite of 5 weighted dimensions</div>\n");
    s.push_str("    </div>\n    <div class=\"dims\">\n");
    for d in &report.dimensions {
        render_dim(s, d);
    }
    s.push_str("    </div>\n  </div>\n");
}

fn render_dim(s: &mut String, d: &DimensionScore) {
    let label = html_escape(d.dimension.label());
    match d.score {
        Some(score) => {
            let width = score.clamp(0.0, 100.0).round() as i64;
            let color = band_var(score);
            s.push_str(&format!(
                "      <div class=\"dim\"><span class=\"lbl\">{label} <small>·{weight}%</small></span>\
                 <span class=\"bar\"><i style=\"width:{width}%;background:{color}\"></i></span>\
                 <span class=\"v\">{value}</span></div>\n",
                weight = d.weight,
                value = score.round() as i64,
            ));
        }
        None => {
            // Not applicable to this server — an empty track and an em dash.
            s.push_str(&format!(
                "      <div class=\"dim\"><span class=\"lbl\">{label} <small>·{weight}%</small></span>\
                 <span class=\"bar\"></span><span class=\"v\">–</span></div>\n",
                weight = d.weight,
            ));
        }
    }
}

fn render_callouts(s: &mut String, report: &CheckReport) {
    s.push_str("  <div class=\"callouts\">\n");

    // Callout 1: the context bill — always shown.
    s.push_str("    <div class=\"co\">\n");
    s.push_str("      <h3>Context bill: every conversation starts here</h3>\n");
    s.push_str(&format!(
        "      <div class=\"big\">{} tokens</div>\n",
        commas(report.total_tokens)
    ));
    s.push_str(&format!("      <p>{}</p>\n", context_bill_prose(report)));
    s.push_str("    </div>\n");

    // Callout 2: the tool-count cliff — only when the advisor actually fired it.
    if report
        .advisor
        .iter()
        .any(|f| f.message.contains("tools exposed"))
    {
        s.push_str("    <div class=\"co\">\n");
        s.push_str("      <h3>Tool count: past the accuracy cliff</h3>\n");
        s.push_str(&format!(
            "      <div class=\"big\">{} tools</div>\n",
            report.tool_count
        ));
        s.push_str(
            "      <p>Published measurements show model tool-selection accuracy degrading \
             materially past ~30–50 tools. A mis-selected tool is a wrong <i>action</i>, not just \
             a wrong answer.</p>\n",
        );
        s.push_str("    </div>\n");
    }

    s.push_str("  </div>\n");
}

/// The context-bill callout's explanatory sentence, adapting to how context cost
/// was actually scored.
fn context_bill_prose(report: &CheckReport) -> String {
    match &report.context_provenance {
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
                .as_deref()
                .map(|c| format!(", {}", html_escape(c)))
                .unwrap_or_default();
            let comparison = if *percentile >= 50 {
                format!("heavier than ~{percentile}%")
            } else {
                format!("lighter than ~{}%", 100u32.saturating_sub(*percentile))
            };
            format!(
                "Injected before the user's first word. Against {census} of {n} public servers{when}, \
                 this is <b>{comparison}</b> of the measured ecosystem."
            )
        }
        ContextProvenance::AbsoluteBands => "Injected before the user's first word. Scored on \
             documented absolute token bands — no ecosystem census was loaded for a percentile \
             comparison."
            .to_string(),
    }
}

fn render_chart(s: &mut String, report: &CheckReport) {
    if report.per_tool_tokens.is_empty() {
        return;
    }
    // Rank a copy descending by tokens, ties by name, for a stable chart.
    let mut ranked: Vec<(&str, usize)> = report
        .per_tool_tokens
        .iter()
        .map(|(n, t)| (n.as_str(), *t))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let total = report.per_tool_tokens.len();
    let median = median_tokens(&report.per_tool_tokens);
    let max = ranked.first().map(|(_, t)| *t).unwrap_or(0).max(1) as f64;
    let shown = ranked.len().min(CHART_TOOLS);

    s.push_str("  <section>\n");
    s.push_str(&format!(
        "    <h2>Where the tokens go — top {shown} of {total} tool{plural}</h2>\n",
        plural = if total == 1 { "" } else { "s" },
    ));
    s.push_str(&format!(
        "    <p class=\"cap\">Priced with the exact gpt-4o tokenizer. The vertical line marks the \
         server's own median tool ({} tok).</p>\n",
        commas(median),
    ));
    s.push_str("    <div class=\"panel\">\n      <div class=\"chart\">\n");

    let med_pct = (median as f64 / max) * 100.0;
    for (name, tok) in ranked.iter().take(CHART_TOOLS) {
        let fill_pct = (*tok as f64 / max) * 100.0;
        s.push_str(&format!(
            "        <div class=\"crow\"><span class=\"name\">{name}</span>\
             <span class=\"ctrack\"><span class=\"cfill\" style=\"width:{fill:.1}%\"></span>\
             <span class=\"medline\" style=\"left:{med:.1}%\"></span></span>\
             <span class=\"val\">{val}</span></div>\n",
            name = html_escape(name),
            fill = fill_pct,
            med = med_pct,
            val = commas(*tok),
        ));
    }

    s.push_str("      </div>\n");
    // Legend: median + how far the top tool sits above it.
    let top = ranked.first();
    let ratio = if median > 0 {
        top.map(|(_, t)| format!("{:.1}×", *t as f64 / median as f64))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let legend = match top {
        Some((name, _)) if median > 0 => format!(
            "median tool = {med} tok · top tool <span class=\"mono\">{name}</span> is {ratio} \
             median · full table via <span class=\"mono\">jig budget --json</span>",
            med = commas(median),
            name = html_escape(name),
        ),
        _ => "full per-tool table via <span class=\"mono\">jig budget --json</span>".to_string(),
    };
    s.push_str(&format!("      <div class=\"legend\">{legend}</div>\n"));
    s.push_str("    </div>\n  </section>\n");
}

/// The median of the per-tool token counts (lower-middle for an even count, to
/// match the advisor's own definition).
fn median_tokens(per_tool: &[(String, usize)]) -> usize {
    if per_tool.is_empty() {
        return 0;
    }
    let mut v: Vec<usize> = per_tool.iter().map(|(_, t)| *t).collect();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2
    }
}

fn render_advisor(s: &mut String, report: &CheckReport) {
    if report.advisor.is_empty() {
        return;
    }
    s.push_str("  <section>\n");
    s.push_str(&format!(
        "    <h2>Advisor — tool-set findings ({})</h2>\n",
        report.advisor.len()
    ));
    s.push_str(
        "    <p class=\"cap\">Deterministic detectors for the failure modes that make models pick \
         the wrong tool. Not scored into the grade.</p>\n",
    );
    s.push_str("    <div class=\"panel flist\">\n");
    for f in &report.advisor {
        render_finding_row(s, f);
    }
    s.push_str("    </div>\n  </section>\n");
}

fn render_finding_row(s: &mut String, f: &Finding) {
    s.push_str(&format!(
        "      <div class=\"f\"><span class=\"sev {cls}\">{sev}</span><span>{msg}\
         <span class=\"fix\">{fix}</span></span></div>\n",
        cls = sev_class(f.severity),
        sev = f.severity.tag(),
        msg = render_inline(&f.message),
        fix = render_inline(&f.fix),
    ));
}

fn render_top_fixes(s: &mut String, report: &CheckReport) {
    let fixes = report.top_fixes(TOP_FIXES);
    if fixes.is_empty() {
        return;
    }
    s.push_str("  <section>\n");
    s.push_str("    <h2>Top fixes, in order of impact</h2>\n");
    s.push_str("    <div class=\"panel\">\n      <ol class=\"fixes\">\n");
    for f in fixes {
        s.push_str(&format!(
            "        <li>{msg}<span class=\"fix\">{dim} · {fix}</span></li>\n",
            msg = render_inline(&f.message),
            dim = html_escape(f.dimension.key()),
            fix = render_inline(&f.fix),
        ));
    }
    s.push_str("      </ol>\n    </div>\n  </section>\n");
}

fn render_footer(s: &mut String, report: &CheckReport, meta: &ReportMeta) {
    let mut notes: Vec<String> = Vec::new();
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
            let when = collected
                .as_deref()
                .map(|c| format!(", collected {}", html_escape(c)))
                .unwrap_or_default();
            let which = if *bundled {
                "jig's bundled census"
            } else {
                "an ecosystem dataset"
            };
            notes.push(format!(
                "Context cost was scored against {which} (n={n}{when}); a percentile is only as \
                 trustworthy as its sample."
            ));
        }
        ContextProvenance::AbsoluteBands => notes.push(
            "Context cost was scored with documented absolute bands (no ecosystem census loaded)."
                .to_string(),
        ),
    }
    notes.push(format!(
        "Protocol compliance and robustness reflect only what was observed in one session. Rubric \
         weights are editorial ({}).",
        html_escape(report.rubric_version)
    ));

    s.push_str("  <footer>\n    <b>Honesty notes.</b> ");
    s.push_str(&notes.join(" "));
    s.push_str("<br>\n    Generated by <span class=\"mono\">jig check</span> · ");
    s.push_str(&format!(
        "jig {} · <a href=\"https://github.com/Shodh-Labs/jig\" style=\"color:var(--brand)\">\
         github.com/Shodh-Labs/jig</a> · Shodh Labs\n  </footer>\n",
        html_escape(&meta.jig_version),
    ));
}

// ---------------------------------------------------------------------------
// Inline stylesheet (theme-aware, self-contained) — adapted from the M7 design
// spec; light + dark via `prefers-color-scheme` tokens plus explicit
// `data-theme` overrides.
// ---------------------------------------------------------------------------

const STYLE: &str = r#"<style>
  :root {
    --surface:#FAFAF8; --panel:#FFFFFF; --sunk:#F1EFEA; --ink:#1D1B18; --muted:#6E6A63;
    --faint:#9A958C; --hairline:#E5E2DC; --grid:#EFEDE8; --brand:#8A6B2D;
    --ok:#0D9488; --warn:#B45309; --bad:#C24437; --num:#3D5E8C;
    --shadow:0 1px 3px rgb(0 0 0 / 0.06);
  }
  @media (prefers-color-scheme: dark) {
    :root { --surface:#16171A; --panel:#1E2024; --sunk:#191B1E; --ink:#E8E6E1; --muted:#98948C;
      --faint:#6B6862; --hairline:#2C2E33; --grid:#26282D; --brand:#C79A4B;
      --ok:#0FA793; --warn:#D97706; --bad:#E06A5A; --num:#89A7CF; --shadow:none; }
  }
  :root[data-theme="light"] { --surface:#FAFAF8; --panel:#FFFFFF; --sunk:#F1EFEA; --ink:#1D1B18; --muted:#6E6A63;
    --faint:#9A958C; --hairline:#E5E2DC; --grid:#EFEDE8; --brand:#8A6B2D;
    --ok:#0D9488; --warn:#B45309; --bad:#C24437; --num:#3D5E8C; --shadow:0 1px 3px rgb(0 0 0 / 0.06); }
  :root[data-theme="dark"] { --surface:#16171A; --panel:#1E2024; --sunk:#191B1E; --ink:#E8E6E1; --muted:#98948C;
    --faint:#6B6862; --hairline:#2C2E33; --grid:#26282D; --brand:#C79A4B;
    --ok:#0FA793; --warn:#D97706; --bad:#E06A5A; --num:#89A7CF; --shadow:none; }

  * { box-sizing: border-box; }
  body { background: var(--surface); color: var(--ink); margin: 0;
    font-family: system-ui, "Segoe UI", sans-serif; line-height: 1.55; padding: 2.2rem 1.25rem 4rem; }
  .mono { font-family: ui-monospace, "Cascadia Code", Consolas, monospace; }
  main { max-width: 54rem; margin: 0 auto; display: flex; flex-direction: column; gap: 1.6rem; }

  header .eyebrow { font-family: ui-monospace, Consolas, monospace; font-size: .7rem; letter-spacing: .14em;
    color: var(--brand); text-transform: uppercase; margin-bottom: .4rem; }
  header h1 { font-size: 1.5rem; margin: 0 0 .2rem; text-wrap: balance; }
  header .meta { color: var(--muted); font-size: .85rem; }
  header .meta .mono { color: var(--faint); font-size: .78rem; }

  .hero { display: grid; grid-template-columns: auto 1fr; gap: 1.4rem; align-items: center;
    background: var(--panel); border: 1px solid var(--hairline); border-radius: 12px; padding: 1.4rem 1.6rem; box-shadow: var(--shadow); }
  .score { text-align: center; padding-right: 1.4rem; border-right: 1px solid var(--hairline); }
  .score .n { font-family: ui-monospace, Consolas, monospace; font-size: 3.4rem; font-weight: 750; line-height: 1; }
  .score .g { display: inline-block; margin-top: .45rem; font-family: ui-monospace, Consolas, monospace; font-weight: 700;
    color: var(--surface); border-radius: 6px; padding: .1rem .6rem; font-size: 1rem; }
  .score .sub { color: var(--faint); font-size: .72rem; margin-top: .4rem; }
  .dims { display: flex; flex-direction: column; gap: .45rem; min-width: 0; }
  .dim { display: grid; grid-template-columns: 11.5rem 1fr 2.6rem; gap: .7rem; align-items: center; font-size: .85rem; }
  .dim .lbl { text-align: right; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .dim .lbl small { color: var(--faint); }
  .bar { height: 10px; border-radius: 5px; background: var(--grid); overflow: hidden; }
  .bar i { display: block; height: 100%; border-radius: 5px; }
  .dim .v { font-family: ui-monospace, Consolas, monospace; font-variant-numeric: tabular-nums; text-align: right; }

  .callouts { display: grid; grid-template-columns: 1fr 1fr; gap: .9rem; }
  @media (max-width: 700px) { .callouts { grid-template-columns: 1fr; } .hero { grid-template-columns: 1fr; } .score { border-right: 0; padding-right: 0; border-bottom: 1px solid var(--hairline); padding-bottom: 1rem; } }
  .co { background: var(--panel); border: 1px solid var(--hairline); border-left: 4px solid var(--warn);
    border-radius: 10px; padding: 1rem 1.1rem; box-shadow: var(--shadow); }
  .co h3 { margin: 0 0 .3rem; font-size: .95rem; }
  .co .big { font-family: ui-monospace, Consolas, monospace; font-size: 1.6rem; font-weight: 700; }
  .co p { margin: .3rem 0 0; font-size: .84rem; color: var(--muted); }

  section > h2 { font-size: 1.05rem; margin: .4rem 0 .1rem; }
  section > .cap { color: var(--muted); font-size: .84rem; margin: 0 0 .8rem; }
  .panel { background: var(--panel); border: 1px solid var(--hairline); border-radius: 10px; padding: 1rem 1.15rem; box-shadow: var(--shadow); }

  .chart { display: flex; flex-direction: column; gap: 2px; }
  .crow { display: grid; grid-template-columns: 11.5rem 1fr 3.4rem; gap: .6rem; align-items: center; font-size: .82rem; padding: .1rem .3rem; border-radius: 5px; }
  .crow:hover { background: var(--grid); }
  .crow .name { font-family: ui-monospace, Consolas, monospace; text-align: right; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .ctrack { position: relative; height: 14px; }
  .cfill { position: absolute; left: 0; top: 1px; height: 12px; background: var(--ok); border-radius: 0 4px 4px 0; min-width: 3px; }
  .crow .val { font-family: ui-monospace, Consolas, monospace; font-variant-numeric: tabular-nums; color: var(--muted); text-align: right; font-size: .78rem; }
  .medline { position: absolute; top: -2px; bottom: -2px; width: 2px; background: var(--faint); opacity: .7; }
  .legend { font-size: .74rem; color: var(--faint); margin-top: .55rem; }

  .flist { display: flex; flex-direction: column; gap: .55rem; }
  .f { display: grid; grid-template-columns: auto 1fr; gap: .6rem; align-items: baseline; font-size: .86rem; }
  .sev { font-family: ui-monospace, Consolas, monospace; font-size: .68rem; border-radius: 999px; padding: .1rem .55rem; white-space: nowrap; border: 1px solid; }
  .sev.h { color: var(--bad); border-color: var(--bad); }
  .sev.m { color: var(--warn); border-color: var(--warn); }
  .sev.l { color: var(--muted); border-color: var(--hairline); }
  .f .fix { display: block; color: var(--muted); font-size: .8rem; margin-top: .1rem; }
  .f .fix::before { content: "\2192 "; color: var(--faint); }

  ol.fixes { margin: 0; padding-left: 1.2rem; display: flex; flex-direction: column; gap: .6rem; font-size: .88rem; }
  ol.fixes .mono { font-size: .82rem; }
  ol.fixes .fix { display: block; color: var(--muted); font-size: .8rem; }
  ol.fixes .fix::before { content: "\2192 "; color: var(--faint); }

  footer { color: var(--faint); font-size: .76rem; border-top: 1px solid var(--hairline); padding-top: 1rem; line-height: 1.7; }
</style>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::check::Observations;
    use jig_core::{evaluate, CheckInput, UnknownMethodProbe};
    use serde_json::{json, Value};

    fn tool(name: &str, desc: Option<&str>, schema: Value) -> jig_core::Tool {
        let mut m = serde_json::Map::new();
        m.insert("name".to_string(), json!(name));
        if let Some(d) = desc {
            m.insert("description".to_string(), json!(d));
        }
        m.insert("inputSchema".to_string(), schema);
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    fn meta() -> ReportMeta {
        ReportMeta {
            transport: "stdio".to_string(),
            command_line: "jig check --stdio \"./jig-mock-server\"".to_string(),
            date: "2026-07-20".to_string(),
            jig_version: "0.4.0".to_string(),
        }
    }

    /// A deterministic multi-tool fixture that exercises every report section:
    /// the chart (several tools), the advisor (a synonym collision), schema and
    /// description findings, and the top-fixes ranker.
    fn sample_input() -> CheckInput {
        CheckInput {
            server_name: "jig-mock-server".to_string(),
            server_version: "0.1.0".to_string(),
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({ "tools": {} }),
            instructions: Some("A toy MCP server for exercising Jig.".to_string()),
            tools: vec![
                tool(
                    "get_status",
                    Some("Return the current status of the primary job."),
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "fetch_status",
                    Some("Retrieve the present status of the primary job."),
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "make_reservation",
                    Some("Book a table. Demonstrates a nested object argument and an enum."),
                    json!({ "type": "object", "properties": {
                        "party": { "type": "object" },
                        "date": { "type": "string", "description": "ISO-8601 date." }
                    }, "required": ["party", "date"] }),
                ),
            ],
            observations: Observations {
                pollution_lines: 0,
                list_latency: Some(std::time::Duration::from_millis(12)),
                clean_shutdown: true,
                unknown_method: UnknownMethodProbe::Errored(-32601),
                ..Default::default()
            },
        }
    }

    #[test]
    fn sanitize_is_slug_and_never_traverses() {
        assert_eq!(sanitize_server_name("jig-mock-server"), "jig-mock-server");
        assert_eq!(sanitize_server_name("My Server 2000!"), "my-server-2000");
        assert_eq!(sanitize_server_name("@scope/pkg"), "scope-pkg");
        // Path-traversal and separators collapse to a safe slug.
        assert_eq!(sanitize_server_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_server_name("a\\b/c"), "a-b-c");
        // Nothing usable → the fallback, never an empty name.
        assert_eq!(sanitize_server_name("///"), "server");
        assert_eq!(sanitize_server_name(""), "server");
    }

    #[test]
    fn default_filename_uses_sanitized_name() {
        assert_eq!(
            ReportMeta::default_filename("My Server!"),
            "jig-report-my-server.html"
        );
    }

    #[test]
    fn hostile_tool_name_is_escaped_not_executed() {
        let mut input = sample_input();
        input.tools.push(tool(
            "<script>alert('xss')</script>",
            Some("<img src=x onerror=alert(1)>"),
            json!({ "type": "object", "properties": {} }),
        ));
        // Also try to inject via the server name itself.
        input.server_name = "evil<script>".to_string();
        let report = evaluate(&input, None);
        let html = render(&report, &meta());
        assert!(
            !html.contains("<script>alert"),
            "raw <script> must never reach the document"
        );
        assert!(
            html.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"),
            "the hostile tool name must arrive HTML-escaped"
        );
        assert!(
            html.contains("evil&lt;script&gt;"),
            "the hostile server name must arrive HTML-escaped"
        );
        assert!(
            !html.contains("onerror=alert(1)>"),
            "the hostile description must not reach the document unescaped"
        );
    }

    #[test]
    fn render_inline_escapes_before_wrapping_code() {
        let out = render_inline("collision on `<b>` here");
        assert!(out.contains("<span class=\"mono\">&lt;b&gt;</span>"));
        assert!(!out.contains("<b>"));
    }

    #[test]
    fn today_is_iso_shaped() {
        let d = today_utc();
        assert_eq!(d.len(), 10, "YYYY-MM-DD, got {d}");
        assert_eq!(d.as_bytes()[4], b'-');
        assert_eq!(d.as_bytes()[7], b'-');
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_262), (2020, 1, 1));
    }

    #[test]
    fn golden_html_report() {
        let report = evaluate(&sample_input(), None);
        insta::assert_snapshot!("report_html", render(&report, &meta()));
    }

    #[test]
    fn golden_html_report_bundled_percentile() {
        // With the bundled census engaged, the callout + footer switch to the
        // percentile provenance language.
        let p = jig_core::bundled_percentiles().unwrap();
        let report = evaluate(&sample_input(), Some(&p));
        let html = render(&report, &meta());
        assert!(html.contains("bundled census"));
        assert!(html.contains("public servers"));
    }
}
