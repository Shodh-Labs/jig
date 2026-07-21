//! Dimension 4: description quality (heuristic).

use super::util::*;
use super::*;

/// Description: relative weight of a tool name containing whitespace
/// (uncallable). **Unchanged at 15**: vanishingly rare and categorically fatal —
/// the archetype of a class that should dominate when it fires.
const DQ_NAME_HAS_SPACE: f64 = 15.0;
/// Description: relative weight of a tool name breaking the server's dominant
/// naming convention (kebab vs snake). **5 → 4 in `rubric-v1.2`**: cosmetic
/// relative to a description defect, and by construction it can only fire on a
/// minority of a server's tools, so it never dominated — trimmed for consistency
/// with the re-tune rather than to fix an observed problem.
const DQ_NAME_INCONSISTENT: f64 = 4.0;
/// Description: relative weight of a description that is present but too terse
/// for a model to select on (see [`DQ_TERSE_TOKENS`]) or missing entirely.
/// **6 → 8 in `rubric-v1.2`**: this is the class that actually determines
/// whether a model can pick the right tool, and it is far from universal —
/// exactly what the rate regime should weight up.
const DQ_DESC_TERSE: f64 = 8.0;
/// Description: relative weight of a description long enough to waste context
/// (see [`DQ_VERBOSE_TOKENS`]). **4 → 3 in `rubric-v1.2`**: verbosity is already
/// priced directly, and much more precisely, by the context-cost dimension;
/// carrying a heavy second weight here double-charged it.
const DQ_DESC_VERBOSE: f64 = 3.0;
/// Description: relative weight of a tool missing a human-facing `title`.
/// **1 → 0.5 in `rubric-v1.2`**, for the same reason as
/// [`SCHEMA_MISSING_ANNOTATIONS`]: `title` is optional and recently
/// standardized, servers omit it on every tool or none, and a class pinned at a
/// ~1.0 defect rate carries no information about quality while consuming its
/// full share of the dimension.
const DQ_MISSING_TITLE: f64 = 0.5;

/// The sum of description quality's *simultaneously attainable* class weights.
///
/// Unlike schema hygiene, some classes here are mutually exclusive per tool: a
/// name is whitespace-broken **or** convention-inconsistent (the whitespace
/// check short-circuits), and a description is terse **or** verbose, never both.
/// The worst attainable server therefore takes the heavier of each exclusive
/// pair plus the title weight — `rubric-v1.2`: 15 + 8 + 0.5 = 23.5 (was
/// 15 + 6 + 1 = 22) — and scaling by the naive sum of all five would make
/// [`RATE_SCORE_FLOOR`] unreachable.
const DQ_WEIGHT_SUM: f64 = DQ_NAME_HAS_SPACE + DQ_DESC_TERSE + DQ_MISSING_TITLE;

/// Description quality's rate scale: maps a fully-defective server onto
/// [`RATE_SCORE_FLOOR`]. (`rubric-v1.2`: 85 / 23.5 ≈ 3.62; was 85 / 22.)
const DQ_RATE_SCALE: f64 = RATE_DEDUCTION_SPAN / DQ_WEIGHT_SUM;
/// A description at or below this token count is "terse".
const DQ_TERSE_TOKENS: usize = 4;
/// A description at or above this token count is "verbose".
const DQ_VERBOSE_TOKENS: usize = 160;

// -- The context-cost composite cap (rubric-v1.1) ----------------------------

