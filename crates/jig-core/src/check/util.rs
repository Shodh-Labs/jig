//! Small helpers shared across the scoring dimensions: the shared context
//! tokenizer, rate-scoring math, score clamping, and text formatting.

use std::sync::LazyLock;

use crate::tokens::ModelCounter;

use super::*;

/// The context-metric tokenizer, built once per process. Constructing a
/// tiktoken BPE is expensive, so [`evaluate`] — which may be called many times
/// (e.g. in a property test) — shares this single counter rather than rebuilding
/// it per tool or per call. `None` only if the tokenizer failed to build, in
/// which case token counts degrade to `0` rather than panicking.
static CONTEXT_COUNTER: LazyLock<Option<ModelCounter>> =
    LazyLock::new(|| ModelCounter::new(CONTEXT_METRIC_MODEL).ok());

/// The shared context-metric counter, if it built successfully.
pub(super) fn context_counter() -> Option<&'static ModelCounter> {
    CONTEXT_COUNTER.as_ref()
}

/// The model whose exact tokenizer defines the context-cost metric.
const CONTEXT_METRIC_MODEL: &str = "gpt-4o";

/// The full deduction span of a rate-scored dimension: 100% defective in every
/// class deducts exactly this much, landing the score on [`RATE_SCORE_FLOOR`].
pub(super) const RATE_DEDUCTION_SPAN: f64 = 100.0 - RATE_SCORE_FLOOR;

/// Clamp a running score into `0..=100`.
pub(super) fn clamp_score(s: f64) -> f64 {
    s.clamp(0.0, 100.0)
}

/// The shrinkage strength `k` — the number of *pseudo-items* the neutral prior
/// contributes to every class denominator (`rubric-v1.2`, defect 3).
///
/// A raw defect rate is a point estimate whose variance explodes as the
/// denominator shrinks: a 1-tool server with one flaw sits at a 100% defect rate
/// and consumes a whole class weight, while a 40-tool server needs 40 flaws for
/// the same. That is not a quality difference, it is a sample-size artefact, and
/// it matters — **5 of the 29 servers in the census expose exactly one tool, and
/// 11 of 29 expose five or fewer** (`data/percentiles.json`, `tool_count`).
///
/// `k = 2` is chosen against that distribution so the prior is decisive only
/// where the evidence genuinely is thin, and negligible where it is not:
///
/// | Tools `n` | Census position | Prior weight `k/(n+k)` |
/// | --- | --- | --- |
/// | 1 | p17 | 67% |
/// | 2 | — | 50% |
/// | 5 | p38 | 29% |
/// | 14 | median | 13% |
/// | 26 | p76 | 7% |
/// | 89 | p100 | 2% |
///
/// By the census median the prior moves a score by roughly a point; at the top
/// of the distribution it is nearly invisible. Deliberately small: this corrects
/// for uncertainty, it does not forgive defects.
pub(super) const RATE_SHRINKAGE_K: f64 = 2.0;

/// The neutral prior defect rate a class is shrunk *toward*.
///
/// **0.0, and that is a documented limitation, not a considered choice.** The
/// principled prior is the census median defect rate for the class, which would
/// pull a thin observation toward what the ecosystem typically does. It is not
/// derivable here: `data/census-raw.json` records `toolCount`,
/// `contextCostTokens`, `capabilities`, `stdoutPollutionLines` and friends, but
/// **no per-class schema or description defect counts at all** — the census
/// never captured the fields these dimensions grade. Until the census is
/// extended to carry them, 0 is the only prior that invents nothing.
///
/// The direction of the resulting bias is stated plainly: shrinking toward 0
/// means a thin surface is treated as *probably clean*, so small servers are
/// scored generously rather than harshly. That is the right way to be wrong when
/// the evidence is thin and the grade is public, but it is a thumb on the scale
/// and should be replaced with a measured prior as soon as one exists.
const RATE_SHRINKAGE_PRIOR: f64 = 0.0;

