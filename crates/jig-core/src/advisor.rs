//! The **tool-set advisor** — deterministic detectors for the failure modes
//! that make a model pick the *wrong* tool, or pay too much to see the right
//! one. House style: no LLM anywhere. Every signal is a mechanical fact about
//! the tool set (names, descriptions, token costs), and every finding carries a
//! concrete fix.
//!
//! # Why a separate analyzer
//!
//! The `jig check` rubric grades one tool at a time (does *this* tool have a
//! description, a type, a title). But three of the most damaging real-world
//! problems are *emergent* — they exist only in the relationship between tools:
//!
//! 1. **Naming collisions / ambiguity.** `get_status` and `fetch_status` are
//!    different strings but the same request; a model cannot reliably choose
//!    between them. Two tools whose descriptions overlap 90% give the model no
//!    basis to pick either.
//! 2. **The accuracy cliff.** Tool-selection accuracy degrades materially once a
//!    server exposes more than a few dozen tools — a property of the *count*, not
//!    any single tool.
//! 3. **Cost dominance.** One fat tool can quietly carry the majority of the
//!    context bill, so the whole surface pays for a definition few calls need.
//!
//! # Purity & determinism
//!
//! [`advise`] is a pure function of the tool list plus the per-tool token costs
//! the caller already computed (`jig check` reuses its context-cost pass; `jig
//! budget` reuses its `ModelBudget`). It does no I/O and no tokenizing of its
//! own. Its output is **stably sorted** (severity, then message), so the same
//! input always yields byte-identical findings in the same order — a hard
//! requirement for the snapshot tests and for CI diffing.
//!
//! # Scoring
//!
//! Advisor findings are **not** scored into the `rubric-v1.1` composite (whether
//! and how to weight tool-set health is a separate decision). They are tagged
//! with the [`Dimension::ToolSet`] category — a sentinel deliberately excluded
//! from [`Dimension::all`] and never given a [`DimensionScore`](crate::check::DimensionScore),
//! so it carries a machine key (`tool_set`) and a ranking weight without ever
//! entering the composite. Their `points` exist only so the shared "Top fixes"
//! ranker can order them next to dimension findings; because no `DimensionScore`
//! holds them, they can never move the grade.

use std::collections::BTreeSet;

use crate::check::{Dimension, Finding, Severity};
use crate::protocol::Tool;

// ---------------------------------------------------------------------------
// Tunable thresholds (documented, so an advisory is never a black box)
// ---------------------------------------------------------------------------

/// Accuracy cliff: more tools than this earns a MEDIUM finding.
const CLIFF_MEDIUM_TOOLS: usize = 30;
/// Accuracy cliff: more tools than this earns a HIGH finding.
const CLIFF_HIGH_TOOLS: usize = 50;

/// Description near-duplication: token-set Jaccard at or above this is flagged.
const DESC_JACCARD_THRESHOLD: f64 = 0.8;

/// Cost dominance: a tool costing more than this multiple of the server's median
/// tool *and* more than [`COST_MIN_TOKENS`] tokens is flagged.
const COST_DOMINANCE_RATIO: f64 = 3.0;
/// Cost dominance: above this multiple the single-tool finding is MEDIUM, not LOW.
const COST_DOMINANCE_MEDIUM_RATIO: f64 = 5.0;
/// Cost dominance: a tool below this absolute token cost is never flagged, however
/// large its ratio — trimming a 40-token tool is not worth a finding.
const COST_MIN_TOKENS: usize = 200;
/// Cost concentration: fraction of total tool tokens carried by the top 3 tools
/// above which a "consider a split" finding fires.
const COST_CONCENTRATION_THRESHOLD: f64 = 0.50;
/// Cost concentration: above this fraction the concentration finding is MEDIUM.
const COST_CONCENTRATION_MEDIUM: f64 = 0.70;
/// Cost concentration: the rule only fires on a server with at least this many
/// tools. On a tiny surface "the top 3 carry the majority" is trivially true and
/// says nothing — this guard is the false-positive mitigation.
const COST_CONCENTRATION_MIN_TOOLS: usize = 6;

/// Collision scan: the maximum number of unordered tool pairs examined. A few
/// hundred tools (~50k pairs) scan fully; beyond that the scan truncates and
/// notes the cap rather than going quadratic without bound.
const COLLISION_PAIR_CAP: usize = 50_000;

