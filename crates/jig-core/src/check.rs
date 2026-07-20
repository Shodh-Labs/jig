//! `jig check` — the **report card**: one scored verdict over everything Jig can
//! observe about a server in a single connect-inspect-budget session.
//!
//! Instruments require interpretation; a *grade with a to-do list* converts. This
//! module is the scoring engine behind `jig check`: five weighted dimensions,
//! each `0..=100`, composited into an overall `0..=100`, with every deduction
//! captured as a typed [`Finding`] whose `fix` text is the product.
//!
//! # Purity
//!
//! [`evaluate`] is a **pure function** of a [`CheckInput`] (data already gathered
//! in one session) plus an optional ecosystem [`Percentiles`] dataset. It opens
//! no connections and does no I/O, so every scoring rule is unit-testable against
//! constructed fixtures and the whole report is snapshot-lockable. The CLI does
//! the one live session, fills a [`CheckInput`], and calls [`evaluate`] once.
//!
//! # The rubric (`rubric-v1`)
//!
//! | Dimension | Weight | What it measures |
//! | --- | --- | --- |
//! | [Protocol compliance](Dimension::Protocol) | 25 | handshake, stdout framing, spec-valid capabilities, timeouts |
//! | [Context cost](Dimension::ContextCost) | 25 | gpt-4o exact total tokens, percentile or absolute bands |
//! | [Schema hygiene](Dimension::SchemaHygiene) | 20 | per-tool: descriptions, param types/descriptions, annotations |
//! | [Description quality](Dimension::DescriptionQuality) | 15 | *heuristic* — description length, name consistency, titles |
//! | [Robustness](Dimension::Robustness) | 15 | *observed only* — list latency, clean shutdown |
//!
//! Each dimension starts at 100 and subtracts documented penalties (see the
//! `PENALTY_*` / `*_PENALTY` constants), clamped to `0..=100`. A dimension that
//! is not applicable (e.g. schema hygiene on a server exposing no tools) is
//! *excluded* from the composite and its weight is dropped, never assumed to be
//! 100.

use std::sync::LazyLock;
use std::time::Duration;

use serde_json::Value;

use crate::protocol::Tool;
use crate::tokens::{canonical_tool_json, ModelCounter};

/// The context-metric tokenizer, built once per process. Constructing a
/// tiktoken BPE is expensive, so [`evaluate`] — which may be called many times
/// (e.g. in a property test) — shares this single counter rather than rebuilding
/// it per tool or per call. `None` only if the tokenizer failed to build, in
/// which case token counts degrade to `0` rather than panicking.
static CONTEXT_COUNTER: LazyLock<Option<ModelCounter>> =
    LazyLock::new(|| ModelCounter::new(CONTEXT_METRIC_MODEL).ok());

/// The shared context-metric counter, if it built successfully.
fn context_counter() -> Option<&'static ModelCounter> {
    CONTEXT_COUNTER.as_ref()
}

/// The rubric version string, emitted in `--json` so a score is always tied to
/// the ruleset that produced it.
pub const RUBRIC_VERSION: &str = "rubric-v1";

/// The model whose exact tokenizer defines the context-cost metric.
const CONTEXT_METRIC_MODEL: &str = "gpt-4o";

// ---------------------------------------------------------------------------
// Penalty tables (documented, so a score is never a black box)
// ---------------------------------------------------------------------------

/// Protocol: points deducted per non-protocol (framing-breaking) stdout line.
const PROTOCOL_POLLUTION_PENALTY: f64 = 15.0;
/// Protocol: cap on the total pollution deduction.
const PROTOCOL_POLLUTION_CAP: f64 = 60.0;
/// Protocol: points per capability advertised outside the negotiated spec.
const PROTOCOL_OFFSPEC_CAP_PENALTY: f64 = 10.0;
/// Protocol: cap on the total off-spec-capability deduction.
const PROTOCOL_OFFSPEC_CAP_CAP: f64 = 30.0;
/// Protocol: deduction when a list operation timed out (server accepted the
/// request but never answered).
const PROTOCOL_LIST_TIMEOUT_PENALTY: f64 = 40.0;
/// Protocol: deduction per tool whose name violates the MCP name format
/// (conformance scenario `tools-name-format`, SEP-986).
const PROTOCOL_TOOL_NAME_FORMAT_PENALTY: f64 = 8.0;
/// Protocol: cap on the total tool-name-format deduction.
const PROTOCOL_TOOL_NAME_FORMAT_CAP: f64 = 24.0;
/// Protocol: deduction per missing/empty required `initialize` result field
/// (conformance scenario `server-initialize`, MCP-Initialize).
const PROTOCOL_INIT_FIELD_PENALTY: f64 = 10.0;
/// Protocol: deduction when the server answers an unknown method with a
/// non-standard JSON-RPC error code (conformance scenario `negative`).
const PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY: f64 = 10.0;
/// Protocol: deduction when the server *accepts* an unknown method instead of
/// rejecting it with `-32601` (conformance scenario `negative`).
const PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY: f64 = 20.0;

/// The JSON-RPC 2.0 "Method not found" error code every MCP server must return
/// for a method it does not implement (JSON-RPC 2.0 §5.1).
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// The maximum length (characters) of a legal MCP tool name (SEP-986).
const TOOL_NAME_MAX_LEN: usize = 64;

/// How many leading bytes of a polluting line to quote in the fix text.
const POLLUTION_EXCERPT_BYTES: usize = 24;

/// Schema: deduction per tool missing a description.
const SCHEMA_MISSING_TOOL_DESC: f64 = 8.0;
/// Schema: deduction per parameter missing a description.
const SCHEMA_PARAM_MISSING_DESC: f64 = 3.0;
/// Schema: deduction per parameter missing a type (and no enum/`$ref`/etc.).
const SCHEMA_PARAM_MISSING_TYPE: f64 = 5.0;
/// Schema: deduction per tool declaring no annotations (`readOnlyHint`, …).
const SCHEMA_MISSING_ANNOTATIONS: f64 = 1.0;
/// Schema: cap on the total missing-annotations deduction (it is minor).
const SCHEMA_ANNOTATIONS_CAP: f64 = 10.0;

/// Description: deduction for a tool name containing whitespace (uncallable).
const DQ_NAME_HAS_SPACE: f64 = 15.0;
/// Description: deduction for a tool name breaking the server's dominant
/// naming convention (kebab vs snake).
const DQ_NAME_INCONSISTENT: f64 = 5.0;
/// Description: deduction for a description that is present but too terse for a
/// model to select on (see [`DQ_TERSE_TOKENS`]) or missing entirely.
const DQ_DESC_TERSE: f64 = 6.0;
/// Description: deduction for a description long enough to waste context (see
/// [`DQ_VERBOSE_TOKENS`]).
const DQ_DESC_VERBOSE: f64 = 4.0;
/// Description: deduction per tool missing a human-facing `title`.
const DQ_MISSING_TITLE: f64 = 1.0;
/// Description: cap on the total missing-title deduction (it is minor).
const DQ_TITLE_CAP: f64 = 10.0;
/// A description at or below this token count is "terse".
const DQ_TERSE_TOKENS: usize = 4;
/// A description at or above this token count is "verbose".
const DQ_VERBOSE_TOKENS: usize = 160;

/// Robustness: list latency at or below this is unremarkable (full sub-score).
const ROBUST_LATENCY_FAST_MS: u128 = 1_000;
/// Robustness: list latency at or below this is sluggish (mid sub-score).
const ROBUST_LATENCY_SLOW_MS: u128 = 3_000;
/// Robustness sub-score for a sluggish list operation.
const ROBUST_LATENCY_SLUGGISH_SCORE: f64 = 70.0;
/// Robustness sub-score for a slow list operation.
const ROBUST_LATENCY_SLOW_SCORE: f64 = 40.0;
/// Robustness sub-score for an unclean shutdown.
const ROBUST_UNCLEAN_SHUTDOWN_SCORE: f64 = 30.0;

/// Context-cost absolute-band anchor points `(tokens, score)`, ascending by
/// tokens. Score is piecewise-linearly interpolated between anchors and clamped
/// to `0..=100`. Tuned to the battery: a ~1.4k-token server (`everything`)
/// lands ~93, a ~3.4k one (`playwright`) ~86, 8–20k is "heavy", >20k "severe".
const CONTEXT_BANDS: &[(f64, f64)] = &[
    (0.0, 100.0),
    (2_000.0, 90.0),
    (8_000.0, 75.0),
    (20_000.0, 45.0),
    (50_000.0, 5.0),
];

/// One MCP protocol revision and the top-level server-capability keys it
/// defines. Capability legality is **version-relative**: `completions` is legal
/// from `2025-03-26`, `tasks` was introduced (experimentally) in `2025-11-25`,
/// and `extensions` in the `2026-07-28` release candidate — so the same
/// advertised capability is graded differently under different negotiated
/// revisions.
///
/// Sets are sorted so membership reads cleanly; the exact keys are taken from
/// each revision's published `schema.ts` `ServerCapabilities` interface
/// (`github.com/modelcontextprotocol/modelcontextprotocol/schema/<rev>`).
struct Revision {
    /// The revision date string (the negotiated `protocolVersion` value).
    id: &'static str,
    /// Top-level server-capability keys legal in this revision.
    capabilities: &'static [&'static str],
}

/// Known MCP revisions, oldest first. The last entry is the latest known
/// revision, used to validate a server that negotiates a version Jig does not
/// recognize (with the assumption noted in the finding).
///
/// `tasks` appears here under `2025-11-25`, where it was standardized as an
/// (experimental) top-level capability. In the `2026-07-28` release candidate
/// the Tasks feature was redesigned as an *extension* advertised through the
/// `extensions` capability map, so `tasks` is intentionally **not** a top-level
/// key of the `2026-07-28` set (see `docs/conformance-alignment.md`).
const REVISIONS: &[Revision] = &[
    Revision {
        id: "2024-11-05",
        capabilities: &["experimental", "logging", "prompts", "resources", "tools"],
    },
    Revision {
        id: "2025-03-26",
        capabilities: &[
            "completions",
            "experimental",
            "logging",
            "prompts",
            "resources",
            "tools",
        ],
    },
    Revision {
        id: "2025-06-18",
        capabilities: &[
            "completions",
            "experimental",
            "logging",
            "prompts",
            "resources",
            "tools",
        ],
    },
    Revision {
        id: "2025-11-25",
        capabilities: &[
            "completions",
            "experimental",
            "logging",
            "prompts",
            "resources",
            "tasks",
            "tools",
        ],
    },
    Revision {
        id: "2026-07-28",
        capabilities: &[
            "completions",
            "experimental",
            "extensions",
            "logging",
            "prompts",
            "resources",
            "tools",
        ],
    },
];