/// A class's **empirical-Bayes shrunk defect rate** (`rubric-v1.2`, defect 3):
///
/// ```text
/// adjusted_rate = (defects + k * prior) / (n + k)
/// ```
///
/// with `k` = [`RATE_SHRINKAGE_K`] and `prior` = [`RATE_SHRINKAGE_PRIOR`]. As
/// `n` grows the prior washes out and this converges on the raw rate `d/n`, so
/// large-surface grading is materially unchanged; as `n` shrinks the estimate is
/// pulled toward the prior, so a single defect on a one-tool server no longer
/// reads as a total, confident failure.
///
/// One deliberate consequence: a 100%-defective server no longer lands *exactly*
/// on [`RATE_SCORE_FLOOR`] at finite `n`, approaching it from above as the
/// surface grows (40 tools → 19.0, 900 → 15.2). That is the shrinkage working as
/// intended — certainty that a defect rate really is 100% is itself a function
/// of how many items were observed — and it is why the floor is a `clamp` bound
/// rather than an asserted equality.
pub(super) fn shrunk_rate(defects: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    let adjusted = (defects as f64 + RATE_SHRINKAGE_K * RATE_SHRINKAGE_PRIOR)
        / (denominator as f64 + RATE_SHRINKAGE_K);
    // A class can never exceed a 100% defect rate, but clamp defensively so a
    // miscounted denominator cannot inflate the deduction.
    adjusted.clamp(0.0, 1.0)
}

/// Accumulator for a [rate-scored dimension](crate::check#rate-based-dimensions-rubric-v11-re-tuned-in-rubric-v12).
///
/// Findings are emitted during the per-tool walk, before the defect *rates* are
/// known, so each is registered here against its defect class along with how
/// many defective items it covers. [`apply`](RateTally::apply) then computes the
/// dimension score from the class rates and back-fills each finding's `points`
/// with its exact share of the deduction it caused.
#[derive(Default)]
pub(super) struct RateTally {
    /// `(class index, defective items covered)` per finding, in emission order.
    entries: Vec<(usize, usize)>,
}

impl RateTally {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Register `finding` as covering `items` defective items of class `class`,
    /// returning it unchanged so the caller can push it. Findings must be pushed
    /// in the order they are recorded — [`apply`](RateTally::apply) pairs them
    /// positionally and asserts the lengths agree.
    pub(super) fn record(&mut self, class: usize, items: usize, finding: Finding) -> Finding {
        self.entries.push((class, items));
        finding
    }

    /// Score the dimension from the recorded defect rates and back-fill each
    /// finding's `points`.
    ///
    /// `classes` gives, per class, its `(index, relative weight, denominator)`.
    /// A class whose denominator is 0 has no items to be defective and
    /// contributes nothing. The returned score is clamped to
    /// `RATE_SCORE_FLOOR..=100`.
    pub(super) fn apply(
        &self,
        classes: &[(usize, f64, usize)],
        scale: f64,
        findings: &mut [Finding],
    ) -> f64 {
        debug_assert_eq!(
            self.entries.len(),
            findings.len(),
            "every finding of a rate-scored dimension must be registered with RateTally::record"
        );

        // Defective item count per class, indexed by class index.
        let n_classes = classes.len();
        let mut defective = vec![0usize; n_classes];
        for (class, items) in &self.entries {
            if let Some(slot) = defective.get_mut(*class) {
                *slot += items;
            }
        }

        // Per-class deduction: relative weight × defect rate × scale.
        let mut deduction_per_class = vec![0.0f64; n_classes];
        let mut total = 0.0;
        for (class, weight, denominator) in classes {
            if *denominator == 0 || defective[*class] == 0 {
                continue;
            }
            let rate = shrunk_rate(defective[*class], *denominator);
            let d = scale * weight * rate;
            deduction_per_class[*class] = d;
            total += d;
        }

        // Back-fill points: each finding takes its pro-rata share of the class
        // deduction it contributed to, so "Top fixes" ranks by true composite
        // impact rather than by a raw per-item penalty the score never applied.
        for (finding, (class, items)) in findings.iter_mut().zip(&self.entries) {
            let class_defective = defective.get(*class).copied().unwrap_or(0);
            finding.points = if class_defective == 0 {
                0.0
            } else {
                deduction_per_class[*class] * *items as f64 / class_defective as f64
            };
        }

        (100.0 - total).clamp(RATE_SCORE_FLOOR, 100.0)
    }
}
/// A dimension excluded from the composite (not applicable to this server).
pub(super) fn not_applicable(dimension: Dimension, why: &str) -> DimensionScore {
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
pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Join names as backtick-quoted, comma-separated: `` `a`, `b` ``.
pub(super) fn quote_join(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Insert thousands separators: `12345` -> `12,345`.
pub(super) fn commas(n: usize) -> String {
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

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(1234), "1,234");
        assert_eq!(commas(1234567), "1,234,567");
    }
}