/// Ranking points assigned to a HIGH advisor finding (for Top-fixes ordering
/// only — advisor findings never deduct from a dimension score).
const POINTS_HIGH: f64 = 15.0;
/// Ranking points for a MEDIUM advisor finding.
const POINTS_MEDIUM: f64 = 8.0;
/// Ranking points for a LOW advisor finding.
const POINTS_LOW: f64 = 3.0;

/// The synonym groups whose members a model treats as interchangeable. Two tool
/// names that reduce to the same token multiset *after* mapping each token to its
/// group representative are a naming collision.
const SYNONYM_GROUPS: &[&[&str]] = &[
    &["get", "fetch", "retrieve", "read"],
    &["list", "enumerate"],
    &["create", "add", "new"],
    &["delete", "remove"],
    &["update", "edit", "modify"],
    &["search", "find", "query"],
];

/// Generic, non-distinguishing tokens. When one name is a subset of another and
/// the *only* extra tokens are these, the longer name adds no real information —
/// `get_user` vs `get_user_info`.
const GENERIC_TOKENS: &[&str] = &[
    "info",
    "information",
    "data",
    "details",
    "detail",
    "result",
    "results",
    "response",
    "output",
    "value",
    "values",
    "item",
    "items",
    "object",
    "meta",
    "full",
];

/// English stopwords removed before computing description Jaccard overlap, so the
/// score reflects *content* words rather than shared connective tissue.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "for", "and", "or", "in", "on", "at", "by", "with", "from",
    "this", "that", "it", "its", "as", "is", "are", "be", "was", "were", "will", "can", "use",
    "used", "using", "into", "out", "if", "then", "else", "when", "which", "what", "not", "no",
    "yes", "you", "your",
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// The per-tool token cost the cost-dominance analyzer reads. The caller sources
/// these from whatever budget it already computed — `jig check` from its
/// context-cost pass, `jig budget` from its primary [`ModelBudget`](crate::tokens::ModelBudget)
/// — so the advisor never re-tokenizes anything.
#[derive(Debug, Clone)]
pub struct ToolTokenCost {
    /// The tool's name (must match a name in the tool list).
    pub name: String,
    /// The tool's canonical-rendering token count.
    pub tokens: usize,
}

/// Run every tool-set analyzer and return the findings, **stably sorted** by
/// severity then message. Pure and deterministic. An empty or single-tool set
/// never produces a finding.
///
/// `costs` carries per-tool token counts already computed by the caller; a tool
/// with no matching cost entry contributes `0` to the cost analysis (it still
/// participates in the name/description analyzers).
pub fn advise(tools: &[Tool], costs: &[ToolTokenCost]) -> Vec<Finding> {
    let mut findings = Vec::new();

    collision_findings(tools, &mut findings);
    accuracy_cliff_finding(tools.len(), &mut findings);
    cost_dominance_findings(tools, costs, &mut findings);

    sort_findings(&mut findings);
    findings
}

/// Stable, deterministic ordering: most-severe first, ties broken by message.
/// Uses a total ordering so the sort is reproducible byte-for-byte.
fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| a.message.cmp(&b.message))
    });
}

/// Most-severe-first rank for advisor ordering.
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::High => 0,
        Severity::Medium => 1,
        Severity::Low => 2,
        Severity::Info => 3,
    }
}

/// The ranking points a `tool_set` finding of this severity carries.
fn points_for(severity: Severity) -> f64 {
    match severity {
        Severity::High => POINTS_HIGH,
        Severity::Medium => POINTS_MEDIUM,
        Severity::Low => POINTS_LOW,
        Severity::Info => 0.0,
    }
}

/// Build a `tool_set`-category [`Finding`].
fn finding(severity: Severity, message: String, fix: String) -> Finding {
    Finding {
        dimension: Dimension::ToolSet,
        severity,
        message,
        fix,
        points: points_for(severity),
        // Advisor findings are advisory by definition — never pinned.
        pinned: false,
    }
}

// ---------------------------------------------------------------------------
// Analyzer 1: naming collision / ambiguity
// ---------------------------------------------------------------------------

