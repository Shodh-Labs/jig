//! Dimension 5: robustness (observed only).

use super::util::*;
use super::*;

/// Robustness: sub-score when the server exited non-zero on a failed start
/// *without* naming the environment variable it needed (`rubric-v1.3`, SOP 26).
/// Failing fast is the right instinct and is not punished; failing mutely is
/// what costs, because the user is left to guess.
pub(crate) const ROBUST_CRED_UNNAMED_SCORE: f64 = 60.0;
/// Robustness: sub-score when the server **hung** instead of exiting on a
/// missing credential (`rubric-v1.3`, SOP 26). Scored at 0: a hang is strictly
/// worse than a crash. The client has no signal at all, the user waits out a
/// timeout, and 2 of the 29 census servers do exactly this.
pub(crate) const ROBUST_CRED_HANG_SCORE: f64 = 0.0;
/// Robustness: sub-score when the server exited **zero** after failing to
/// start (`rubric-v1.3`, SOP 26). Also 0, and for a sharper reason than the
/// hang: a zero exit is an affirmative lie. A supervisor reads it as success and
/// will not restart; a client cannot distinguish it from a clean shutdown.
pub(crate) const ROBUST_CRED_EXIT_ZERO_SCORE: f64 = 0.0;

/// Robustness timing anchors `(milliseconds, sub-score)`, ascending by time —
/// the ramp both the boot and the list-latency sub-scores interpolate over
/// ([`timing_subscore`]).
///
/// # Why a ramp and not the `rubric-v1.3` buckets (`rubric-v1.4`)
///
/// `rubric-v1.3` scored these as a three-step function: `<= 1s` → 100,
/// `<= 3s` → 70, else 40. A 50-server fleet run showed why that fails. Every
/// `npx` server landed in the same bucket on boot and the same bucket on
/// latency, so robustness came out **exactly 80 for all 26 graded servers** —
/// a constant, not a measurement. Two servers differing by 6 seconds of boot and
/// two differing by 1 millisecond scored identically, while 2.999s and 3.001s
/// differed by 30 points.
///
/// That is the same shape `rubric-v1.2` spent a release deleting from the
/// context cap, for the same reason, and the fix is the same: a continuous,
/// monotone non-increasing ramp. The anchors are chosen to *pass through* the
/// `rubric-v1.3` bucket edges — 1s → 100, 3s → 70 — so the release changes the
/// resolution of the dimension without moving the judgement it encodes, and no
/// server's score moves by more than the bucket it was already rounded into.
///
/// The tail below 3s is new: `rubric-v1.3` floored at 40 the moment a server
/// crossed 3s, which meant a 3.1s boot and a 60s boot were indistinguishable.
const ROBUST_TIMING_ANCHORS: &[(f64, f64)] = &[
    // Instant. Nothing a user can perceive.
    (0.0, 100.0),
    // The `rubric-v1.3` "fast" edge, preserved exactly.
    (1_000.0, 100.0),
    // The `rubric-v1.3` "sluggish" edge, preserved exactly.
    (3_000.0, 70.0),
    // Beyond the old cliff: a slow server is now distinguishable from a
    // catastrophic one.
    (10_000.0, 40.0),
    (30_000.0, 15.0),
];

/// Floor for a timing sub-score. Matches the dimension floor of 15 that
/// `rubric-v1.1` introduced and its reasoning: a server that answered at all has
/// demonstrated *some* structure, and 0 is reserved for genuinely absent
/// behaviour.
const ROBUST_TIMING_FLOOR: f64 = 15.0;

/// Robustness sub-score for an unclean shutdown.
const ROBUST_UNCLEAN_SHUTDOWN_SCORE: f64 = 30.0;

