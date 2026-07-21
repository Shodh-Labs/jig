//! Dimension 2: context cost.

use super::util::*;
use super::*;

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

pub(super) fn score_context(
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
                code: FindingCode::ContextCostHeavySurface,
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
                rank_points: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::testkit::*;
    use serde_json::json;

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
    fn heavy_surface_emits_context_finding_naming_biggest_tool() {
        // One tool with a giant description → well over 8k tokens.
        let big = "lorem ipsum dolor sit amet ".repeat(4000);
        let input = CheckInput {
            tools: vec![
                tool(
                    "giant",
                    Some(big.trim()),
                    json!({ "type": "object", "properties": {} }),
                ),
                tool(
                    "small",
                    Some("a small helper tool here"),
                    json!({ "type": "object", "properties": {} }),
                ),
            ],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        assert!(report.total_tokens > 8_000);
        let c = report.dimension(Dimension::ContextCost).unwrap();
        assert!(c.findings.iter().any(|f| f.fix.contains("`giant`")));
    }
}