fn collision_findings(tools: &[Tool], out: &mut Vec<Finding>) {
    // Precompute each tool's normalized token list and canonical (synonym-mapped)
    // multiset once, so the O(n²) pairwise pass stays cheap per pair.
    let normalized: Vec<NameTokens> = tools.iter().map(|t| NameTokens::new(&t.name)).collect();
    let descs: Vec<BTreeSet<String>> = tools
        .iter()
        .map(|t| description_tokens(t.description.as_deref()))
        .collect();

    let n = tools.len();
    let mut pairs_examined = 0usize;
    let mut capped = false;

    'outer: for i in 0..n {
        for j in (i + 1)..n {
            if pairs_examined >= COLLISION_PAIR_CAP {
                capped = true;
                break 'outer;
            }
            pairs_examined += 1;

            let (a, b) = (&normalized[i], &normalized[j]);

            // (a) same canonical token multiset modulo synonyms → HIGH.
            if !a.canonical.is_empty() && a.canonical == b.canonical {
                out.push(finding(
                    Severity::High,
                    format!(
                        "`{}` vs `{}`: models cannot reliably distinguish these — same action, \
                         interchangeable words",
                        tools[i].name, tools[j].name
                    ),
                    format!(
                        "merge `{}` and `{}` into one tool, or rename one with a token that names \
                         the real difference (scope, resource, side effect)",
                        tools[i].name, tools[j].name
                    ),
                ));
                // A pure collision subsumes the weaker subset/overlap signals.
                continue;
            }

            // (b) one name is a token-subset of the other, extras all generic → MEDIUM.
            if let Some((short, long)) = generic_subset_pair(i, j, &normalized, tools) {
                out.push(finding(
                    Severity::Medium,
                    format!(
                        "`{short}` vs `{long}`: names differ only by generic word(s) — the model \
                         has little basis to choose"
                    ),
                    format!(
                        "rename `{long}` to state what it adds over `{short}` (or drop one if they \
                         truly return the same thing)"
                    ),
                ));
            }

            // (c) description near-duplication → MEDIUM.
            if let Some(pct) = jaccard_overlap(&descs[i], &descs[j]) {
                if pct >= DESC_JACCARD_THRESHOLD {
                    let pct_round = (pct * 100.0).round() as u32;
                    out.push(finding(
                        Severity::Medium,
                        format!(
                            "`{}` and `{}` descriptions overlap {pct_round}% — the model has no \
                             basis to choose",
                            tools[i].name, tools[j].name
                        ),
                        format!(
                            "sharpen one description to name the distinguishing case — when to \
                             reach for `{}` instead of `{}`",
                            tools[i].name, tools[j].name
                        ),
                    ));
                }
            }
        }
    }

    if capped {
        out.push(finding(
            Severity::Info,
            format!(
                "{n} tools — collision scan capped at {COLLISION_PAIR_CAP} pairs; some pairs were \
                 not compared"
            ),
            "split this surface into focused servers; a set this large has deeper problems than \
             any single collision"
                .to_string(),
        ));
    }
}

/// Decide whether tools `i` and `j` form a generic-subset pair, returning
/// `(shorter_name, longer_name)` when they do. Requires a *strict* multiset
/// subset (equal multisets are handled by the stronger collision rule) whose
/// extra tokens are all generic.
fn generic_subset_pair<'a>(
    i: usize,
    j: usize,
    normalized: &[NameTokens],
    tools: &'a [Tool],
) -> Option<(&'a str, &'a str)> {
    let a = &normalized[i];
    let b = &normalized[j];
    if a.tokens.is_empty() || b.tokens.is_empty() {
        return None;
    }
    // Compare on the raw token multisets (not synonym-canonical): a subset that
    // only adds generic words is the signal, and synonym-swapping is the other
    // rule's job.
    if let Some(extra) = strict_subset_extra(&a.tokens, &b.tokens) {
        if extra.iter().all(|t| GENERIC_TOKENS.contains(&t.as_str())) {
            return Some((&tools[i].name, &tools[j].name));
        }
    }
    if let Some(extra) = strict_subset_extra(&b.tokens, &a.tokens) {
        if extra.iter().all(|t| GENERIC_TOKENS.contains(&t.as_str())) {
            return Some((&tools[j].name, &tools[i].name));
        }
    }
    None
}