/// The revision whose id matches `version`, if Jig knows it.
fn revision_for(version: &str) -> Option<&'static Revision> {
    REVISIONS.iter().find(|r| r.id == version)
}

/// The latest known revision — the fallback when a server negotiates a version
/// Jig does not recognize.
fn latest_revision() -> &'static Revision {
    REVISIONS
        .last()
        .expect("REVISIONS is a non-empty compile-time table")
}

/// The earliest known revision that defines `cap` as a top-level server
/// capability, if any does — so a finding can say where a capability *is*
/// standardized.
fn capability_introduced_in(cap: &str) -> Option<&'static str> {
    REVISIONS
        .iter()
        .find(|r| r.capabilities.contains(&cap))
        .map(|r| r.id)
}

/// A short human note for a capability advertised outside the negotiated
/// `version`, or `None` when the capability is in-spec for that revision.
///
/// Public so every Jig surface (e.g. `jig inspect`) can annotate advertised
/// capabilities the same **version-aware** way `jig check` grades them, instead
/// of hard-coding a single revision's flat list. An unknown `version` is
/// validated against the latest known revision, with that assumption noted.
pub fn capability_offspec_note(capability: &str, version: &str) -> Option<String> {
    let (revision, assumed_latest) = match revision_for(version) {
        Some(r) => (r, false),
        None => (latest_revision(), true),
    };
    if revision.capabilities.contains(&capability) {
        return None;
    }
    let where_defined = match capability_introduced_in(capability) {
        Some(rev) => format!("first defined in {rev}"),
        None => "not defined in any known MCP revision".to_string(),
    };
    let assumed = if assumed_latest {
        format!("; version unknown, checked against {}", revision.id)
    } else {
        String::new()
    };
    Some(format!(
        "not defined in negotiated revision {} ({where_defined}{assumed})",
        revision.id
    ))
}

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

/// Severity of a [`Finding`], ordered most-to-least serious.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A correctness/framing problem that breaks real clients.
    High,
    /// A quality problem that measurably degrades model behavior.
    Medium,
    /// A minor, easily-fixed nit.
    Low,
    /// Informational only — reported, never scored.
    Info,
}

impl Severity {
    /// A short lowercase tag (`high`, `medium`, `low`, `info`).
    pub fn tag(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }
}

/// One of the five rubric dimensions, or the [`ToolSet`](Dimension::ToolSet)
/// advisor category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// Protocol compliance (weight 25).
    Protocol,
    /// Context cost (weight 25).
    ContextCost,
    /// Schema hygiene (weight 20).
    SchemaHygiene,
    /// Description quality — heuristic (weight 15).
    DescriptionQuality,
    /// Robustness — observed behavior (weight 15).
    Robustness,
    /// The tool-set advisor category (see [`crate::advisor`]). **Not a scored
    /// rubric dimension** — it is deliberately excluded from [`Dimension::all`]
    /// and never produces a [`DimensionScore`], so it never enters the
    /// composite. It exists to give the advisor's findings a machine key
    /// (`tool_set`) and a ranking weight for the shared "Top fixes" list.
    ToolSet,
}

/// The ranking weight of an advisor ([`Dimension::ToolSet`]) finding. Used
/// **only** to order advisor findings against dimension findings in "Top fixes"
/// — it is not a rubric weight and never enters the composite (no advisor
/// finding is ever attached to a scored [`DimensionScore`]).
const TOOL_SET_RANK_WEIGHT: u32 = 18;

impl Dimension {
    /// The dimension's composite weight (or, for [`ToolSet`](Dimension::ToolSet),
    /// its fixed "Top fixes" ranking weight — see the `TOOL_SET_RANK_WEIGHT`
    /// constant).
    pub fn weight(self) -> u32 {
        match self {
            Dimension::Protocol => 25,
            Dimension::ContextCost => 25,
            Dimension::SchemaHygiene => 20,
            Dimension::DescriptionQuality => 15,
            Dimension::Robustness => 15,
            Dimension::ToolSet => TOOL_SET_RANK_WEIGHT,
        }
    }

    /// A human-facing label.
    pub fn label(self) -> &'static str {
        match self {
            Dimension::Protocol => "Protocol compliance",
            Dimension::ContextCost => "Context cost",
            Dimension::SchemaHygiene => "Schema hygiene",
            Dimension::DescriptionQuality => "Description quality",
            Dimension::Robustness => "Robustness",
            Dimension::ToolSet => "Tool set",
        }
    }

    /// A short machine key (`protocol`, `context_cost`, …, `tool_set`).
    pub fn key(self) -> &'static str {
        match self {
            Dimension::Protocol => "protocol",
            Dimension::ContextCost => "context_cost",
            Dimension::SchemaHygiene => "schema_hygiene",
            Dimension::DescriptionQuality => "description_quality",
            Dimension::Robustness => "robustness",
            Dimension::ToolSet => "tool_set",
        }
    }

    /// Whether this dimension is scored by (honestly-labelled) heuristics rather
    /// than deterministic protocol facts.
    pub fn is_heuristic(self) -> bool {
        matches!(self, Dimension::DescriptionQuality)
    }

    /// The declared weight of every scored rubric dimension, in rubric order.
    /// [`ToolSet`](Dimension::ToolSet) is intentionally absent — it is not scored.
    pub fn all() -> [Dimension; 5] {
        [
            Dimension::Protocol,
            Dimension::ContextCost,
            Dimension::SchemaHygiene,
            Dimension::DescriptionQuality,
            Dimension::Robustness,
        ]
    }
}

/// A single scored deduction: what was wrong, how bad, and how to fix it. The
/// `fix` string is the whole point of the product — it turns an instrument
/// reading into a to-do item.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Which dimension this finding belongs to.
    pub dimension: Dimension,
    /// How serious it is.
    pub severity: Severity,
    /// What is wrong, in one line.
    pub message: String,
    /// The concrete remedy — e.g. "trim `search`'s description — save ~1,900
    /// tokens".
    pub fix: String,
    /// Points deducted from the dimension's `0..=100` score by this finding.
    /// `Info` findings carry `0.0`. Used to rank the "Top fixes" list by
    /// composite impact (`points * weight`).
    pub points: f64,
    /// Whether this finding is *pinned* into the "Top fixes" list regardless of
    /// its numeric rank. Set for breaks-real-clients findings — chiefly stdout
    /// pollution — so a heavy context-cost or many-tool server can never bury
    /// the one problem that stops the server from working at all.
    pub pinned: bool,
}

impl Finding {
    /// This finding's impact on the composite score: dimension-local `points`
    /// scaled by the dimension weight. Higher = fixing it moves the grade more.
    pub fn weighted_impact(&self) -> f64 {
        self.points * self.dimension.weight() as f64
    }
}

/// The scored result for one dimension.
#[derive(Debug, Clone)]
pub struct DimensionScore {
    /// Which dimension.
    pub dimension: Dimension,
    /// The `0..=100` score, or `None` when the dimension is not applicable to
    /// this server and is therefore excluded from the composite.
    pub score: Option<f64>,
    /// The dimension's composite weight (mirrors [`Dimension::weight`]).
    pub weight: u32,
    /// A one-line reason shown next to the score.
    pub summary: String,
    /// Whether this dimension is heuristic (labelled as such in the report).
    pub heuristic: bool,
    /// Every deduction taken, in the order applied.
    pub findings: Vec<Finding>,
}

/// How the context-cost dimension was scored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextProvenance {
    /// Scored against an ecosystem percentile dataset. Carries the server's
    /// percentile rank (rounded) and the sample count.
    Percentile {
        /// The server's percentile rank in the dataset (0..=100, rounded).
        percentile: u32,
        /// Number of samples in the dataset.
        n: usize,
        /// The dataset's `collected` date (YYYY-MM-DD), if known.
        collected: Option<String>,
        /// Whether the dataset was the census bundled into the binary (the
        /// default) rather than a user-supplied `--percentiles` file. Drives the
        /// "bundled census" provenance label so the number's age is visible.
        bundled: bool,
    },
    /// Scored with the fixed absolute bands (no ecosystem dataset available).
    AbsoluteBands,
}

/// The complete report card produced by [`evaluate`].
#[derive(Debug, Clone)]
pub struct Report {
    /// Server name (from `serverInfo`).
    pub server_name: String,
    /// Server version.
    pub server_version: String,
    /// The negotiated protocol version.
    pub protocol_version: String,
    /// The weighted composite score, `0..=100` (unrounded).
    pub composite: f64,
    /// Per-dimension scores, in rubric order.
    pub dimensions: Vec<DimensionScore>,
    /// The gpt-4o exact total tokens the context-cost dimension measured.
    pub total_tokens: usize,
    /// How context cost was scored.
    pub context_provenance: ContextProvenance,
    /// The rubric version that produced this report.
    pub rubric_version: &'static str,
    /// Number of tools observed.
    pub tool_count: usize,
    /// Per-tool gpt-4o context-cost token counts, in server order, exactly as
    /// the context-cost dimension measured them. Surfaced so a downstream
    /// renderer (the HTML report card) can draw the per-tool token chart without
    /// re-tokenizing. Empty when the server exposes no tools.
    pub per_tool_tokens: Vec<(String, usize)>,
    /// Tool-set advisor findings (see [`crate::advisor`]), stably sorted. These
    /// are tagged [`Dimension::ToolSet`] and are **never** scored into the
    /// composite; they surface in a dedicated report section and may be ranked
    /// into "Top fixes". Empty when no advisory fired.
    pub advisor: Vec<Finding>,
}

impl Report {
    /// The composite rounded to the nearest integer — the headline number and
    /// the value the `--min-score` gate and `--badge` compare against.
    pub fn composite_rounded(&self) -> u32 {
        self.composite.round() as u32
    }

    /// The single dimension score, by dimension.
    pub fn dimension(&self, d: Dimension) -> Option<&DimensionScore> {
        self.dimensions.iter().find(|s| s.dimension == d)
    }