/// Interpolate [`ROBUST_TIMING_ANCHORS`] at `ms`.
///
/// **Monotonicity.** The anchor table is ascending in time and non-increasing in
/// score, and linear interpolation between adjacent anchors preserves both, so
/// `timing_subscore` is monotone non-increasing in `ms` over its whole domain: a
/// server can never improve its robustness score by getting slower. A property
/// test asserts this over a dense sweep.
fn timing_subscore(ms: u128) -> f64 {
    let ms = ms as f64;
    let anchors = ROBUST_TIMING_ANCHORS;
    let first = anchors[0];
    let last = anchors[anchors.len() - 1];
    if ms <= first.0 {
        return first.1;
    }
    if ms >= last.0 {
        return last.1.max(ROBUST_TIMING_FLOOR);
    }
    for pair in anchors.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        if ms <= x1 {
            let t = if x1 > x0 { (ms - x0) / (x1 - x0) } else { 0.0 };
            return (y0 + (y1 - y0) * t).max(ROBUST_TIMING_FLOOR);
        }
    }
    last.1.max(ROBUST_TIMING_FLOOR)
}

pub(super) fn score_robustness(input: &CheckInput) -> DimensionScore {
    let obs = &input.observations;
    let mut subscores: Vec<f64> = Vec::new();
    let mut findings = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    // Latency sub-score (only if observed).
    if let Some(latency) = obs.list_latency {
        let ms = latency.as_millis();
        let sub = timing_subscore(ms);
        subscores.push(sub);
        parts.push(format!("list {ms}ms"));
        if sub < 100.0 {
            findings.push(Finding {
                dimension: Dimension::Robustness,
                code: FindingCode::RobustnessSlowList,
                severity: Severity::Medium,
                message: format!("tools/list took {ms}ms"),
                fix: "reduce list latency — avoid per-request cold starts or slow enumeration"
                    .to_string(),
                points: 100.0 - sub,
                rank_points: None,
                pinned: false,
            });
        }
    }

    // Boot sub-score (`rubric-v1.3`, SOP 25). Only *boot* is graded: install
    // time is the registry's and the network's, is paid once rather than per
    // session, and is not the author's to fix — so it is reported on the timing
    // line and never scored. See [`crate::boot`] for how the split is taken.
    // `rubric-v1.4`: the *server's* boot, with the measured `npx` launcher floor
    // subtracted. See [`crate::boot::Timing::server_boot`] for why the whole
    // launch was the wrong number and what changed.
    if let Some(boot) = obs.timing.server_boot() {
        let ms = boot.as_millis();
        let sub = timing_subscore(ms);
        subscores.push(sub);
        // Just the graded half here: the full install/boot split has its own
        // line in every renderer, and repeating it inside the robustness
        // summary would imply install was scored too.
        parts.push(format!("boot {ms}ms"));
        if sub < 100.0 {
            findings.push(Finding {
                dimension: Dimension::Robustness,
                code: FindingCode::RobustnessSlowBoot,
                severity: Severity::Medium,
                message: match obs.timing.launcher {
                    Some(floor) => format!(
                        "server boot took {ms}ms (launch to `initialize` response, less the \
                         measured {}ms npx launcher floor; install time excluded)",
                        floor.as_millis()
                    ),
                    None => format!(
                        "server boot took {ms}ms (launch to `initialize` response; install time \
                         excluded)"
                    ),
                },
                fix: "shorten the path from process start to the initialize response — defer \
                      client construction, index building, and network calls until the first \
                      tool call needs them"
                    .to_string(),
                points: 100.0 - sub,
                rank_points: None,
                pinned: false,
            });
        }
    }

    // Credential-failure UX (`rubric-v1.3`, SOP 26). Contributes nothing at all
    // on a server that started, and nothing on the PASS case either — the rule
    // only ever penalizes the shapes that are unambiguously worse for the user.
    // See [`crate::credential`].
    if let Some(sub) = obs.startup.subscore() {
        subscores.push(sub);
        parts.push(obs.startup.tag().replace('_', " "));
    }
    if let Some(f) = obs.startup.finding() {
        findings.push(f);
    }

    // Clean-shutdown sub-score (always observed by the session).
    let shutdown_sub = if obs.clean_shutdown {
        parts.push("clean shutdown".to_string());
        100.0
    } else {
        parts.push("unclean shutdown".to_string());
        findings.push(Finding {
            dimension: Dimension::Robustness,
            code: FindingCode::RobustnessUncleanShutdown,
            severity: Severity::Medium,
            message: "the server did not shut down cleanly".to_string(),
            fix: "handle transport close / EOF and exit promptly on shutdown".to_string(),
            points: 100.0 - ROBUST_UNCLEAN_SHUTDOWN_SCORE,
            rank_points: None,
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
                code: FindingCode::RobustnessStderrNoise,
                severity: Severity::Info,
                message: format!(
                    "server wrote {} bytes to stderr (informational)",
                    commas(bytes)
                ),
                fix: "no action needed — stderr logging is valid; noted for context".to_string(),
                points: 0.0,
                rank_points: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::testkit::*;

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

    /// The ramp must be monotone non-increasing across its whole domain: a
    /// server can never raise its robustness score by getting slower. This is
    /// the same property `rubric-v1.2` asserted over the context cap, and the
    /// reason the `rubric-v1.3` three-bucket step function had to go.
    #[test]
    fn the_timing_ramp_is_monotone_and_bounded() {
        let mut previous = f64::INFINITY;
        for ms in (0u128..=60_000).step_by(37) {
            let score = timing_subscore(ms);
            assert!(
                score <= previous + 1e-9,
                "ramp rose at {ms}ms: {previous} -> {score}"
            );
            assert!(
                (ROBUST_TIMING_FLOOR..=100.0).contains(&score),
                "ramp left its bounds at {ms}ms: {score}"
            );
            previous = score;
        }
    }

    /// The `rubric-v1.3` bucket edges are preserved exactly, so this release
    /// changes the *resolution* of the dimension without moving the judgement it
    /// encoded. Nobody's score jumps because the shape changed.
    #[test]
    fn the_ramp_passes_through_the_v1_3_bucket_edges() {
        assert_eq!(timing_subscore(0), 100.0);
        assert_eq!(timing_subscore(1_000), 100.0);
        assert_eq!(timing_subscore(3_000), 70.0);
    }

    /// The point of the ramp: values the `rubric-v1.3` buckets collapsed into
    /// one constant are now distinguishable. This is what gives robustness any
    /// spread at all.
    #[test]
    fn the_ramp_separates_servers_the_buckets_collapsed() {
        // All three scored exactly 40 under `rubric-v1.3`.
        let a = timing_subscore(3_100);
        let b = timing_subscore(8_800);
        let c = timing_subscore(40_000);
        assert!(a > b && b > c, "no separation: {a} / {b} / {c}");
        // And two servers a millisecond apart no longer differ by 30 points.
        assert!((timing_subscore(2_999) - timing_subscore(3_001)).abs() < 0.1);
    }

    /// The measured `npx` launcher floor reaches the score: two servers with the
    /// same raw launch but different shim costs must not grade the same, and the
    /// one whose time was mostly shim must grade *better*.
    #[test]
    fn the_launcher_floor_reaches_the_robustness_score() {
        let scored = |launcher: Option<Duration>| {
            let mut input = clean_input();
            input.observations.timing = crate::boot::Timing {
                install: Some(Duration::from_millis(12_500)),
                boot: Some(Duration::from_millis(2_900)),
                prewarm_skipped: false,
                launcher,
            };
            evaluate(&input, None)
                .dimension(Dimension::Robustness)
                .unwrap()
                .score
                .unwrap()
        };
        let unsubtracted = scored(None);
        let subtracted = scored(Some(Duration::from_millis(2_600)));
        assert!(
            subtracted > unsubtracted,
            "the floor did not reach the score: {unsubtracted} -> {subtracted}"
        );
        // 2.9s - 2.6s = 0.3s, comfortably inside the "unremarkable" band, so the
        // boot sub-score is a clean 100 and the dimension is no longer dragged.
        assert_eq!(timing_subscore(300), 100.0);
    }

    /// **The `rubric-v1.4` spread test.** Under `rubric-v1.3` every `npx` server
    /// produced robustness exactly 80 — the fleet run measured zero spread over
    /// 26 servers. A dimension that returns one number for every subject is not
    /// measuring the subject. Distinct boot profiles must now produce distinct
    /// scores.
    #[test]
    fn robustness_has_spread_across_distinct_boot_profiles() {
        let scored = |boot_ms: u64| {
            let mut input = clean_input();
            input.observations.timing = crate::boot::Timing {
                install: None,
                boot: Some(Duration::from_millis(boot_ms)),
                prewarm_skipped: false,
                launcher: Some(Duration::from_millis(2_600)),
            };
            evaluate(&input, None)
                .dimension(Dimension::Robustness)
                .unwrap()
                .score
                .unwrap()
        };
        // Raw launches that `rubric-v1.3` scored identically (all > 3s → 40).
        let scores: Vec<f64> = [3_100u64, 5_000, 8_800, 20_000, 45_000]
            .iter()
            .map(|ms| scored(*ms))
            .collect();
        let distinct: std::collections::BTreeSet<String> =
            scores.iter().map(|s| format!("{s:.3}")).collect();
        assert_eq!(
            distinct.len(),
            scores.len(),
            "robustness still collapses distinct servers onto one score: {scores:?}"
        );
        // …and the ordering is the right way round.
        assert!(
            scores.windows(2).all(|w| w[0] > w[1]),
            "slower servers did not score lower: {scores:?}"
        );
    }

    /// The PASS case is informational: naming the variable earns no deduction,
    /// and no sub-score either, so it cannot inflate a grade.
    #[test]
    fn a_named_credential_variable_costs_nothing() {
        let baseline = evaluate(&clean_input(), None);
        let mut input = clean_input();
        input.observations.startup = crate::credential::Verdict::NamedVariable {
            variable: "ACME_API_KEY".to_string(),
            exit_code: 1,
        };
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Robustness).unwrap().score,
            baseline.dimension(Dimension::Robustness).unwrap().score
        );
    }

    /// The three penalized shapes each lower Robustness, in the documented
    /// order: unnamed < hang == exit-zero.
    #[test]
    fn credential_failure_shapes_lower_robustness_in_order() {
        let score_for = |verdict: crate::credential::Verdict| {
            let mut input = clean_input();
            input.observations.startup = verdict;
            evaluate(&input, None)
                .dimension(Dimension::Robustness)
                .unwrap()
                .score
                .unwrap()
        };
        let baseline = score_for(crate::credential::Verdict::NotObserved);
        let unnamed = score_for(crate::credential::Verdict::UnnamedVariable { exit_code: 1 });
        let hung = score_for(crate::credential::Verdict::Hung);
        let zero = score_for(crate::credential::Verdict::ExitedZero);
        assert!(unnamed < baseline, "unnamed must cost something");
        assert!(hung < unnamed, "a hang is worse than a mute exit");
        assert_eq!(hung, zero, "both are scored at zero sub-score");
    }

    /// Only *boot* is scored. Install time is reported and never graded, so two
    /// servers with the same boot and wildly different install costs score
    /// identically.
    #[test]
    fn install_time_is_reported_but_never_scored() {
        let with_timing = |install: Option<Duration>| {
            let mut input = clean_input();
            input.observations.timing = crate::boot::Timing {
                install,
                boot: Some(Duration::from_millis(400)),
                prewarm_skipped: false,
                launcher: None,
            };
            evaluate(&input, None)
                .dimension(Dimension::Robustness)
                .unwrap()
                .score
                .unwrap()
        };
        assert_eq!(
            with_timing(Some(Duration::from_secs(30))),
            with_timing(Some(Duration::from_millis(1))),
        );
    }

    /// A slow boot *is* scored, and draws a finding whose text says install
    /// time was excluded — so the number cannot be misread as a cold start.
    #[test]
    fn a_slow_boot_lowers_robustness_and_says_what_it_measured() {
        let mut input = clean_input();
        input.observations.timing = crate::boot::Timing {
            install: Some(Duration::from_secs(12)),
            boot: Some(Duration::from_secs(5)),
            prewarm_skipped: false,
            launcher: None,
        };
        let report = evaluate(&input, None);
        let robustness = report.dimension(Dimension::Robustness).unwrap();
        assert!(robustness.score.unwrap() < 100.0);
        let finding = robustness
            .findings
            .iter()
            .find(|f| f.message.contains("boot took"))
            .expect("boot finding");
        assert!(finding.message.contains("install time excluded"));
    }
}