/// If multiset `small` is a strict subset of multiset `large`, return the extra
/// tokens (`large - small`); otherwise `None`. Both inputs are token vectors.
fn strict_subset_extra(small: &[String], large: &[String]) -> Option<Vec<String>> {
    if small.len() >= large.len() {
        return None;
    }
    let mut remaining = large.to_vec();
    for tok in small {
        let pos = remaining.iter().position(|t| t == tok)?;
        remaining.swap_remove(pos);
    }
    Some(remaining)
}

/// A tool name reduced to tokens and to its synonym-canonical multiset.
struct NameTokens {
    /// Lowercased tokens in first-seen order (a multiset, order not significant).
    tokens: Vec<String>,
    /// Sorted, synonym-mapped tokens — the multiset two names must share to be a
    /// collision. Empty when the name has no alphanumeric tokens.
    canonical: Vec<String>,
}

impl NameTokens {
    fn new(name: &str) -> NameTokens {
        let tokens = tokenize_name(name);
        let mut canonical: Vec<String> = tokens.iter().map(|t| canonical_synonym(t)).collect();
        canonical.sort();
        NameTokens { tokens, canonical }
    }
}

/// Split a tool name into lowercased tokens, breaking on separators
/// (`-`, `_`, whitespace, other punctuation) *and* camelCase boundaries.
/// `getUserByID` → `[get, user, by, id]`; `list-http_Servers` → `[list, http, servers]`.
fn tokenize_name(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (idx, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            // Separator: flush.
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        // camelCase boundary: a lower/digit followed by an upper, or an upper
        // that begins a new word after a run of uppers (e.g. the `P` before
        // `arser` in `XMLParser`).
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[idx - 1];
            let next_lower = chars
                .get(idx + 1)
                .map(|n| n.is_lowercase())
                .unwrap_or(false);
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Map a token to its synonym group's representative (the group's first member),
/// or return the token unchanged when it is in no group.
fn canonical_synonym(token: &str) -> String {
    for group in SYNONYM_GROUPS {
        if group.contains(&token) {
            return group[0].to_string();
        }
    }
    token.to_string()
}

/// The distinct, stopword-free, lowercased alphanumeric tokens of a description.
fn description_tokens(desc: Option<&str>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Some(desc) = desc else {
        return set;
    };
    for raw in desc.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        set.insert(lower);
    }
    set
}

/// Jaccard overlap of two description token sets: `|A ∩ B| / |A ∪ B|`. Returns
/// `None` when either set is empty (an empty description gives the model nothing
/// to compare, so it is not a "near-duplicate" signal).
fn jaccard_overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Option<f64> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return None;
    }
    Some(inter as f64 / union as f64)
}

// ---------------------------------------------------------------------------
// Analyzer 2: accuracy cliff (tool count)
// ---------------------------------------------------------------------------

fn accuracy_cliff_finding(tool_count: usize, out: &mut Vec<Finding>) {
    let severity = if tool_count > CLIFF_HIGH_TOOLS {
        Severity::High
    } else if tool_count > CLIFF_MEDIUM_TOOLS {
        Severity::Medium
    } else {
        return;
    };
    let threshold = if severity == Severity::High {
        CLIFF_HIGH_TOOLS
    } else {
        CLIFF_MEDIUM_TOOLS
    };
    out.push(finding(
        severity,
        format!(
            "{tool_count} tools exposed — past ~{threshold} a model's tool-selection accuracy \
             degrades materially"
        ),
        "published measurements show selection accuracy falling off past a few dozen tools; split \
         into focused servers or defer rarely-used tools. Server-side tool search (the MCP RC's \
         Tool-Search direction) is the structural fix."
            .to_string(),
    ));
}

// ---------------------------------------------------------------------------
// Analyzer 3: cost dominance
// ---------------------------------------------------------------------------

