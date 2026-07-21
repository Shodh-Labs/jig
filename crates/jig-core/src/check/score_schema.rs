//! Dimension 3: schema hygiene.

use super::util::*;
use super::*;

/// Schema: relative weight of a tool missing a description. **8 → 10 in
/// `rubric-v1.2`**: a tool with no description at all is uncommon and severe (a
/// model cannot select it), so the class discriminates well and earns weight.
const SCHEMA_MISSING_TOOL_DESC: f64 = 10.0;
/// Schema: relative weight of a parameter missing a description. **Unchanged at
/// 3**: moderately common, moderately serious — the reference point the other
/// three were re-tuned against.
const SCHEMA_PARAM_MISSING_DESC: f64 = 3.0;
/// Schema: relative weight of a parameter missing a type (no enum/`$ref`/etc.).
/// **5 → 8 in `rubric-v1.2`**: an untyped parameter is rare and directly breaks
/// argument generation and validation — the rare-but-serious profile the rate
/// regime should punish hardest.
const SCHEMA_PARAM_MISSING_TYPE: f64 = 8.0;
/// Schema: relative weight of a tool declaring no annotations (`readOnlyHint`, …).
/// **1 → 0.5 in `rubric-v1.2`**: annotations are optional and recently
/// standardized, and servers that omit them omit them on *every* tool, so the
/// class sits at a defect rate of ~1.0 almost everywhere. A class that is
/// near-universally violated separates nobody from anybody; under the rate
/// regime it was nonetheless consuming its **full** weight on almost every
/// server, which is precisely defect 2. It keeps a non-zero weight because the
/// advice is still worth giving.
const SCHEMA_MISSING_ANNOTATIONS: f64 = 0.5;

/// The sum of schema hygiene's class weights — the deduction a server that is
/// 100% defective in *every* class would take before scaling. All four classes
/// are simultaneously attainable, so this is the true worst case.
const SCHEMA_WEIGHT_SUM: f64 = SCHEMA_MISSING_TOOL_DESC
    + SCHEMA_PARAM_MISSING_DESC
    + SCHEMA_PARAM_MISSING_TYPE
    + SCHEMA_MISSING_ANNOTATIONS;

/// Schema hygiene's rate scale: maps a fully-defective server onto
/// [`RATE_SCORE_FLOOR`]. (`rubric-v1.2`: 85 / 21.5 ≈ 3.95; was 85 / 17 = 5.0.)
const SCHEMA_RATE_SCALE: f64 = RATE_DEDUCTION_SPAN / SCHEMA_WEIGHT_SUM;