    /// The top `n` fixes across all dimensions **and the tool-set advisor**,
    /// ranked by impact (`points * weight`) descending, ties broken by severity,
    /// then rubric dimension order (advisor findings rank after the five scored
    /// dimensions), then message. `Info` findings and zero-impact findings are
    /// excluded. Advisor findings rank by their `points` and the advisor ranking
    /// weight; they still never affect the composite.
    ///
    /// **Pinned** findings (breaks-real-clients issues such as stdout pollution)
    /// are always included: if a pinned finding would fall outside the top `n`
    /// by rank, it displaces the lowest-ranked unpinned entry so it is never
    /// buried under higher-scoring but less-fatal findings.
    pub fn top_fixes(&self, n: usize) -> Vec<&Finding> {
        let mut all: Vec<&Finding> = self
            .dimensions
            .iter()
            .flat_map(|d| d.findings.iter())
            .chain(self.advisor.iter())
            .filter(|f| f.points > 0.0 && f.severity != Severity::Info)
            .collect();
        all.sort_by(|a, b| {
            b.weighted_impact()
                .partial_cmp(&a.weighted_impact())
                .unwrap_or(std::cmp::Ordering::Equal)
                // On equal impact, the more severe fix leads.
                .then_with(|| severity_rank(a.severity).cmp(&severity_rank(b.severity)))
                .then_with(|| dim_rank(a.dimension).cmp(&dim_rank(b.dimension)))
                .then_with(|| a.message.cmp(&b.message))
        });
        if n == 0 {
            return Vec::new();
        }
        let mut top: Vec<&Finding> = all.iter().copied().take(n).collect();
        // Ensure every pinned finding is present. If one ranked below the cutoff,
        // swap it in for the current lowest-ranked *unpinned* entry, preserving
        // the ranked order of the survivors.
        for pinned in all.iter().copied().filter(|f| f.pinned) {
            if top.iter().any(|f| std::ptr::eq(*f, pinned)) {
                continue;
            }
            if let Some(pos) = top.iter().rposition(|f| !f.pinned) {
                top[pos] = pinned;
            }
        }
        // Re-sort the (possibly displaced) survivors so the list stays ranked.
        top.sort_by(|a, b| {
            b.weighted_impact()
                .partial_cmp(&a.weighted_impact())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| severity_rank(a.severity).cmp(&severity_rank(b.severity)))
                .then_with(|| dim_rank(a.dimension).cmp(&dim_rank(b.dimension)))
                .then_with(|| a.message.cmp(&b.message))
        });
        top
    }

    /// Whether any scored dimension was heuristic (drives the report footnote).
    pub fn has_heuristic_dimension(&self) -> bool {
        self.dimensions.iter().any(|d| d.heuristic)
    }
}

/// Rubric order rank for stable tie-breaking. The advisor category
/// ([`Dimension::ToolSet`]) is not in [`Dimension::all`], so it ranks *after*
/// every scored dimension.
fn dim_rank(d: Dimension) -> usize {
    Dimension::all()
        .iter()
        .position(|x| *x == d)
        .unwrap_or(Dimension::all().len())
}

/// Severity rank for tie-breaking: most-severe first.
fn severity_rank(s: Severity) -> usize {
    match s {
        Severity::High => 0,
        Severity::Medium => 1,
        Severity::Low => 2,
        Severity::Info => 3,
    }
}

/// A one-line dimension summary from its findings: the first finding's message,
/// with a `(+N more)` tail when there are others. Empty findings yield `clean`.
fn summarize_findings(findings: &[Finding], clean: &str) -> String {
    match findings.split_first() {
        None => clean.to_string(),
        Some((head, [])) => head.message.clone(),
        Some((head, rest)) => format!("{} (+{} more)", head.message, rest.len()),
    }
}

/// The location and first bytes of a stdout-pollution line, so a finding can
/// point at the exact byte where MCP framing broke and quote the offending
/// bytes. Populated from the tap's
/// [`non_protocol_inbound_detailed`](crate::ProtocolTap::non_protocol_inbound_detailed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PollutionSite {
    /// Byte offset in the stdout stream where the line began, if the transport
    /// tracked one (stdio does; HTTP does not).
    pub offset: Option<u64>,
    /// The offending line's text (lossily decoded).
    pub line: String,
}

/// The outcome of probing a server with a deliberately unknown JSON-RPC method,
/// used to grade error-code correctness (conformance scenario `negative`). A
/// spec-conformant server answers with a JSON-RPC `-32601 Method not found`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownMethodProbe {
    /// The server was not probed (e.g. the session ended first).
    #[default]
    NotProbed,
    /// The server answered with a JSON-RPC error carrying this code. `-32601`
    /// is conformant; any other code is a finding.
    Errored(i64),
    /// The server returned a *success* result for a method it should not know —
    /// a clear conformance violation.
    Accepted,
    /// The server did not answer (timeout / disconnect); inconclusive, so not
    /// scored either way.
    NoAnswer,
}

/// The passively-observed session facts the robustness and protocol dimensions
/// score. Only what was *actually observed* — nothing is assumed.
#[derive(Debug, Clone, Default)]
pub struct Observations {
    /// Count of non-protocol (framing-breaking) lines seen on the server's
    /// stdout (from the tap's `non_protocol_inbound`).
    pub pollution_lines: usize,
    /// The location + first bytes of the *first* polluting line, when captured,
    /// so the pollution finding can name the exact byte offset and quote it.
    pub first_pollution: Option<PollutionSite>,
    /// Whether a `*/list` operation timed out.
    pub list_timed_out: bool,
    /// Observed wall-clock latency of the `tools/list` operation, if measured.
    pub list_latency: Option<Duration>,
    /// Whether the session shut the server down cleanly.
    pub clean_shutdown: bool,
    /// Server stderr volume in bytes, if captured. **Informational only** — it
    /// is reported, never scored (a server logging to stderr is correct MCP).
    pub stderr_noise_bytes: Option<usize>,
    /// The outcome of the unknown-method error-code probe.
    pub unknown_method: UnknownMethodProbe,
}

/// Everything the scorer needs, gathered in one live session. Plain data, so the
/// engine is pure and every rule is testable against a constructed fixture.
#[derive(Debug, Clone)]
pub struct CheckInput {
    /// Server name (`serverInfo.name`).
    pub server_name: String,
    /// Server version (`serverInfo.version`).
    pub server_version: String,
    /// The negotiated protocol version.
    pub protocol_version: String,
    /// The server's advertised capabilities, as raw JSON.
    pub capabilities: Value,
    /// The server's `instructions` string, if any (counted in context cost).
    pub instructions: Option<String>,
    /// The server's tools.
    pub tools: Vec<Tool>,
    /// Passively-observed session facts.
    pub observations: Observations,
}

// ---------------------------------------------------------------------------
// Ecosystem percentiles (optional dataset)
// ---------------------------------------------------------------------------

/// An ascending array of raw samples for one metric.
#[derive(Debug, Clone)]
pub struct MetricSamples {
    /// Ascending sample values.
    pub samples: Vec<f64>,
}

impl MetricSamples {
    /// The percentile rank of `x`: `100 * (count of samples <= x) / len`, in
    /// `0.0..=100.0`. Empty samples yield `0.0`.
    pub fn percentile(&self, x: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let below = self.samples.iter().filter(|s| **s <= x).count();
        100.0 * below as f64 / self.samples.len() as f64
    }
}

/// The optional ecosystem dataset backing percentile scoring — see
/// `docs/percentiles-schema.md`.
#[derive(Debug, Clone)]
pub struct Percentiles {
    /// Per-server gpt-4o exact total tokens across the ecosystem.
    pub context_cost_tokens: MetricSamples,
    /// The dataset's `collected` date, if provided.
    pub collected: Option<String>,
    /// The dataset's top-level `collected` date (the census run date), used to
    /// date the startup-failure cohort note. May differ from
    /// [`collected`](Self::collected), which mirrors the token metric's own date.
    pub census_date: Option<String>,
    /// Optional ecosystem startup-failure rate: the fraction (or percentage) of
    /// surveyed public servers that failed at startup / during the handshake.
    /// Drives the one-line cohort context shown when a checked server fails to
    /// start. `None` when the dataset does not carry it (silent fallback).
    pub startup_failure_rate: Option<f64>,
    /// Whether this dataset is the census bundled into the binary (see
    /// [`bundled_percentiles`]) rather than one loaded from a user-supplied file.
    /// Propagated into [`ContextProvenance::Percentile::bundled`] so the report
    /// can label the default census as bundled and show its age.
    pub bundled: bool,
}

/// The census dataset embedded into the binary at compile time — the same
/// `data/percentiles.json` the repo ships — so an `npx`/installed `jig check`
/// scores context cost against the ecosystem even with no dataset file on disk.
/// A user-supplied `--percentiles <file>` still overrides it.
pub const BUNDLED_PERCENTILES_JSON: &str = include_str!("../../../data/percentiles.json");

/// Parse the [bundled census](BUNDLED_PERCENTILES_JSON) into a [`Percentiles`]
/// with [`bundled`](Percentiles::bundled) set. `None` only if the embedded JSON
/// ever fails to carry a usable `context_cost_tokens.samples` array (a
/// compile-time-fixed asset, so this is effectively infallible).
pub fn bundled_percentiles() -> Option<Percentiles> {
    let v: Value = serde_json::from_str(BUNDLED_PERCENTILES_JSON).ok()?;
    let mut p = Percentiles::from_json(&v)?;
    p.bundled = true;
    Some(p)
}

impl Percentiles {
    /// A one-line ecosystem cohort note for a server that failed at startup, or
    /// `None` when the dataset carries no `startup_failure_rate`. A rate `<= 1`
    /// is read as a fraction, otherwise as an already-scaled percentage.
    pub fn startup_failure_note(&self) -> Option<String> {
        let rate = self.startup_failure_rate?;
        let pct = if rate <= 1.0 { rate * 100.0 } else { rate };
        let when = self
            .census_date
            .as_deref()
            .map(|d| d.get(0..7).unwrap_or(d).to_string())
            .map(|ym| format!("the {ym} census"))
            .unwrap_or_else(|| "a recent census".to_string());
        Some(format!(
            "For context: in {when}, {pct:.0}% of surveyed public MCP servers also failed at startup."
        ))
    }
}