fn cost_dominance_findings(tools: &[Tool], costs: &[ToolTokenCost], out: &mut Vec<Finding>) {
    if tools.len() < 2 {
        return;
    }
    // Pair each tool with its cost (0 when the caller supplied none), preserving
    // tool order for determinism.
    let mut per_tool: Vec<(&str, usize)> = Vec::with_capacity(tools.len());
    for t in tools {
        let tokens = costs
            .iter()
            .find(|c| c.name == t.name)
            .map(|c| c.tokens)
            .unwrap_or(0);
        per_tool.push((t.name.as_str(), tokens));
    }

    let total: usize = per_tool.iter().map(|(_, t)| *t).sum();
    if total == 0 {
        return;
    }

    // ---- Rule A: single dominant tool (> ratio × median AND > floor) ----
    let median = median_tokens(&per_tool);
    if median > 0.0 {
        // Deterministic order: descending tokens, ties by name — the biggest
        // offenders lead, and equal-cost tools sort reproducibly.
        let mut ranked = per_tool.clone();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (name, tokens) in ranked {
            let ratio = tokens as f64 / median;
            if tokens > COST_MIN_TOKENS && ratio > COST_DOMINANCE_RATIO {
                let severity = if ratio >= COST_DOMINANCE_MEDIUM_RATIO {
                    Severity::Medium
                } else {
                    Severity::Low
                };
                out.push(finding(
                    severity,
                    format!(
                        "`{name}` costs {tokens} tok — {ratio:.1}x your median tool; it dominates \
                         the surface's context bill"
                    ),
                    format!(
                        "trim `{name}`'s description or flatten its input schema; a single tool \
                         this heavy is where the cheapest tokens are"
                    ),
                ));
            }
        }
    }

    // ---- Rule B: top-3 concentration ----
    if per_tool.len() >= COST_CONCENTRATION_MIN_TOOLS {
        let mut sorted: Vec<usize> = per_tool.iter().map(|(_, t)| *t).collect();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let top3: usize = sorted.iter().take(3).sum();
        let share = top3 as f64 / total as f64;
        if share > COST_CONCENTRATION_THRESHOLD {
            let pct = (share * 100.0).round() as u32;
            let severity = if share > COST_CONCENTRATION_MEDIUM {
                Severity::Medium
            } else {
                Severity::Low
            };
            out.push(finding(
                severity,
                format!(
                    "3 of your {} tools carry {pct}% of the tool-surface token cost",
                    per_tool.len()
                ),
                "consider splitting the heavy few into their own server (or trimming them) so the \
                 whole surface stops paying for definitions most calls never use"
                    .to_string(),
            ));
        }
    }
}