pub(super) fn score_schema(input: &CheckInput) -> DimensionScore {
    if input.tools.is_empty() {
        return not_applicable(Dimension::SchemaHygiene, "no tools to inspect");
    }

    let n_tools = input.tools.len();
    // Total top-level parameters across every tool — the denominator for the two
    // parameter-level defect classes.
    let n_params: usize = input
        .tools
        .iter()
        .map(|t| param_count(&t.input_schema))
        .sum();

    let mut rates = RateTally::new();
    let mut findings = Vec::new();

    for tool in &input.tools {
        // Missing tool description.
        if tool.description.as_deref().unwrap_or("").trim().is_empty() {
            findings.push(rates.record(
                SCHEMA_CLASS_TOOL_DESC,
                1,
                Finding {
                    dimension: Dimension::SchemaHygiene,
                    code: FindingCode::SchemaHygieneToolMissingDescription,
                    severity: Severity::Medium,
                    message: format!("`{}` has no description", tool.name),
                    fix: format!("add a one-line description to `{}`", tool.name),
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        }

        // Per-parameter checks over the top-level properties (deterministic).
        let (no_desc, no_type) = schema_param_gaps(&tool.input_schema);
        if !no_desc.is_empty() {
            findings.push(rates.record(
                SCHEMA_CLASS_PARAM_DESC,
                no_desc.len(),
                Finding {
                    dimension: Dimension::SchemaHygiene,
                    code: FindingCode::SchemaHygieneParamMissingDescription,
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
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        }
        if !no_type.is_empty() {
            findings.push(rates.record(
                SCHEMA_CLASS_PARAM_TYPE,
                no_type.len(),
                Finding {
                    dimension: Dimension::SchemaHygiene,
                    code: FindingCode::SchemaHygieneParamMissingType,
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
                    points: 0.0,
                    rank_points: None,
                    pinned: false,
                },
            ));
        }
    }

    // Missing annotations, as a single rolled-up finding over all tools.
    let missing_annotations = input.tools.iter().filter(|t| !has_annotations(t)).count();
    if missing_annotations > 0 {
        findings.push(rates.record(
            SCHEMA_CLASS_ANNOTATIONS,
            missing_annotations,
            Finding {
                dimension: Dimension::SchemaHygiene,
                code: FindingCode::SchemaHygieneMissingAnnotations,
                severity: Severity::Low,
                message: format!(
                    "{missing_annotations} tool(s) declare no annotations \
                     (readOnlyHint, destructiveHint, …)"
                ),
                fix: "add tool annotations so clients can reason about side effects".to_string(),
                points: 0.0,
                rank_points: None,
                pinned: false,
            },
        ));
    }

    // Rate-based deduction (rubric-v1.1): each class contributes its relative
    // weight scaled by the fraction of items in that class that are defective,
    // so a large tool surface can no longer saturate the dimension on its own.
    let classes = [
        (SCHEMA_CLASS_TOOL_DESC, SCHEMA_MISSING_TOOL_DESC, n_tools),
        (SCHEMA_CLASS_PARAM_DESC, SCHEMA_PARAM_MISSING_DESC, n_params),
        (SCHEMA_CLASS_PARAM_TYPE, SCHEMA_PARAM_MISSING_TYPE, n_params),
        (
            SCHEMA_CLASS_ANNOTATIONS,
            SCHEMA_MISSING_ANNOTATIONS,
            n_tools,
        ),
    ];
    let score = rates.apply(&classes, SCHEMA_RATE_SCALE, &mut findings);

    let summary = schema_summary(&findings, n_tools);
    DimensionScore {
        dimension: Dimension::SchemaHygiene,
        score: Some(score),
        weight: Dimension::SchemaHygiene.weight(),
        summary,
        heuristic: false,
        findings,
    }
}

/// Schema hygiene defect classes (indices into the tally).
const SCHEMA_CLASS_TOOL_DESC: usize = 0;
const SCHEMA_CLASS_PARAM_DESC: usize = 1;
const SCHEMA_CLASS_PARAM_TYPE: usize = 2;
const SCHEMA_CLASS_ANNOTATIONS: usize = 3;

/// The number of top-level `properties` a tool's input schema declares — the
/// per-tool contribution to the parameter-class denominator.
fn param_count(schema: &Value) -> usize {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(serde_json::Map::len)
        .unwrap_or(0)
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

/// Whether a tool declares any annotations, decided **exactly** from the typed
/// [`Tool::annotations`] field.
///
/// Per MCP `2025-06-18` `annotations` is a sibling of `inputSchema` on the tool
/// object. Jig used to sniff the input schema for an `annotations` key or any
/// `*Hint` key, which was wrong in both directions: a tool with a legitimate
/// argument named `readOnlyHint`, or with JSON-Schema `annotations`, scored as
/// annotated when it declared nothing; and the shape servers actually send was
/// invisible because the typed struct dropped it.
///
/// A bare `"annotations": {}` declares nothing and is not counted — the object
/// is present but makes no claim about the tool.
///
/// [`Tool::annotations`]: crate::protocol::Tool::annotations
fn has_annotations(tool: &Tool) -> bool {
    tool.annotations.as_ref().is_some_and(|a| !a.is_empty())
}

fn schema_summary(findings: &[Finding], n_tools: usize) -> String {
    let clean = format!(
        "{n_tools} tool{} — descriptions, types and params all present",
        plural(n_tools)
    );
    summarize_findings(findings, &clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::testkit::*;
    use serde_json::json;

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
        // One tool, one parameter: `x` has neither a type nor a description and
        // the tool declares no annotations, so three of the four classes are
        // fully defective (the tool itself *is* described). Each denominator is
        // 1, so `rubric-v1.2` shrinkage puts every rate at 1/3 rather than 1 —
        // one broken tool out of one is not yet evidence of a broken server.
        let rate = shrunk_rate(1, 1);
        let expected = 100.0
            - SCHEMA_RATE_SCALE
                * rate
                * (SCHEMA_PARAM_MISSING_TYPE
                    + SCHEMA_PARAM_MISSING_DESC
                    + SCHEMA_MISSING_ANNOTATIONS);
        assert_eq!(s.score, Some(expected));
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
        // The one tool has no description and no annotations — both those
        // classes are 100% defective. The tool declares no parameters at all, so
        // the two parameter classes have an empty denominator and contribute
        // nothing rather than counting as clean.
        let expected = 100.0
            - SCHEMA_RATE_SCALE
                * shrunk_rate(1, 1)
                * (SCHEMA_MISSING_TOOL_DESC + SCHEMA_MISSING_ANNOTATIONS);
        assert_eq!(s.score, Some(expected));
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
        // Only the annotations nit, over a single tool (shrunk to a 1/3 rate).
        assert_eq!(
            s.score,
            Some(100.0 - SCHEMA_RATE_SCALE * shrunk_rate(1, 1) * SCHEMA_MISSING_ANNOTATIONS)
        );
    }

    /// Defect **rate**, not defect count, sets the score — the whole of
    /// `rubric-v1.1`'s defect 1, where the 90-tool server saturated at 0 while
    /// the 3-tool server scored well.
    ///
    /// `rubric-v1.2` layers confidence shrinkage on top, so the scores are no
    /// longer *identical* across sizes — a small surface is deliberately graded
    /// more leniently, because one defect out of three is weak evidence. What
    /// must hold is that the residual size-dependence is monotone, small, and
    /// converging: the same rate scores no worse as the surface grows, and by
    /// the census median tool count the gap is a point or two, not the 85-point
    /// chasm `rubric-v1` produced.
    #[test]
    fn schema_rate_is_essentially_independent_of_tool_surface_size() {
        // Thirds, so every rate is exactly representable at all sizes.
        for numerator in [0usize, 1, 2, 3] {
            let sizes = [3usize, 30, 90, 900];
            let scores: Vec<f64> = sizes
                .into_iter()
                .map(|n| schema_score(n, n * numerator / 3))
                .collect();

            // Monotone: shrinkage only ever *helps* the smaller surface, so the
            // score falls (or holds) as n grows toward the raw rate.
            for w in scores.windows(2) {
                assert!(
                    w[0] >= w[1] - 1e-9,
                    "a {numerator}/3 defect rate must not score better at a larger surface, \
                     got {scores:?}"
                );
            }
            // Converging: by 90 tools the shrinkage is nearly spent, and the
            // 90-vs-900 gap is under a point.
            assert!(
                (scores[2] - scores[3]).abs() < 2.0,
                "shrinkage must be nearly spent by n=90 for rate {numerator}/3, got {scores:?}"
            );
            // Quantified: the leniency a small surface enjoys is exactly the
            // shrinkage formula's own displacement,
            // `SPAN * raw_rate * k / (n + k)`, and nothing more. Asserting the
            // identity rather than a hand-picked bound keeps this test honest
            // if `k` is ever re-tuned.
            let raw_rate = numerator as f64 / 3.0;
            for (i, n) in sizes.iter().enumerate() {
                let limit = 100.0 - RATE_DEDUCTION_SPAN * raw_rate;
                let expected_leniency = RATE_DEDUCTION_SPAN * raw_rate * RATE_SHRINKAGE_K
                    / (*n as f64 + RATE_SHRINKAGE_K);
                assert!(
                    (scores[i] - limit - expected_leniency).abs() < 1e-9,
                    "n={n}, rate {numerator}/3: leniency {} != {expected_leniency}",
                    scores[i] - limit
                );
            }
        }
    }

    /// The documented formula: `score = 100 - 85 * (d + k*prior) / (n + k)`,
    /// clamped to `15..=100`, with `k = 2` and `prior = 0`.
    #[test]
    fn schema_rate_matches_the_documented_formula() {
        for n in [1usize, 3, 30, 90, 900] {
            for numerator in [0usize, 1, 2, 3] {
                let defects = n * numerator / 3;
                // Every class in this fixture shares the same denominator and
                // the same defect count, so the whole dimension reduces to one
                // shrunk rate times the full span.
                let adjusted = defects as f64 / (n as f64 + RATE_SHRINKAGE_K);
                let expected =
                    (100.0 - RATE_DEDUCTION_SPAN * adjusted).clamp(RATE_SCORE_FLOOR, 100.0);
                let got = schema_score(n, defects);
                assert!(
                    (got - expected).abs() < 1e-9,
                    "n={n}, {numerator}/3 defective: expected {expected}, got {got}"
                );
            }
            // A clean server is still pinned at exactly 100 — shrinkage toward a
            // prior of 0 cannot penalise a surface with nothing wrong with it.
            assert!((schema_score(n, 0) - 100.0).abs() < 1e-9);
        }
        // The concrete numbers quoted in the module docs for a 1/3 defect rate.
        for (n, expected) in [(3usize, 83.0), (90, 72.283), (900, 71.729)] {
            assert!((schema_score(n, n / 3) - expected).abs() < 0.01, "n={n}");
        }
    }

    /// **`rubric-v1.2`, defect 3.** A 1-tool server with its single tool broken
    /// is at a raw 100% defect rate, exactly like a 40-tool server with all 40
    /// broken — but one of those is a coin flip and the other is a verdict.
    /// Shrinkage must separate them by a wide margin, while leaving the
    /// large-`n` end where `rubric-v1.1` had it.
    #[test]
    fn small_surfaces_are_shrunk_toward_the_prior() {
        let one = schema_score(1, 1);
        let forty = schema_score(40, 40);

        // Under rubric-v1.1 both of these were exactly RATE_SCORE_FLOOR (15.0).
        assert!(
            one > forty + 40.0,
            "a 1-tool server with 1 defect ({one}) must score materially better than a \
             40-tool server with 40 defects ({forty})"
        );
        assert!(
            (one - 71.667).abs() < 0.01,
            "1/1 -> 85*(1/3) deduction: {one}"
        );
        assert!(
            (forty - 19.048).abs() < 0.01,
            "40/40 -> 85*(40/42): {forty}"
        );

        // Large-n behaviour is unchanged in substance: a fully-defective large
        // surface still lands within a few points of the floor, approaching it
        // from above as the evidence accumulates.
        assert!(forty < RATE_SCORE_FLOOR + 5.0);
        assert!(schema_score(900, 900) < RATE_SCORE_FLOOR + 0.5);
        // And an ordinary rate at an ordinary size barely moves at all.
        for n in [30usize, 90] {
            let v11 = 100.0 - RATE_DEDUCTION_SPAN * (1.0 / 3.0);
            assert!(
                (schema_score(n, n / 3) - v11).abs() < 2.0,
                "n={n} must stay within 2 points of the rubric-v1.1 score"
            );
        }
    }

    /// The floor is 15, not 0 — a server that listed tools has done *something*
    /// right, and 0 stays reserved for genuinely absent structure.
    ///
    /// Under `rubric-v1.2` a fully-defective server *approaches* the floor from
    /// above as its surface grows rather than landing on it exactly: shrinkage
    /// means confidence that a 100% defect rate is real is itself a function of
    /// how many items were observed. The floor is a bound, not an equality.
    #[test]
    fn rate_scored_dimensions_floor_at_15_not_zero() {
        // 90 tools, every one defective in every class: the worst realistic
        // input the schema dimension can receive.
        let report = evaluate(&input_with_tools(schema_rate_tools(90, 90)), None);
        let schema = report.dimension(Dimension::SchemaHygiene).unwrap();
        let score = schema.score.unwrap();
        assert!(
            (RATE_SCORE_FLOOR..RATE_SCORE_FLOOR + 3.0).contains(&score),
            "a fully-defective 90-tool surface must sit just above the floor, got {score}"
        );
        assert!(score > 0.0, "the floor must be strictly above 0");
        // The clamp is real: an enormous surface converges onto the floor and
        // never passes through it.
        assert!(schema_score(100_000, 100_000) >= RATE_SCORE_FLOOR);
        // A server with no tools at all is `None` (excluded), never 0 — that is
        // the "genuinely absent structure" case the floor reserves 0 for.
        let empty = evaluate(&input_with_tools(Vec::new()), None);
        assert_eq!(
            empty.dimension(Dimension::SchemaHygiene).unwrap().score,
            None
        );
    }

    /// Findings are unchanged by the rate rework: still one per defect, still
    /// carrying fix text. Only the arithmetic behind `points` moved.
    #[test]
    fn rate_scoring_preserves_one_finding_per_defect() {
        let report = evaluate(&input_with_tools(schema_rate_tools(30, 10)), None);
        let schema = report.dimension(Dimension::SchemaHygiene).unwrap();
        // 10 missing tool descriptions + 10 param-description findings + 10
        // param-type findings + 1 rolled-up annotations finding.
        assert_eq!(schema.findings.len(), 31);
        assert!(
            schema.findings.iter().all(|f| !f.fix.trim().is_empty()),
            "every finding must still carry fix text"
        );
        // Each finding's points is its share of the deduction it caused, so the
        // parts sum to the whole.
        let summed: f64 = schema.findings.iter().map(|f| f.points).sum();
        assert!(
            (summed - (100.0 - schema.score.unwrap())).abs() < 1e-9,
            "finding points must sum to the dimension deduction"
        );
    }
}