pub(super) fn score_description(input: &CheckInput) -> DimensionScore {
    if input.tools.is_empty() {
        let mut d = not_applicable(Dimension::DescriptionQuality, "no tools to inspect");
        d.heuristic = true;
        return d;
    }

    let n_tools = input.tools.len();
    let mut rates = RateTally::new();
    let mut findings = Vec::new();

    // ---- Naming: spaces (uncallable) and convention consistency ----
    let convention = dominant_convention(&input.tools);
    for tool in &input.tools {
        if tool.name.chars().any(char::is_whitespace) {
            findings.push(rates.record(
                DQ_CLASS_NAME_SPACE,
                1,
                Finding {
                    dimension: Dimension::DescriptionQuality,
                    code: FindingCode::DescriptionQualityNameHasWhitespace,
                    severity: Severity::High,
                    message: format!(
                        "`{}` contains whitespace — models cannot call it",
                        tool.name
                    ),
                    fix: format!(
                        "rename `{}` to a whitespace-free identifier (kebab or snake case)",
                        tool.name
                    ),
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        } else if let Some(dom) = convention {
            if name_convention(&tool.name) == Some(dom.other()) {
                findings.push(rates.record(
                    DQ_CLASS_NAME_INCONSISTENT,
                    1,
                    Finding {
                        dimension: Dimension::DescriptionQuality,
                        code: FindingCode::DescriptionQualityNameConventionInconsistent,
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
                        points: 0.0,
                        rank_points: None,
                        pinned: false,
                    },
                ));
            }
        }
    }

    // ---- Description length bands (token-based, gpt-4o) ----
    for tool in &input.tools {
        let toks = description_tokens(tool);
        if toks <= DQ_TERSE_TOKENS {
            findings.push(rates.record(
                DQ_CLASS_DESC_TERSE,
                1,
                Finding {
                    dimension: Dimension::DescriptionQuality,
                    code: FindingCode::DescriptionQualityDescriptionTerse,
                    severity: Severity::Medium,
                    message: format!(
                        "`{}` description is very terse ({toks} tokens) — models struggle to select it",
                        tool.name
                    ),
                    fix: format!(
                        "expand `{}`'s description to say what it does and when to use it",
                        tool.name
                    ),
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        } else if toks >= DQ_VERBOSE_TOKENS {
            findings.push(rates.record(
                DQ_CLASS_DESC_VERBOSE,
                1,
                Finding {
                    dimension: Dimension::DescriptionQuality,
                    code: FindingCode::DescriptionQualityDescriptionVerbose,
                    severity: Severity::Low,
                    message: format!(
                        "`{}` description is very long ({toks} tokens) — context waste",
                        tool.name
                    ),
                    fix: format!(
                        "tighten `{}`'s description; move detail into params",
                        tool.name
                    ),
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        }
    }

    // ---- Titles (minor) ----
    let missing_titles = input
        .tools
        .iter()
        .filter(|t| t.title.as_deref().unwrap_or("").trim().is_empty())
        .count();
    if missing_titles > 0 {
        findings.push(rates.record(
            DQ_CLASS_TITLE,
            missing_titles,
            Finding {
                dimension: Dimension::DescriptionQuality,
                code: FindingCode::DescriptionQualityMissingTitle,
                severity: Severity::Low,
                message: format!("{missing_titles} tool(s) have no human-facing title"),
                fix: "add a `title` to each tool for nicer client display".to_string(),
                points: 0.0,
                rank_points: None,
                pinned: false,
            },
        ));
    }

    // Rate-based deduction (rubric-v1.1) — every class here is per-tool, so the
    // denominator is the tool count throughout.
    let classes = [
        (DQ_CLASS_NAME_SPACE, DQ_NAME_HAS_SPACE, n_tools),
        (DQ_CLASS_NAME_INCONSISTENT, DQ_NAME_INCONSISTENT, n_tools),
        (DQ_CLASS_DESC_TERSE, DQ_DESC_TERSE, n_tools),
        (DQ_CLASS_DESC_VERBOSE, DQ_DESC_VERBOSE, n_tools),
        (DQ_CLASS_TITLE, DQ_MISSING_TITLE, n_tools),
    ];
    let score = rates.apply(&classes, DQ_RATE_SCALE, &mut findings);

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

/// Description quality defect classes (indices into the tally).
const DQ_CLASS_NAME_SPACE: usize = 0;
const DQ_CLASS_NAME_INCONSISTENT: usize = 1;
const DQ_CLASS_DESC_TERSE: usize = 2;
const DQ_CLASS_DESC_VERBOSE: usize = 3;
const DQ_CLASS_TITLE: usize = 4;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::testkit::*;
    use serde_json::json;

    #[test]
    fn name_with_space_tanks_description_quality() {
        let input = CheckInput {
            tools: vec![tool(
                "bad name",
                Some("a reasonably sized description of the tool"),
                json!({ "type": "object", "properties": {} }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let d = report.dimension(Dimension::DescriptionQuality).unwrap();
        assert!(d.findings.iter().any(|f| f.message.contains("whitespace")));
        assert!(d.heuristic);
        // The single tool has a whitespace name and no title, both fully
        // defective over a denominator of 1 (shrunk to a 1/3 rate); its
        // description is neither terse nor verbose.
        assert_eq!(
            d.score,
            Some(
                100.0 - DQ_RATE_SCALE * shrunk_rate(1, 1) * (DQ_NAME_HAS_SPACE + DQ_MISSING_TITLE)
            )
        );
    }

    #[test]
    fn mixed_naming_convention_flags_the_minority() {
        let input = CheckInput {
            tools: vec![
                tool(
                    "get_user",
                    Some("snake one two three"),
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "get_item",
                    Some("snake one two three"),
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "get-thing",
                    Some("kebab one two three"),
                    json!({ "type": "object", "properties": {} }),
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
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "v",
                    Some(long.trim()),
                    json!({ "type": "object", "properties": {} }),
                ),
            ],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let d = report.dimension(Dimension::DescriptionQuality).unwrap();
        assert!(d.findings.iter().any(|f| f.message.contains("very terse")));
        assert!(d.findings.iter().any(|f| f.message.contains("very long")));
    }

    /// Description quality has the same rate shape and the same floor.
    #[test]
    fn description_quality_is_rate_based_and_floors_at_15() {
        // Every tool maximally defective: whitespace name + terse description +
        // no title. That is the worst simultaneously-attainable class set.
        let tools: Vec<Tool> = (0..40)
            .map(|i| {
                tool(
                    &format!("bad tool {i}"),
                    Some("do"),
                    json!({ "type": "object", "properties": {} }),
                )
            })
            .collect();
        let worst = evaluate(&input_with_tools(tools), None);
        let score = worst
            .dimension(Dimension::DescriptionQuality)
            .unwrap()
            .score
            .unwrap();
        // 40 tools, every class at 40/42 after shrinkage: just above the floor,
        // converging onto it as the surface grows (see
        // `rate_scored_dimensions_floor_at_15_not_zero`).
        assert!(
            (RATE_SCORE_FLOOR..RATE_SCORE_FLOOR + 5.0).contains(&score),
            "a 100%-defective description surface lands just above the floor, got {score}"
        );
    }
}