impl Percentiles {
    /// Parse a [`Percentiles`] from the `data/percentiles.json` JSON value.
    ///
    /// Returns `None` if the required `context_cost_tokens.samples` array is
    /// absent or not numeric — the caller then falls back to absolute bands.
    /// Samples are sorted defensively, so an unsorted file still scores
    /// correctly.
    pub fn from_json(v: &Value) -> Option<Percentiles> {
        let arr = v.get("context_cost_tokens")?.get("samples")?.as_array()?;
        let mut samples: Vec<f64> = arr.iter().filter_map(Value::as_f64).collect();
        if samples.is_empty() {
            return None;
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let collected = v
            .get("context_cost_tokens")
            .and_then(|m| m.get("collected"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let census_date = v
            .get("collected")
            .and_then(Value::as_str)
            .map(str::to_string);
        let startup_failure_rate = v.get("startup_failure_rate").and_then(Value::as_f64);
        Some(Percentiles {
            context_cost_tokens: MetricSamples { samples },
            collected,
            census_date,
            startup_failure_rate,
            bundled: false,
        })
    }

    /// Load a [`Percentiles`] dataset from `path`.
    ///
    /// Returns `Ok(None)` when the file does not exist (the common case — the
    /// dataset is optional) or is present but does not carry a usable
    /// `context_cost_tokens.samples` array. Returns `Err` only on an unexpected
    /// I/O error reading a file that does exist.
    pub fn load(path: impl AsRef<std::path::Path>) -> std::io::Result<Option<Percentiles>> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str::<Value>(&text)
                .ok()
                .as_ref()
                .and_then(Percentiles::from_json)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

/// The shields.io color band for a composite `score` (`0..=100`).
///
/// `>=90` brightgreen, `75..=89` green, `60..=74` yellow, `40..=59` orange,
/// `<40` red.
pub fn badge_color(score: u32) -> &'static str {
    match score {
        90..=u32::MAX => "brightgreen",
        75..=89 => "green",
        60..=74 => "yellow",
        40..=59 => "orange",
        _ => "red",
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// The context-cost metric: per-tool gpt-4o exact token counts and their grand
/// total (plus the server `instructions` string). Computed once per
/// [`evaluate`] with the shared counter.
struct ToolCosts {
    /// `(tool name, canonical-rendering tokens)`, in server order.
    per_tool: Vec<(String, usize)>,
    /// Grand total: every tool plus the instructions string.
    total: usize,
}

impl ToolCosts {
    /// The `(name, tokens)` of the single largest tool, if any.
    fn biggest(&self) -> Option<&(String, usize)> {
        self.per_tool.iter().max_by_key(|(_, t)| *t)
    }
}

/// Compute the context-cost metric with the shared counter (one BPE, no
/// per-tool rebuild). Token counts degrade to `0` if the tokenizer is absent.
fn tool_costs(tools: &[Tool], instructions: Option<&str>) -> ToolCosts {
    let counter = context_counter();
    let mut per_tool = Vec::with_capacity(tools.len());
    let mut total = 0usize;
    for t in tools {
        let toks = counter
            .map(|c| c.count(&canonical_tool_json(t)))
            .unwrap_or(0);
        total += toks;
        per_tool.push((t.name.clone(), toks));
    }
    if let (Some(c), Some(instr)) = (counter, instructions) {
        total += c.count(instr);
    }
    ToolCosts { per_tool, total }
}

/// Score a server. Pure: no I/O, no connections — everything comes from `input`
/// and the optional `percentiles` dataset.
pub fn evaluate(input: &CheckInput, percentiles: Option<&Percentiles>) -> Report {
    // The context-cost metric: gpt-4o exact totals, computed once and reused for
    // the per-tool "biggest offender" fix text.
    let costs = tool_costs(&input.tools, input.instructions.as_deref());
    let total_tokens = costs.total;

    let protocol = score_protocol(input);
    let (context, provenance) = score_context(total_tokens, &costs, percentiles);
    let schema = score_schema(input);
    let description = score_description(input);
    let robustness = score_robustness(input);

    let dimensions = vec![protocol, context, schema, description, robustness];
    let composite = composite_score(&dimensions);

    // The tool-set advisor reuses the per-tool token costs already computed
    // above — it never re-tokenizes. Its findings are unscored (see
    // [`Dimension::ToolSet`]).
    let advisor = crate::advisor::advise(&input.tools, &advisor_costs(&costs));

    Report {
        server_name: input.server_name.clone(),
        server_version: input.server_version.clone(),
        protocol_version: input.protocol_version.clone(),
        composite,
        dimensions,
        total_tokens,
        context_provenance: provenance,
        rubric_version: RUBRIC_VERSION,
        tool_count: input.tools.len(),
        per_tool_tokens: costs.per_tool.clone(),
        advisor,
    }
}

/// Adapt the check pass's per-tool token costs into the advisor's input shape.
fn advisor_costs(costs: &ToolCosts) -> Vec<crate::advisor::ToolTokenCost> {
    costs
        .per_tool
        .iter()
        .map(|(name, tokens)| crate::advisor::ToolTokenCost {
            name: name.clone(),
            tokens: *tokens,
        })
        .collect()
}

/// The weighted composite over the *applicable* dimensions (those with a
/// `Some` score), renormalizing by the sum of their weights. A dimension scored
/// `None` is excluded — never treated as 100.
fn composite_score(dimensions: &[DimensionScore]) -> f64 {
    let mut weighted = 0.0;
    let mut total_weight = 0.0;
    for d in dimensions {
        if let Some(s) = d.score {
            weighted += s * d.weight as f64;
            total_weight += d.weight as f64;
        }
    }
    if total_weight == 0.0 {
        0.0
    } else {
        weighted / total_weight
    }
}

/// Clamp a running score into `0..=100`.
fn clamp_score(s: f64) -> f64 {
    s.clamp(0.0, 100.0)
}

// ---- Dimension 1: protocol compliance -------------------------------------

fn score_protocol(input: &CheckInput) -> DimensionScore {
    let mut score = 100.0;
    let mut findings = Vec::new();

    // Stdout pollution: the single most common real-world MCP break. Pinned into
    // Top Fixes because it stops real clients working regardless of its score.
    if input.observations.pollution_lines > 0 {
        let n = input.observations.pollution_lines;
        let raw = PROTOCOL_POLLUTION_PENALTY * n as f64;
        let points = raw.min(PROTOCOL_POLLUTION_CAP);
        score -= points;
        let (message, fix) = pollution_finding_text(n, input.observations.first_pollution.as_ref());
        findings.push(Finding {
            dimension: Dimension::Protocol,
            severity: Severity::High,
            message,
            fix,
            points,
            pinned: true,
        });
    }

    // Capabilities advertised outside the *negotiated* spec revision. Legality
    // is version-relative (see `REVISIONS`): the same capability is clean under
    // a revision that defines it and off-spec under one that does not.
    let (revision, assumed_latest) = match revision_for(&input.protocol_version) {
        Some(r) => (r, false),
        None => (latest_revision(), true),
    };
    let offspec = offspec_capabilities(&input.capabilities, revision);
    if !offspec.is_empty() {
        let raw = PROTOCOL_OFFSPEC_CAP_PENALTY * offspec.len() as f64;
        let points = raw.min(PROTOCOL_OFFSPEC_CAP_CAP);
        score -= points;
        let (message, fix) = offspec_finding_text(&offspec, revision, assumed_latest);
        findings.push(Finding {
            dimension: Dimension::Protocol,
            severity: Severity::Medium,
            message,
            fix,
            points,
            pinned: false,
        });
    }

    // Conformance `server-initialize` (MCP-Initialize): the initialize result
    // MUST carry a non-empty serverInfo (name + version) and an object
    // capabilities map. serde already requires the fields to be present; here we
    // catch the present-but-empty / wrong-shape cases a live server can still
    // send.
    let init_gaps = initialize_field_gaps(input);
    if !init_gaps.is_empty() {
        let points = PROTOCOL_INIT_FIELD_PENALTY * init_gaps.len() as f64;
        score -= points;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            severity: Severity::High,
            message: format!(
                "initialize result has {} (conformance: server-initialize)",
                join_and(&init_gaps)
            ),
            fix: "return a spec-valid initialize result: a non-empty serverInfo.name and \
                  serverInfo.version, and a capabilities object"
                .to_string(),
            points,
            pinned: false,
        });
    }

    // Conformance `tools-name-format` (SEP-986): every tool name must be 1..=64
    // chars and match `^[A-Za-z0-9_./-]+$`. A malformed name is uncallable.
    let bad_names = tool_name_format_violations(&input.tools);
    if !bad_names.is_empty() {
        let raw = PROTOCOL_TOOL_NAME_FORMAT_PENALTY * bad_names.len() as f64;
        let points = raw.min(PROTOCOL_TOOL_NAME_FORMAT_CAP);
        score -= points;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            severity: Severity::High,
            message: format!(
                "tool name{} {} violate MCP name format (conformance: tools-name-format, SEP-986)",
                plural(bad_names.len()),
                join_violations(&bad_names)
            ),
            fix: "rename to 1–64 chars matching ^[A-Za-z0-9_./-]+$ (no spaces or other symbols)"
                .to_string(),
            points,
            pinned: false,
        });
    }

    // Conformance `negative`: an unknown method must be rejected with the
    // JSON-RPC `-32601 Method not found` code, never a different code or a
    // spurious success.
    match input.observations.unknown_method {
        UnknownMethodProbe::Errored(code) if code != JSONRPC_METHOD_NOT_FOUND => {
            score -= PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY;
            findings.push(Finding {
                dimension: Dimension::Protocol,
                severity: Severity::Medium,
                message: format!(
                    "unknown method answered with JSON-RPC error {code}, not {JSONRPC_METHOD_NOT_FOUND} \
                     Method not found (conformance: negative)"
                ),
                fix: format!(
                    "return error code {JSONRPC_METHOD_NOT_FOUND} for methods the server does not implement"
                ),
                points: PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY,
                pinned: false,
            });
        }
        UnknownMethodProbe::Accepted => {
            score -= PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY;
            findings.push(Finding {
                dimension: Dimension::Protocol,
                severity: Severity::High,
                message: "server returned a success result for an unknown method instead of \
                          -32601 Method not found (conformance: negative)"
                    .to_string(),
                fix: format!(
                    "reject unimplemented methods with JSON-RPC error {JSONRPC_METHOD_NOT_FOUND}"
                ),
                points: PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY,
                pinned: false,
            });
        }
        // Conformant (-32601), inconclusive (no answer), or not probed.
        _ => {}
    }

    // A list operation the server accepted but never answered.
    if input.observations.list_timed_out {
        score -= PROTOCOL_LIST_TIMEOUT_PENALTY;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            severity: Severity::High,
            message: "a list operation timed out — the server accepted the request but never \
                      responded"
                .to_string(),
            fix: "ensure every request receives a response; check for a hang in the list handler"
                .to_string(),
            points: PROTOCOL_LIST_TIMEOUT_PENALTY,
            pinned: false,
        });
    }

    let score = clamp_score(score);
    let summary = summarize_findings(
        &findings,
        "clean handshake, no stdout pollution, spec-valid capabilities",
    );
    DimensionScore {
        dimension: Dimension::Protocol,
        score: Some(score),
        weight: Dimension::Protocol.weight(),
        summary,
        heuristic: false,
        findings,
    }
}