/// The median of a list of per-tool token counts, as `f64`. Even-length lists
/// average the two middle values. An empty list yields `0.0`.
fn median_tokens(per_tool: &[(&str, usize)]) -> f64 {
    if per_tool.is_empty() {
        return 0.0;
    }
    let mut v: Vec<usize> = per_tool.iter().map(|(_, t)| *t).collect();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid] as f64
    } else {
        (v[mid - 1] as f64 + v[mid] as f64) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn tool(name: &str, desc: Option<&str>) -> Tool {
        let mut m = serde_json::Map::new();
        m.insert("name".to_string(), json!(name));
        if let Some(d) = desc {
            m.insert("description".to_string(), json!(d));
        }
        m.insert(
            "inputSchema".to_string(),
            json!({ "type": "object", "properties": {} }),
        );
        serde_json::from_value::<Tool>(Value::Object(m)).unwrap()
    }

    fn cost(name: &str, tokens: usize) -> ToolTokenCost {
        ToolTokenCost {
            name: name.to_string(),
            tokens,
        }
    }

    fn messages(findings: &[Finding]) -> Vec<String> {
        findings.iter().map(|f| f.message.clone()).collect()
    }

    // ---- tokenizer -------------------------------------------------------

    #[test]
    fn tokenize_splits_separators_and_camel() {
        assert_eq!(tokenize_name("get_status"), vec!["get", "status"]);
        assert_eq!(tokenize_name("get-status"), vec!["get", "status"]);
        assert_eq!(tokenize_name("getStatus"), vec!["get", "status"]);
        assert_eq!(
            tokenize_name("getUserByID"),
            vec!["get", "user", "by", "id"]
        );
        assert_eq!(tokenize_name("XMLParser"), vec!["xml", "parser"]);
        assert_eq!(
            tokenize_name("browser_take_screenshot"),
            vec!["browser", "take", "screenshot"]
        );
        assert!(tokenize_name("").is_empty());
    }

    // ---- 1a synonym collisions -------------------------------------------

    #[test]
    fn synonym_collision_is_high() {
        let tools = vec![tool("get_status", None), tool("fetch_status", None)];
        let f = advise(&tools, &[]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].dimension, Dimension::ToolSet);
        assert!(f[0].message.contains("get_status") && f[0].message.contains("fetch_status"));
    }

    #[test]
    fn synonym_collision_across_conventions() {
        // kebab vs snake vs camel, same canonical action.
        let tools = vec![tool("list-users", None), tool("enumerateUsers", None)];
        let f = advise(&tools, &[]);
        assert!(f.iter().any(|x| x.severity == Severity::High));
    }

    #[test]
    fn reordered_tokens_are_a_collision() {
        let tools = vec![tool("user_get", None), tool("get_user", None)];
        let f = advise(&tools, &[]);
        assert!(f.iter().any(|x| x.severity == Severity::High));
    }

    #[test]
    fn distinct_actions_do_not_collide() {
        let tools = vec![tool("get_user", None), tool("delete_user", None)];
        let f = advise(&tools, &[]);
        assert!(
            f.is_empty(),
            "distinct verbs must not collide: {:?}",
            messages(&f)
        );
    }

    // ---- 1b generic-subset names -----------------------------------------

    #[test]
    fn generic_subset_is_medium() {
        let tools = vec![tool("get_user", None), tool("get_user_info", None)];
        let f = advise(&tools, &[]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].message.contains("generic"));
    }

    #[test]
    fn non_generic_extra_token_is_not_a_subset_finding() {
        // `admin` is a real distinguisher, not a generic filler.
        let tools = vec![tool("get_user", None), tool("get_user_admin", None)];
        let f = advise(&tools, &[]);
        assert!(
            f.is_empty(),
            "meaningful extra token must not flag: {:?}",
            messages(&f)
        );
    }

    // ---- 1c description overlap ------------------------------------------

    #[test]
    fn description_overlap_boundary() {
        // Build two descriptions whose stopword-free token sets have a known
        // Jaccard. Shared 8 words, each has 1 unique → 8/10 = 0.80 (>= 0.8 fires).
        let shared = "alpha bravo charlie delta echo foxtrot golf hotel";
        let a = format!("{shared} unique1");
        let b = format!("{shared} unique2");
        let tools = vec![tool("one", Some(&a)), tool("two", Some(&b))];
        let f = advise(&tools, &[]);
        assert!(
            f.iter()
                .any(|x| x.message.contains("overlap") && x.message.contains("80%")),
            "0.80 Jaccard must fire: {:?}",
            messages(&f)
        );

        // 7 shared, 3 unique each → 7/13 ≈ 0.538 < 0.8, must NOT fire.
        let shared7 = "alpha bravo charlie delta echo foxtrot golf";
        let a2 = format!("{shared7} u1 u2 u3");
        let b2 = format!("{shared7} v1 v2 v3");
        let tools2 = vec![tool("one", Some(&a2)), tool("two", Some(&b2))];
        let f2 = advise(&tools2, &[]);
        assert!(
            !f2.iter().any(|x| x.message.contains("overlap")),
            "0.54 Jaccard must not fire: {:?}",
            messages(&f2)
        );
    }

    #[test]
    fn jaccard_just_below_threshold_does_not_fire() {
        // 8 shared, 1 unique in A, 2 unique in B → 8/11 ≈ 0.727 < 0.8.
        let shared = "alpha bravo charlie delta echo foxtrot golf hotel";
        let a = format!("{shared} u1");
        let b = format!("{shared} v1 v2");
        let tools = vec![tool("one", Some(&a)), tool("two", Some(&b))];
        let f = advise(&tools, &[]);
        assert!(!f.iter().any(|x| x.message.contains("overlap")));
    }

    #[test]
    fn jaccard_math() {
        let a: BTreeSet<String> = ["x", "y", "z"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["x", "y", "w"].iter().map(|s| s.to_string()).collect();
        // inter 2, union 4 → 0.5
        assert_eq!(jaccard_overlap(&a, &b), Some(0.5));
        let empty = BTreeSet::new();
        assert_eq!(jaccard_overlap(&a, &empty), None);
    }

    // ---- 2 accuracy cliff ------------------------------------------------

    fn n_tools(n: usize) -> Vec<Tool> {
        (0..n)
            .map(|i| tool(&format!("tool_{i}"), Some("does a distinct thing number")))
            .collect()
    }

    #[test]
    fn cliff_thresholds_30_31_50_51() {
        assert!(!advise(&n_tools(30), &[]).iter().any(is_cliff));
        let f31 = advise(&n_tools(31), &[]);
        assert!(f31
            .iter()
            .any(|x| is_cliff(x) && x.severity == Severity::Medium));
        let f50 = advise(&n_tools(50), &[]);
        assert!(f50
            .iter()
            .any(|x| is_cliff(x) && x.severity == Severity::Medium));
        let f51 = advise(&n_tools(51), &[]);
        assert!(f51
            .iter()
            .any(|x| is_cliff(x) && x.severity == Severity::High));
    }

    fn is_cliff(f: &Finding) -> bool {
        f.message.contains("tools exposed")
    }

    // ---- 3 cost dominance ------------------------------------------------

    #[test]
    fn median_even_and_odd() {
        assert_eq!(median_tokens(&[("a", 10), ("b", 20), ("c", 30)]), 20.0);
        assert_eq!(median_tokens(&[("a", 10), ("b", 20)]), 15.0);
        assert_eq!(median_tokens(&[]), 0.0);
    }

    #[test]
    fn dominant_tool_flagged() {
        let tools = vec![
            tool("a", None),
            tool("b", None),
            tool("big", None),
            tool("c", None),
        ];
        let costs = vec![
            cost("a", 50),
            cost("b", 50),
            cost("big", 300),
            cost("c", 50),
        ];
        let f = advise(&tools, &costs);
        // median 50, big=300 → 6.0x > 5.0 → MEDIUM.
        let dom = f
            .iter()
            .find(|x| x.message.contains("`big` costs"))
            .expect("dominant finding");
        assert_eq!(dom.severity, Severity::Medium);
        assert!(dom.message.contains("6.0x"));
    }

    #[test]
    fn small_absolute_cost_never_flagged() {
        // 10x the median but only 100 tokens (< 200 floor).
        let tools = vec![tool("a", None), tool("b", None), tool("big", None)];
        let costs = vec![cost("a", 10), cost("b", 10), cost("big", 100)];
        let f = advise(&tools, &costs);
        assert!(!f.iter().any(|x| x.message.contains("dominates")));
    }

    #[test]
    fn top3_concentration_flagged() {
        let names: Vec<Tool> = (0..8).map(|i| tool(&format!("t{i}"), None)).collect();
        let mut costs: Vec<ToolTokenCost> = Vec::new();
        // Three heavy (300 each = 900), five light (20 each = 100) → total 1000, top3=90%.
        for i in 0..3 {
            costs.push(cost(&format!("t{i}"), 300));
        }
        for i in 3..8 {
            costs.push(cost(&format!("t{i}"), 20));
        }
        let f = advise(&names, &costs);
        let conc = f
            .iter()
            .find(|x| x.message.contains("carry"))
            .expect("concentration finding");
        assert_eq!(conc.severity, Severity::Medium); // 90% > 70%
        assert!(conc.message.contains("90%"));
    }

    #[test]
    fn concentration_not_fired_on_tiny_surface() {
        // 3 tools: top3 is trivially 100%, but below the min-tools guard.
        let tools = vec![tool("a", None), tool("b", None), tool("c", None)];
        let costs = vec![cost("a", 300), cost("b", 300), cost("c", 300)];
        let f = advise(&tools, &costs);
        assert!(!f.iter().any(|x| x.message.contains("carry")));
    }

    // ---- edge cases & determinism ----------------------------------------

    #[test]
    fn empty_and_single_tool_never_fire() {
        assert!(advise(&[], &[]).is_empty());
        assert!(advise(
            &[tool("solo", Some("a lonely tool that does one thing"))],
            &[]
        )
        .is_empty());
    }

    #[test]
    fn output_is_deterministic_and_sorted() {
        let tools = vec![
            tool("get_status", None),
            tool("fetch_status", None),
            tool("get_user", None),
            tool("get_user_info", None),
        ];
        let a = advise(&tools, &[]);
        let b = advise(&tools, &[]);
        assert_eq!(messages(&a), messages(&b));
        // Sorted by severity: HIGH (status collision) before MEDIUM (subset).
        assert_eq!(a[0].severity, Severity::High);
        assert!(a
            .windows(2)
            .all(|w| severity_rank(w[0].severity) <= severity_rank(w[1].severity)));
    }
}