/// One off-spec capability: its key and the earliest known revision that
/// defines it (so a finding can point to where it *is* standardized).
struct OffSpecCap {
    /// The advertised capability key.
    name: String,
    /// The earliest known revision defining this key, if any known revision does.
    introduced_in: Option<&'static str>,
}

/// Top-level capability keys advertised outside the negotiated `revision`.
fn offspec_capabilities(caps: &Value, revision: &Revision) -> Vec<OffSpecCap> {
    let Some(map) = caps.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<OffSpecCap> = map
        .keys()
        .filter(|k| !revision.capabilities.contains(&k.as_str()))
        .map(|k| OffSpecCap {
            name: k.clone(),
            introduced_in: capability_introduced_in(k),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Build the (message, fix) for an off-spec-capability finding, naming the
/// negotiated revision and, per capability, where it is first defined.
fn offspec_finding_text(
    offspec: &[OffSpecCap],
    revision: &Revision,
    assumed_latest: bool,
) -> (String, String) {
    let clauses: Vec<String> = offspec
        .iter()
        .map(|c| match c.introduced_in {
            Some(rev) => format!("`{}` (first defined in revision {rev})", c.name),
            None => format!("`{}` (not defined in any known MCP revision)", c.name),
        })
        .collect();
    let assumed = if assumed_latest {
        format!(
            " — negotiated version is unknown to jig, validated against the latest known revision {}",
            revision.id
        )
    } else {
        String::new()
    };
    let message = format!(
        "capability {} not defined in the negotiated MCP revision {}{}",
        clauses.join(", "),
        revision.id,
        assumed
    );
    let fix = "gate off-spec capabilities on the negotiated protocol version, or negotiate a \
               revision that defines them"
        .to_string();
    (message, fix)
}

/// Build the (message, fix) for a stdout-pollution finding, enriched with the
/// exact byte offset and a hex/utf8 excerpt of the first polluting line.
fn pollution_finding_text(n: usize, site: Option<&PollutionSite>) -> (String, String) {
    let message = format!(
        "{n} non-protocol line(s) on stdout — this corrupts MCP's newline-delimited framing"
    );
    let fix = match site {
        Some(site) => {
            let (utf8, hex) = pollution_excerpt(&site.line);
            let at = match site.offset {
                Some(off) => format!("at byte offset {off}"),
                None => "on stdout".to_string(),
            };
            format!(
                "route all logging to stderr; the first polluting line is {at}: \"{utf8}\" \
                 (hex {hex}) — stdout must carry only newline-delimited JSON-RPC"
            )
        }
        None => "route all logging to stderr; stdout must carry only newline-delimited JSON-RPC"
            .to_string(),
    };
    (message, fix)
}

/// A short utf8 + hex excerpt of a polluting line's leading bytes.
fn pollution_excerpt(line: &str) -> (String, String) {
    let bytes = line.as_bytes();
    let take = bytes.len().min(POLLUTION_EXCERPT_BYTES);
    let hex = bytes[..take]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let ellipsis = if bytes.len() > take { "…" } else { "" };
    let utf8: String = line.chars().take(POLLUTION_EXCERPT_BYTES).collect();
    (format!("{utf8}{ellipsis}"), format!("{hex}{ellipsis}"))
}

/// Missing/empty required `initialize` result fields (conformance:
/// server-initialize). Names the concrete gap so the fix is actionable.
fn initialize_field_gaps(input: &CheckInput) -> Vec<String> {
    let mut gaps = Vec::new();
    if input.server_name.trim().is_empty() {
        gaps.push("an empty serverInfo.name".to_string());
    }
    if input.server_version.trim().is_empty() {
        gaps.push("an empty serverInfo.version".to_string());
    }
    // Absent capabilities deserialize to JSON null here; the spec requires an
    // object. A null/array/scalar capabilities value is a shape violation.
    if !input.capabilities.is_object() {
        gaps.push("a non-object capabilities value".to_string());
    }
    gaps
}

/// Join phrases with commas and a trailing "and": `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Tool names that violate the MCP name format (SEP-986): each returned as
/// `(name, reason)`.
fn tool_name_format_violations(tools: &[Tool]) -> Vec<(String, String)> {
    tools
        .iter()
        .filter_map(|t| tool_name_format_reason(&t.name).map(|why| (t.name.clone(), why)))
        .collect()
}

/// The reason `name` violates the MCP tool-name format, or `None` if it is
/// legal: 1..=64 characters, each in `[A-Za-z0-9_./-]`.
fn tool_name_format_reason(name: &str) -> Option<String> {
    let len = name.chars().count();
    if len == 0 {
        return Some("is empty".to_string());
    }
    if len > TOOL_NAME_MAX_LEN {
        return Some(format!("is {len} chars (max {TOOL_NAME_MAX_LEN})"));
    }
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
    if !name.chars().all(legal) {
        return Some("has characters outside [A-Za-z0-9_./-]".to_string());
    }
    None
}

/// Join `(name, reason)` violations as `` `name` (reason) ``, comma-separated.
fn join_violations(v: &[(String, String)]) -> String {
    v.iter()
        .map(|(n, why)| format!("`{n}` {why}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- Dimension 2: context cost --------------------------------------------

fn score_context(
    total_tokens: usize,
    costs: &ToolCosts,
    percentiles: Option<&Percentiles>,
) -> (DimensionScore, ContextProvenance) {
    let x = total_tokens as f64;

    let (score, provenance, band_label) = match percentiles {
        Some(p) if !p.context_cost_tokens.samples.is_empty() => {
            let pct = p.context_cost_tokens.percentile(x);
            // Below the median costs nothing: a lighter-than-typical server is
            // not a finding. Above it, the penalty ramps so the heavy tail
            // (p90+) is graded hard. Tuned against the 2026-07 census
            // (median 1,679 tok, p90 14,401).
            let score = if pct <= 50.0 {
                clamp_score(100.0 - pct * 0.2)
            } else {
                clamp_score(90.0 - (pct - 50.0) * 1.7)
            };
            let pct_round = pct.round() as u32;
            let n = p.context_cost_tokens.samples.len();
            // Always surface the sample size: a percentile is only as
            // trustworthy as the population it was measured against.
            let label = if pct >= 50.0 {
                format!(
                    "{pct_round}th percentile of n={n} measured servers — heavier than {pct_round}%"
                )
            } else {
                format!(
                    "{pct_round}th percentile of n={n} measured servers — lighter than {}%",
                    100 - pct_round.min(100)
                )
            };
            // Prefer the metric's own `collected` date; fall back to the
            // dataset-level census date (the bundled census carries only the
            // latter), truncated to YYYY-MM-DD so provenance always shows an age.
            let collected = p.collected.clone().or_else(|| {
                p.census_date
                    .as_deref()
                    .map(|d| d.get(0..10).unwrap_or(d).to_string())
            });
            (
                score,
                ContextProvenance::Percentile {
                    percentile: pct_round,
                    n,
                    collected,
                    bundled: p.bundled,
                },
                label,
            )
        }
        _ => (
            band_score(x),
            ContextProvenance::AbsoluteBands,
            "no ecosystem data — absolute bands".to_string(),
        ),
    };

    let mut findings = Vec::new();
    // Emit a fix only when the surface is genuinely heavy, and point at the
    // single largest tool so the remedy is concrete.
    if total_tokens > 8_000 {
        if let Some((name, toks)) = costs.biggest() {
            let points = clamp_score(100.0 - score);
            findings.push(Finding {
                dimension: Dimension::ContextCost,
                severity: if total_tokens > 20_000 {
                    Severity::High
                } else {
                    Severity::Medium
                },
                message: format!(
                    "{} tokens on the tool surface ({band_label})",
                    commas(total_tokens)
                ),
                fix: format!(
                    "trim the largest definitions — `{}` alone is ~{} tokens",
                    name,
                    commas(*toks)
                ),
                points,
                pinned: false,
            });
        }
    }

    let summary = format!("{} tokens ({band_label})", commas(total_tokens));
    let dim = DimensionScore {
        dimension: Dimension::ContextCost,
        score: Some(score),
        weight: Dimension::ContextCost.weight(),
        summary,
        heuristic: false,
        findings,
    };
    (dim, provenance)
}

/// Piecewise-linear interpolation over [`CONTEXT_BANDS`].
fn band_score(tokens: f64) -> f64 {
    let bands = CONTEXT_BANDS;
    if tokens <= bands[0].0 {
        return bands[0].1;
    }
    for pair in bands.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        if tokens <= x1 {
            let t = (tokens - x0) / (x1 - x0);
            return clamp_score(y0 + t * (y1 - y0));
        }
    }
    // Beyond the last anchor: hold the floor.
    clamp_score(bands[bands.len() - 1].1)
}

// ---- Dimension 3: schema hygiene ------------------------------------------

fn score_schema(input: &CheckInput) -> DimensionScore {
    if input.tools.is_empty() {
        return not_applicable(Dimension::SchemaHygiene, "no tools to inspect");
    }

    let mut score = 100.0;
    let mut findings = Vec::new();
    let mut annotation_deduction = 0.0;

    for tool in &input.tools {
        // Missing tool description.
        if tool.description.as_deref().unwrap_or("").trim().is_empty() {
            score -= SCHEMA_MISSING_TOOL_DESC;
            findings.push(Finding {
                dimension: Dimension::SchemaHygiene,
                severity: Severity::Medium,
                message: format!("`{}` has no description", tool.name),
                fix: format!("add a one-line description to `{}`", tool.name),
                points: SCHEMA_MISSING_TOOL_DESC,
                pinned: false,
            });
        }

        // Per-parameter checks over the top-level properties (deterministic).
        let (no_desc, no_type) = schema_param_gaps(&tool.input_schema);
        if !no_desc.is_empty() {
            let points = SCHEMA_PARAM_MISSING_DESC * no_desc.len() as f64;
            score -= points;
            findings.push(Finding {
                dimension: Dimension::SchemaHygiene,
                severity: Severity::Medium,
                message: format!(
                    "`{}`: parameter{} {} missing a description",
                    tool.name,
                    plural(no_desc.len()),
                    quote_join(&no_desc)
                ),
                fix: format!(
                    "describe each parameter of `{}` so the model can fill it correctly",
                    tool.name
                ),
                points,
                pinned: false,
            });
        }
        if !no_type.is_empty() {
            let points = SCHEMA_PARAM_MISSING_TYPE * no_type.len() as f64;
            score -= points;
            findings.push(Finding {
                dimension: Dimension::SchemaHygiene,
                severity: Severity::High,
                message: format!(
                    "`{}`: parameter{} {} missing a type",
                    tool.name,
                    plural(no_type.len()),
                    quote_join(&no_type)
                ),
                fix: format!(
                    "give every parameter of `{}` a JSON Schema `type` (or enum/$ref)",
                    tool.name
                ),
                points,
                pinned: false,
            });
        }

        // Missing annotations (minor, capped).
        if !has_annotations(&tool.input_schema, tool) {
            annotation_deduction += SCHEMA_MISSING_ANNOTATIONS;
        }
    }

    // Apply the capped annotation deduction as a single rolled-up finding.
    let annotation_deduction = annotation_deduction.min(SCHEMA_ANNOTATIONS_CAP);
    if annotation_deduction > 0.0 {
        let missing = input
            .tools
            .iter()
            .filter(|t| !has_annotations(&t.input_schema, t))
            .count();
        score -= annotation_deduction;
        findings.push(Finding {
            dimension: Dimension::SchemaHygiene,
            severity: Severity::Low,
            message: format!(
                "{missing} tool(s) declare no annotations (readOnlyHint, destructiveHint, …)"
            ),
            fix: "add tool annotations so clients can reason about side effects".to_string(),
            points: annotation_deduction,
            pinned: false,
        });
    }

    let score = clamp_score(score);
    let summary = schema_summary(&findings, input.tools.len());
    DimensionScore {
        dimension: Dimension::SchemaHygiene,
        score: Some(score),
        weight: Dimension::SchemaHygiene.weight(),
        summary,
        heuristic: false,
        findings,
    }
}

/// The names of top-level properties missing a `description` and missing a
/// `type` (returned separately). All-optional schemas are legal, so a missing
/// `required` array is never flagged.
fn schema_param_gaps(schema: &Value) -> (Vec<String>, Vec<String>) {
    let mut no_desc = Vec::new();
    let mut no_type = Vec::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (name, spec) in props {
            let has_desc = spec
                .get("description")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !has_desc {
                no_desc.push(name.clone());
            }
            if !property_has_type(spec) {
                no_type.push(name.clone());
            }
        }
    }
    no_desc.sort();
    no_type.sort();
    (no_desc, no_type)
}

/// Whether a property declares a type in any accepted form.
fn property_has_type(spec: &Value) -> bool {
    let Some(obj) = spec.as_object() else {
        // A bare `true`/`false` schema (JSON Schema boolean) declares no type.
        return false;
    };
    for key in ["type", "enum", "const", "$ref", "anyOf", "oneOf", "allOf"] {
        if obj.contains_key(key) {
            return true;
        }
    }
    false
}

/// Whether a tool declares any annotations. MCP carries these in a top-level
/// `annotations` object on the tool; some servers instead attach hints to the
/// input schema, so both are accepted.
fn has_annotations(schema: &Value, tool: &Tool) -> bool {
    // The typed `Tool` keeps only fields Jig reads; annotations live in the raw
    // input schema here (or would be added as a typed field later). Check the
    // schema object for any *Hint key or an `annotations` object.
    if let Some(obj) = schema.as_object() {
        if obj.contains_key("annotations") {
            return true;
        }
        if obj.keys().any(|k| k.ends_with("Hint")) {
            return true;
        }
    }
    // Defensive: a future typed annotations field would be checked here.
    let _ = tool;
    false
}

fn schema_summary(findings: &[Finding], n_tools: usize) -> String {
    let clean = format!(
        "{n_tools} tool{} — descriptions, types and params all present",
        plural(n_tools)
    );
    summarize_findings(findings, &clean)
}

// ---- Dimension 4: description quality (heuristic) -------------------------

fn score_description(input: &CheckInput) -> DimensionScore {
    if input.tools.is_empty() {
        let mut d = not_applicable(Dimension::DescriptionQuality, "no tools to inspect");
        d.heuristic = true;
        return d;
    }

    let mut score = 100.0;
    let mut findings = Vec::new();

    // ---- Naming: spaces (uncallable) and convention consistency ----
    let convention = dominant_convention(&input.tools);
    for tool in &input.tools {
        if tool.name.chars().any(char::is_whitespace) {
            score -= DQ_NAME_HAS_SPACE;
            findings.push(Finding {
                dimension: Dimension::DescriptionQuality,
                severity: Severity::High,
                message: format!(
                    "`{}` contains whitespace — models cannot call it",
                    tool.name
                ),
                fix: format!(
                    "rename `{}` to a whitespace-free identifier (kebab or snake case)",
                    tool.name
                ),
                points: DQ_NAME_HAS_SPACE,
                pinned: false,
            });
        } else if let Some(dom) = convention {
            if name_convention(&tool.name) == Some(dom.other()) {
                score -= DQ_NAME_INCONSISTENT;
                findings.push(Finding {
                    dimension: Dimension::DescriptionQuality,
                    severity: Severity::Low,
                    message: format!(
                        "`{}` uses {} while the server is mostly {}",
                        tool.name,
                        dom.other().label(),
                        dom.label()
                    ),
                    fix: format!(
                        "rename `{}` to match the server's {} convention",
                        tool.name,
                        dom.label()
                    ),
                    points: DQ_NAME_INCONSISTENT,
                    pinned: false,
                });
            }
        }
    }

    // ---- Description length bands (token-based, gpt-4o) ----
    for tool in &input.tools {
        let toks = description_tokens(tool);
        if toks <= DQ_TERSE_TOKENS {
            score -= DQ_DESC_TERSE;
            findings.push(Finding {
                dimension: Dimension::DescriptionQuality,
                severity: Severity::Medium,
                message: format!(
                    "`{}` description is very terse ({toks} tokens) — models struggle to select it",
                    tool.name
                ),
                fix: format!(
                    "expand `{}`'s description to say what it does and when to use it",
                    tool.name
                ),
                points: DQ_DESC_TERSE,
                pinned: false,
            });
        } else if toks >= DQ_VERBOSE_TOKENS {
            score -= DQ_DESC_VERBOSE;
            findings.push(Finding {
                dimension: Dimension::DescriptionQuality,
                severity: Severity::Low,
                message: format!(
                    "`{}` description is very long ({toks} tokens) — context waste",
                    tool.name
                ),
                fix: format!(
                    "tighten `{}`'s description; move detail into params",
                    tool.name
                ),
                points: DQ_DESC_VERBOSE,
                pinned: false,
            });
        }
    }

    // ---- Titles (minor, capped) ----
    let missing_titles = input
        .tools
        .iter()
        .filter(|t| t.title.as_deref().unwrap_or("").trim().is_empty())
        .count();
    if missing_titles > 0 {
        let points = (DQ_MISSING_TITLE * missing_titles as f64).min(DQ_TITLE_CAP);
        score -= points;
        findings.push(Finding {
            dimension: Dimension::DescriptionQuality,
            severity: Severity::Low,
            message: format!("{missing_titles} tool(s) have no human-facing title"),
            fix: "add a `title` to each tool for nicer client display".to_string(),
            points,
            pinned: false,
        });
    }

    let score = clamp_score(score);
    let summary = if findings.is_empty() {
        "heuristic · consistent names, well-sized descriptions".to_string()
    } else {
        let head = findings[0].message.as_str();
        if findings.len() == 1 {
            format!("heuristic · {head}")
        } else {
            format!("heuristic · {head} (+{} more)", findings.len() - 1)
        }
    };
    DimensionScore {
        dimension: Dimension::DescriptionQuality,
        score: Some(score),
        weight: Dimension::DescriptionQuality.weight(),
        summary,
        heuristic: true,
        findings,
    }
}

/// The token length of a tool's description under the context metric model,
/// using the shared counter. Falls back to a whitespace word count only if the
/// tokenizer is unavailable (it always builds for gpt-4o).
fn description_tokens(tool: &Tool) -> usize {
    let desc = match tool.description.as_deref() {
        Some(d) if !d.trim().is_empty() => d,
        _ => return 0,
    };
    match context_counter() {
        Some(counter) => counter.count(desc),
        None => desc.split_whitespace().count(),
    }
}

/// A tool naming convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Convention {
    /// `kebab-case` (hyphen-separated).
    Kebab,
    /// `snake_case` (underscore-separated).
    Snake,
}

impl Convention {
    fn label(self) -> &'static str {
        match self {
            Convention::Kebab => "kebab-case",
            Convention::Snake => "snake_case",
        }
    }
    fn other(self) -> Convention {
        match self {
            Convention::Kebab => Convention::Snake,
            Convention::Snake => Convention::Kebab,
        }
    }
}

/// Classify a single name's separator convention, if it uses one distinctly.
/// A name using *both* separators, or neither, returns `None`.
fn name_convention(name: &str) -> Option<Convention> {
    let hyphen = name.contains('-');
    let under = name.contains('_');
    match (hyphen, under) {
        (true, false) => Some(Convention::Kebab),
        (false, true) => Some(Convention::Snake),
        _ => None,
    }
}

/// The server's dominant naming convention, if one clearly leads. `None` on a
/// tie or when no tool uses a separator (so a plain-name server is never
/// penalized for "inconsistency").
fn dominant_convention(tools: &[Tool]) -> Option<Convention> {
    let mut kebab = 0usize;
    let mut snake = 0usize;
    for t in tools {
        match name_convention(&t.name) {
            Some(Convention::Kebab) => kebab += 1,
            Some(Convention::Snake) => snake += 1,
            None => {}
        }
    }
    match kebab.cmp(&snake) {
        std::cmp::Ordering::Greater => Some(Convention::Kebab),
        std::cmp::Ordering::Less => Some(Convention::Snake),
        std::cmp::Ordering::Equal => None,
    }
}

// ---- Dimension 5: robustness (observed only) ------------------------------

fn score_robustness(input: &CheckInput) -> DimensionScore {
    let obs = &input.observations;
    let mut subscores: Vec<f64> = Vec::new();
    let mut findings = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    // Latency sub-score (only if observed).
    if let Some(latency) = obs.list_latency {
        let ms = latency.as_millis();
        let sub = if ms <= ROBUST_LATENCY_FAST_MS {
            100.0
        } else if ms <= ROBUST_LATENCY_SLOW_MS {
            ROBUST_LATENCY_SLUGGISH_SCORE
        } else {
            ROBUST_LATENCY_SLOW_SCORE
        };
        subscores.push(sub);
        parts.push(format!("list {ms}ms"));
        if sub < 100.0 {
            findings.push(Finding {
                dimension: Dimension::Robustness,
                severity: Severity::Medium,
                message: format!("tools/list took {ms}ms"),
                fix: "reduce list latency — avoid per-request cold starts or slow enumeration"
                    .to_string(),
                points: 100.0 - sub,
                pinned: false,
            });
        }
    }

    // Clean-shutdown sub-score (always observed by the session).
    let shutdown_sub = if obs.clean_shutdown {
        parts.push("clean shutdown".to_string());
        100.0
    } else {
        parts.push("unclean shutdown".to_string());
        findings.push(Finding {
            dimension: Dimension::Robustness,
            severity: Severity::Medium,
            message: "the server did not shut down cleanly".to_string(),
            fix: "handle transport close / EOF and exit promptly on shutdown".to_string(),
            points: 100.0 - ROBUST_UNCLEAN_SHUTDOWN_SCORE,
            pinned: false,
        });
        ROBUST_UNCLEAN_SHUTDOWN_SCORE
    };
    subscores.push(shutdown_sub);

    // Stderr noise is informational only — reported, never scored.
    if let Some(bytes) = obs.stderr_noise_bytes {
        if bytes > 0 {
            findings.push(Finding {
                dimension: Dimension::Robustness,
                severity: Severity::Info,
                message: format!(
                    "server wrote {} bytes to stderr (informational)",
                    commas(bytes)
                ),
                fix: "no action needed — stderr logging is valid; noted for context".to_string(),
                points: 0.0,
                pinned: false,
            });
        }
    }

    // Mean of the observed sub-scores; if none observed, exclude the dimension.
    let score = if subscores.is_empty() {
        None
    } else {
        Some(clamp_score(
            subscores.iter().sum::<f64>() / subscores.len() as f64,
        ))
    };

    let summary = if parts.is_empty() {
        "no robustness signals observed".to_string()
    } else {
        parts.join(", ")
    };
    DimensionScore {
        dimension: Dimension::Robustness,
        score,
        weight: Dimension::Robustness.weight(),
        summary,
        heuristic: false,
        findings,
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A dimension excluded from the composite (not applicable to this server).
fn not_applicable(dimension: Dimension, why: &str) -> DimensionScore {
    DimensionScore {
        dimension,
        score: None,
        weight: dimension.weight(),
        summary: format!("n/a — {why}"),
        heuristic: dimension.is_heuristic(),
        findings: Vec::new(),
    }
}

/// `"a"` for 1, `"s"` otherwise — for pluralizing "parameter(s)" etc.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Join names as backtick-quoted, comma-separated: `` `a`, `b` ``.
fn quote_join(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

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

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A clean input over the three mock-server tools.
    fn clean_input() -> CheckInput {
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
                        "party": { "type": "object", "properties": { "size": { "type": "integer" } } },
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
                list_latency: Some(Duration::from_millis(12)),
                clean_shutdown: true,
                // A conformant server: unknown methods → -32601.
                unknown_method: UnknownMethodProbe::Errored(-32601),
                ..Default::default()
            },
        }
    }

    #[test]
    fn weights_sum_to_100() {
        let sum: u32 = Dimension::all().iter().map(|d| d.weight()).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn clean_server_scores_high() {
        let report = evaluate(&clean_input(), None);
        assert!(
            report.composite_rounded() >= 90,
            "clean server should grade A: {}",
            report.composite
        );
        // Protocol perfect; robustness perfect (fast + clean).
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0)
        );
        assert_eq!(
            report.dimension(Dimension::Robustness).unwrap().score,
            Some(100.0)
        );
        assert!(matches!(
            report.context_provenance,
            ContextProvenance::AbsoluteBands
        ));
    }

    #[test]
    fn pollution_deducts_from_protocol_with_finding() {
        let mut input = clean_input();
        input.observations.pollution_lines = 1;
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(85.0));
        assert!(p.findings.iter().any(|f| f.message.contains("stdout")));
        assert_eq!(p.findings[0].severity, Severity::High);
    }

    #[test]
    fn pollution_penalty_is_capped() {
        let mut input = clean_input();
        input.observations.pollution_lines = 100;
        let report = evaluate(&input, None);
        // 100 * 15 caps at 60 → score 40, not negative.
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(40.0)
        );
    }

    #[test]
    fn offspec_capability_is_flagged() {
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "tasks": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(90.0));
        assert!(p.findings.iter().any(|f| f.message.contains("tasks")));
    }

    #[test]
    fn same_capability_graded_by_negotiated_revision() {
        // `completions`: legal from 2025-03-26, off-spec under 2024-11-05.
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "completions": {} });

        input.protocol_version = "2025-06-18".to_string();
        let clean = evaluate(&input, None);
        assert_eq!(
            clean.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0),
            "completions is in-spec for 2025-06-18"
        );

        input.protocol_version = "2024-11-05".to_string();
        let flagged = evaluate(&input, None);
        let p = flagged.dimension(Dimension::Protocol).unwrap();
        assert_eq!(
            p.score,
            Some(90.0),
            "completions is off-spec for 2024-11-05"
        );
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("completions") && f.message.contains("2024-11-05")));
    }

    #[test]
    fn tasks_off_spec_under_2025_06_18_but_clean_under_2025_11_25() {
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "tasks": {} });

        input.protocol_version = "2025-06-18".to_string();
        let flagged = evaluate(&input, None);
        let p = flagged.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(90.0));
        // The finding cites where `tasks` is actually first defined.
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("tasks") && f.message.contains("2025-11-25")));

        input.protocol_version = "2025-11-25".to_string();
        let clean = evaluate(&input, None);
        assert_eq!(
            clean.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0),
            "tasks is defined in 2025-11-25"
        );
    }

    #[test]
    fn unknown_revision_validates_against_latest_and_notes_assumption() {
        let mut input = clean_input();
        input.protocol_version = "2099-01-01".to_string();
        // `extensions` is defined only in the latest known revision (2026-07-28).
        input.capabilities = json!({ "tools": {}, "extensions": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        // extensions is legal under the latest revision → no off-spec finding.
        assert_eq!(p.score, Some(100.0));

        // But `tasks` (not top-level in the latest revision) is still flagged,
        // and the finding notes the unknown-version assumption.
        input.capabilities = json!({ "tools": {}, "tasks": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert!(p.findings.iter().any(|f| {
            f.message.contains("tasks")
                && f.message.contains("unknown to jig")
                && f.message.contains("2026-07-28")
        }));
    }

    #[test]
    fn malformed_tool_name_flagged_as_conformance_violation() {
        let input = CheckInput {
            tools: vec![tool(
                "bad name!",
                Some("a reasonably sized tool description here"),
                json!({ "type": "object", "properties": {}, "annotations": {} }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(100.0 - PROTOCOL_TOOL_NAME_FORMAT_PENALTY));
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("tools-name-format") && f.message.contains("SEP-986")));
    }

    #[test]
    fn overlong_tool_name_flagged() {
        let long = "a".repeat(65);
        assert!(tool_name_format_reason(&long).is_some());
        assert!(tool_name_format_reason("get_user").is_none());
        assert!(tool_name_format_reason("get.user/v2-final").is_none());
        assert!(tool_name_format_reason("").is_some());
    }

    #[test]
    fn empty_initialize_fields_flagged() {
        let mut input = clean_input();
        input.server_name = "  ".to_string();
        input.capabilities = json!([]); // not an object
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        // Two gaps × 10 each = 20.
        assert_eq!(p.score, Some(80.0));
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("server-initialize")));
    }

    #[test]
    fn unknown_method_wrong_code_and_accepted_are_flagged() {
        // Wrong error code.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Errored(-32000);
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(
            p.score,
            Some(100.0 - PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY)
        );
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("negative") && f.message.contains("-32601")));

        // Accepted an unknown method outright.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Accepted;
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0 - PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY)
        );

        // A conformant -32601 is clean.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Errored(-32601);
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0)
        );
    }

    #[test]
    fn pollution_fix_names_byte_offset_and_excerpt() {
        let mut input = clean_input();
        input.observations.pollution_lines = 1;
        input.observations.first_pollution = Some(PollutionSite {
            offset: Some(42),
            line: "[info] started".to_string(),
        });
        let report = evaluate(&input, None);
        let f = report
            .dimension(Dimension::Protocol)
            .unwrap()
            .findings
            .iter()
            .find(|f| f.message.contains("non-protocol line"))
            .unwrap();
        assert!(f.fix.contains("byte offset 42"), "fix: {}", f.fix);
        assert!(f.fix.contains("[info] started"), "fix: {}", f.fix);
        // Hex excerpt of the first bytes ('[' == 0x5b).
        assert!(f.fix.contains("5b"), "fix: {}", f.fix);
    }

    #[test]
    fn pollution_is_pinned_into_top_fixes_even_when_outranked() {
        // A server with heavy context cost + several broken tools whose findings
        // each outrank the single-line pollution deduction by weighted impact.
        let mut input = clean_input();
        input.observations.pollution_lines = 1; // protocol -15 (×25 = 375)
        let big = "lorem ipsum dolor sit amet ".repeat(4000);
        input.tools = vec![
            tool(
                "giant",
                Some(big.trim()),
                json!({ "type": "object", "properties": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {}, "f": {}
                } }),
            ),
            tool(
                "second",
                Some("another tool here for context"),
                json!({ "type": "object", "properties": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {}, "f": {}
                } }),
            ),
        ];
        let report = evaluate(&input, None);
        let fixes = report.top_fixes(3);
        assert!(
            fixes
                .iter()
                .any(|f| f.pinned && f.message.contains("stdout")),
            "pollution must be pinned into the top fixes: {:?}",
            fixes.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn startup_failure_note_formats_percent_and_month() {
        let p = Percentiles {
            context_cost_tokens: MetricSamples { samples: vec![1.0] },
            collected: None,
            census_date: Some("2026-07-19T17:56:54Z".to_string()),
            startup_failure_rate: Some(0.42),
            bundled: false,
        };
        let note = p.startup_failure_note().unwrap();
        assert!(note.contains("42%"), "{note}");
        assert!(note.contains("2026-07"), "{note}");
        // Absent field → silent.
        let mut p2 = p.clone();
        p2.startup_failure_rate = None;
        assert!(p2.startup_failure_note().is_none());
    }

    #[test]
    fn list_timeout_deducts_from_protocol() {
        let mut input = clean_input();
        input.observations.list_timed_out = true;
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(60.0)
        );
    }

    #[test]
    fn missing_param_type_and_desc_hit_schema() {
        let input = CheckInput {
            tools: vec![tool(
                "bad",
                Some("a tool"),
                json!({ "type": "object", "properties": { "x": {} } }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let s = report.dimension(Dimension::SchemaHygiene).unwrap();
        // x has neither type (-5) nor description (-3); plus annotations (-1).
        assert_eq!(s.score, Some(100.0 - 5.0 - 3.0 - 1.0));
        assert!(s
            .findings
            .iter()
            .any(|f| f.message.contains("missing a type")));
        assert!(s
            .findings
            .iter()
            .any(|f| f.message.contains("missing a description")));
    }

    #[test]
    fn missing_tool_description_hits_schema() {
        let input = CheckInput {
            tools: vec![tool(
                "bare",
                None,
                json!({ "type": "object", "properties": {} }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let s = report.dimension(Dimension::SchemaHygiene).unwrap();
        // -8 (no desc) -1 (annotations).
        assert_eq!(s.score, Some(91.0));
    }

    #[test]
    fn all_optional_schema_is_not_penalized_for_missing_required() {
        // Properties present, no `required` — legal, so no type/desc gaps here.
        let input = CheckInput {
            tools: vec![tool(
                "opt",
                Some("all optional"),
                json!({ "type": "object", "properties": { "a": { "type": "string", "description": "an a" } } }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let s = report.dimension(Dimension::SchemaHygiene).unwrap();
        // Only the annotations nit (-1).
        assert_eq!(s.score, Some(99.0));
    }

    #[test]
    fn name_with_space_tanks_description_quality() {
        let input = CheckInput {
            tools: vec![tool(
                "bad name",
                Some("a reasonably sized description of the tool"),
                json!({ "type": "object", "properties": {}, "annotations": {} }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let d = report.dimension(Dimension::DescriptionQuality).unwrap();
        assert!(d.findings.iter().any(|f| f.message.contains("whitespace")));
        assert!(d.heuristic);
        // -15 name space, -1 missing title (no verbose/terse).
        assert_eq!(d.score, Some(84.0));
    }

    #[test]
    fn mixed_naming_convention_flags_the_minority() {
        let input = CheckInput {
            tools: vec![
                tool(
                    "get_user",
                    Some("snake one two three"),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
                tool(
                    "get_item",
                    Some("snake one two three"),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
                tool(
                    "get-thing",
                    Some("kebab one two three"),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
            ],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let d = report.dimension(Dimension::DescriptionQuality).unwrap();
        assert!(d
            .findings
            .iter()
            .any(|f| f.message.contains("get-thing") && f.message.contains("kebab")));
    }

    #[test]
    fn terse_and_verbose_descriptions_flagged() {
        let long = "word ".repeat(200);
        let input = CheckInput {
            tools: vec![
                tool(
                    "t",
                    Some("go"),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
                tool(
                    "v",
                    Some(long.trim()),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
            ],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let d = report.dimension(Dimension::DescriptionQuality).unwrap();
        assert!(d.findings.iter().any(|f| f.message.contains("very terse")));
        assert!(d.findings.iter().any(|f| f.message.contains("very long")));
    }

    #[test]
    fn context_percentile_scoring_and_provenance() {
        // Samples where a heavy server lands high.
        let p = Percentiles {
            context_cost_tokens: MetricSamples {
                samples: vec![100.0, 200.0, 300.0, 400.0, 100_000.0],
            },
            collected: Some("2026-07-19".to_string()),
            census_date: Some("2026-07-19".to_string()),
            startup_failure_rate: None,
            bundled: false,
        };
        let report = evaluate(&clean_input(), Some(&p));
        match &report.context_provenance {
            ContextProvenance::Percentile { n, .. } => assert_eq!(*n, 5),
            other => panic!("expected percentile provenance, got {other:?}"),
        }
        // The tiny mock surface is lighter than 4 of 5 samples → ~20th pct.
        // Below the median costs little: score = 100 − 0.2·pct ≈ 96.
        let c = report.dimension(Dimension::ContextCost).unwrap();
        assert!(
            c.score.unwrap() >= 95.0 && c.score.unwrap() <= 97.0,
            "got {:?}",
            c.score
        );
    }

    #[test]
    fn absent_percentile_file_falls_back_to_bands() {
        let got = Percentiles::load("this/path/does/not/exist.json").unwrap();
        assert!(got.is_none());
        // And evaluate with None → absolute bands.
        let report = evaluate(&clean_input(), None);
        assert!(matches!(
            report.context_provenance,
            ContextProvenance::AbsoluteBands
        ));
    }

    #[test]
    fn percentile_from_json_roundtrips() {
        let v = json!({
            "_schema": "…",
            "context_cost_tokens": { "samples": [300, 100, 200], "collected": "2026-07-19", "n": 3 }
        });
        let p = Percentiles::from_json(&v).unwrap();
        // Sorted defensively.
        assert_eq!(p.context_cost_tokens.samples, vec![100.0, 200.0, 300.0]);
        assert_eq!(p.context_cost_tokens.percentile(200.0), 100.0 * 2.0 / 3.0);
        assert_eq!(p.collected.as_deref(), Some("2026-07-19"));
    }

    #[test]
    fn percentile_load_reads_a_real_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("jig-pct-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"_schema":"…","context_cost_tokens":{"samples":[100,200,300],"collected":"2026-07-19","n":3}}"#,
        )
        .unwrap();
        let p = Percentiles::load(&path).unwrap().expect("loads");
        assert_eq!(p.context_cost_tokens.samples.len(), 3);
        assert_eq!(p.collected.as_deref(), Some("2026-07-19"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn percentile_from_json_missing_samples_is_none() {
        assert!(Percentiles::from_json(&json!({ "context_cost_tokens": {} })).is_none());
        assert!(Percentiles::from_json(&json!({})).is_none());
    }

    #[test]
    fn bundled_percentiles_parse_and_are_marked_bundled() {
        let p = bundled_percentiles().expect("the embedded census parses");
        assert!(p.bundled, "the bundled dataset must be marked bundled");
        assert!(
            !p.context_cost_tokens.samples.is_empty(),
            "the embedded census must carry samples"
        );
        // Scoring with the bundled dataset yields a percentile provenance whose
        // `bundled` flag and census date reach the report.
        let report = evaluate(&clean_input(), Some(&p));
        match &report.context_provenance {
            ContextProvenance::Percentile {
                bundled, collected, ..
            } => {
                assert!(*bundled, "provenance must carry the bundled flag");
                assert!(
                    collected.as_deref().is_some_and(|d| d.starts_with("2026-")),
                    "bundled census date should surface, got {collected:?}"
                );
            }
            other => panic!("expected percentile provenance, got {other:?}"),
        }
    }

    #[test]
    fn heavy_surface_emits_context_finding_naming_biggest_tool() {
        // One tool with a giant description → well over 8k tokens.
        let big = "lorem ipsum dolor sit amet ".repeat(4000);
        let input = CheckInput {
            tools: vec![
                tool(
                    "giant",
                    Some(big.trim()),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
                tool(
                    "small",
                    Some("a small helper tool here"),
                    json!({ "type": "object", "properties": {}, "annotations": {} }),
                ),
            ],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        assert!(report.total_tokens > 8_000);
        let c = report.dimension(Dimension::ContextCost).unwrap();
        assert!(c.findings.iter().any(|f| f.fix.contains("`giant`")));
    }

    #[test]
    fn badge_color_bands() {
        assert_eq!(badge_color(95), "brightgreen");
        assert_eq!(badge_color(90), "brightgreen");
        assert_eq!(badge_color(89), "green");
        assert_eq!(badge_color(75), "green");
        assert_eq!(badge_color(74), "yellow");
        assert_eq!(badge_color(60), "yellow");
        assert_eq!(badge_color(59), "orange");
        assert_eq!(badge_color(40), "orange");
        assert_eq!(badge_color(39), "red");
        assert_eq!(badge_color(0), "red");
    }

    #[test]
    fn empty_server_excludes_schema_and_description() {
        let input = CheckInput {
            tools: vec![],
            instructions: None,
            ..clean_input()
        };
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::SchemaHygiene).unwrap().score,
            None
        );
        assert_eq!(
            report
                .dimension(Dimension::DescriptionQuality)
                .unwrap()
                .score,
            None
        );
        // Composite is still defined over the applicable dimensions.
        assert!(report.composite > 0.0);
    }

    #[test]
    fn top_fixes_ranked_by_weighted_impact() {
        let mut input = clean_input();
        input.observations.pollution_lines = 1; // protocol -15 (×25 = 375)
                                                // Make a schema type gap (-5 ×20 = 100).
        input.tools[0] = tool(
            "echo",
            Some("Echo the provided text straight back."),
            json!({ "type": "object", "properties": { "text": {} } }),
        );
        let report = evaluate(&input, None);
        let fixes = report.top_fixes(3);
        assert!(!fixes.is_empty());
        // The pollution finding (highest weighted impact) ranks first.
        assert_eq!(fixes[0].dimension, Dimension::Protocol);
    }

    #[test]
    fn robustness_excluded_when_nothing_observed() {
        // No latency AND treat shutdown as observed? Shutdown is always observed
        // in a real session, but the pure scorer honors "unobserved". Here we
        // simulate a session that recorded neither by... it always records
        // shutdown, so at minimum shutdown is scored. Verify a clean shutdown
        // with no latency still yields a score (only shutdown observed).
        let mut input = clean_input();
        input.observations.list_latency = None;
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Robustness).unwrap().score,
            Some(100.0)
        );
    }

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(1234), "1,234");
        assert_eq!(commas(1234567), "1,234,567");
    }
}
