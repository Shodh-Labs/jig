//! **Install-vs-boot timing isolation** (SOP 25, `rubric-v1.3`).
//!
//! # The measurement jig was measuring the wrong thing
//!
//! Jig's own README advertised an *"8-second `npx` cold start"* for
//! `@modelcontextprotocol/server-everything`, and SOP 25 cited it as evidence
//! that servers should budget their cold start. That number is real but it is
//! **two numbers glued together**: npm resolving and downloading a package tree
//! into the `_npx` cache, and then the server process actually booting and
//! answering `initialize`.
//!
//! Only the second is a property of the server. The first is a property of the
//! registry, the network, and whether the user has run this package before —
//! and it is paid **once**, not per session. Grading them as one figure told
//! authors to optimize something most of them do not control, and let a server
//! with a genuinely slow boot hide inside a big download.
//!
//! Measured with this module against `@modelcontextprotocol/server-everything`,
//! the split is stark: the overwhelming majority of the "cold start" is install,
//! and boot is a small fraction of a second. See `docs/rubric-changelog.md` for
//! the recorded numbers.
//!
//! # How the split is taken
//!
//! For an `npx`-shaped command, jig runs a **pre-warm pass** first:
//!
//! ```text
//! npx --yes --package <pkg> -- node -e ""
//! ```
//!
//! This resolves and installs `<pkg>` into the `_npx` cache and then runs a
//! trivial `node` program *instead of* the package's own binary, so the cache is
//! populated without the server ever starting. That pass is timed as
//! **install**. The real launch is then timed from spawn to the `initialize`
//! response and reported as **boot**.
//!
//! The `--` separator is what makes this work and is not optional: without it
//! `npx --package X node -e ""` is ambiguous, and `npx -y <pkg>` would run the
//! package's declared binary — which is the server, which is the thing we are
//! trying not to start.
//!
//! **Only boot is scored.** Install is reported and never graded, because it is
//! not the server author's to fix. For a non-`npx` command there is nothing to
//! pre-warm, so install is reported as `n/a` and boot is the whole launch.
//!
//! # The launcher floor (`rubric-v1.4`)
//!
//! `rubric-v1.3` stopped one step short. Even after the install/boot split,
//! "boot" for an `npx` command still contained npm's own shim resolution and
//! process launch — measured at **~2.6s of a ~2.9s reported boot**, against
//! ~0.3s of actual server. It declined to subtract that, and said why: *"doing so
//! would require timing a null server through the same path."*
//!
//! A 50-server fleet run turned the caveat into a defect — every `npx` server
//! tripped the same boot penalty, and robustness came out **exactly 80 for all
//! 26 graded servers**. So the pre-warm pass now runs **twice**: once cold, timed
//! as *install*, and once warm, which *is* a null program timed through the
//! identical path and is recorded as the **launcher floor**.
//!
//! [`Timing::server_boot`] subtracts it. The correction is measured per run
//! rather than asserted as a constant, which was the whole of the objection, and
//! the subtraction is stated in [`Timing::line`] rather than applied silently.
//! Where no floor could be measured nothing is subtracted, so the number remains
//! an *over*-estimate — the safe direction for a grade.

use std::time::Duration;

/// The two halves of a cold start, measured separately (`rubric-v1.3`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Timing {
    /// Time spent populating the package cache, when a pre-warm pass ran.
    /// `None` for a non-`npx` command (nothing to install), or when pre-warming
    /// was skipped with `--no-prewarm`.
    pub install: Option<Duration>,
    /// Time from process spawn to the `initialize` response — the part that is
    /// actually the server's, and the only part scored.
    pub boot: Option<Duration>,
    /// Whether pre-warming was skipped by request (`--no-prewarm`). Distinguishes
    /// "there was nothing to install" from "we did not look", so a reader is
    /// never misled by an `n/a`.
    pub prewarm_skipped: bool,
    /// The **measured launcher floor** (`rubric-v1.4`): how long the same `npx`
    /// path takes to launch a *null* program once the cache is warm. `None` for
    /// a non-`npx` command, or when the calibration pass could not run.
    ///
    /// This is the number `rubric-v1.3` said it could not defensibly produce —
    /// see [`Timing::server_boot`].
    pub launcher: Option<Duration>,
}

impl Timing {
    /// The part of [`boot`](Self::boot) that is actually **the server's**:
    /// measured boot less the measured launcher floor, saturating at zero.
    ///
    /// # Why this exists (`rubric-v1.4`)
    ///
    /// `rubric-v1.3` scored `boot` whole, and its own changelog admitted the
    /// problem: of the ~2.9s it reported for `server-everything`, roughly 2.6s
    /// was npm shim overhead and 0.3s was the server. It declined to subtract
    /// the 2.6s because *"the correction is not a constant, and measuring it
    /// would require timing a null server through the same path on every run"*.
    ///
    /// A 50-server fleet run turned that caveat into a defect. Every `npx`
    /// server tripped the same boot penalty, so robustness scored **exactly 80
    /// for all 26 graded servers** — zero spread, a constant wearing a
    /// dimension's clothes. A dimension that returns the same number for every
    /// subject is not measuring the subject.
    ///
    /// So jig now does the thing the caveat described: the pre-warm pass runs
    /// **twice**, and the second run — cache already warm, `node -e ""` in place
    /// of the server — *is* a null server timed through the same path. The
    /// correction is measured per-run rather than asserted as a constant, which
    /// is exactly the objection `rubric-v1.3` raised against subtracting it.
    ///
    /// Saturating at zero is deliberate: launcher cost is noisy, and a server
    /// that boots faster than the null program did on that machine has a
    /// *measurement* below the floor, not a negative boot time.
    pub fn server_boot(&self) -> Option<Duration> {
        let boot = self.boot?;
        Some(match self.launcher {
            Some(floor) => boot.saturating_sub(floor),
            None => boot,
        })
    }

    /// The one-line rendering every surface uses: `install 7.4s · boot 0.6s`,
    /// or `install n/a · boot 0.6s` when there was nothing to pre-warm.
    ///
    /// When a launcher floor was measured the line states all three numbers, so
    /// the subtraction is checkable rather than silent — the same discipline
    /// `rubric-v1.2` applied to the context cap, which states the sub-score that
    /// produced it:
    ///
    /// ```text
    /// install 12.5s · boot 0.5s (3.1s launch − 2.6s npx shim)
    /// ```
    pub fn line(&self) -> String {
        let install = match (self.install, self.prewarm_skipped) {
            (Some(d), _) => format_secs(d),
            (None, true) => "skipped".to_string(),
            (None, false) => "n/a".to_string(),
        };
        let boot = match (self.server_boot(), self.boot, self.launcher) {
            (Some(server), Some(raw), Some(floor)) => format!(
                "{} ({} launch − {} npx shim)",
                format_secs(server),
                format_secs(raw),
                format_secs(floor)
            ),
            (Some(server), _, _) => format_secs(server),
            _ => "n/a".to_string(),
        };
        format!("install {install} · boot {boot}")
    }
}

/// Format a duration the way the report does: one decimal place, seconds.
fn format_secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// Whether a program name is `npx` in any of the forms a user can type or a
/// discovery file can hold: bare, `.cmd`/`.exe`-suffixed (Windows shims), or an
/// absolute path ending in one of those.
pub fn is_npx(program: &str) -> bool {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_lowercase();
    matches!(base.as_str(), "npx" | "npx.cmd" | "npx.exe" | "npx.ps1")
}

/// The package an `npx` invocation will install, if one can be identified —
/// pure, so the parse is unit-testable without spawning anything.
///
/// `npx`'s own argument grammar is respected: value-taking flags consume their
/// argument, boolean flags do not, `--package=<pkg>` and `--package <pkg>` are
/// both honoured, and everything after `--` is the command rather than the
/// package. The first bare token that is not consumed by a flag is the package
/// specifier.
///
/// Returns `None` when the command is not `npx`-shaped or names no package
/// (`npx` with only flags, or `npx -- node script.js`), in which case there is
/// nothing to pre-warm and install is reported as `n/a`.
pub fn npx_package(program: &str, args: &[String]) -> Option<String> {
    if !is_npx(program) {
        return None;
    }
    /// `npx` flags that take a separate value argument.
    const VALUE_FLAGS: &[&str] = &["-p", "--package", "-c", "--call", "--userconfig", "--shell"];
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // Everything after a bare `--` is the command to run, not a package.
        if arg == "--" {
            return None;
        }
        if let Some(rest) = arg.strip_prefix("--package=") {
            return non_empty(rest);
        }
        if let Some(rest) = arg.strip_prefix("-p=") {
            return non_empty(rest);
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            // `--package <pkg>` names the package explicitly — the strongest
            // signal available, so take it rather than waiting for a bare token.
            let value = it.next();
            if matches!(arg.as_str(), "-p" | "--package") {
                return value.and_then(|v| non_empty(v));
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return non_empty(arg);
    }
    None
}

/// `Some(s.to_string())` unless `s` is empty.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// The argument vector that populates the `npx` cache for `package` **without
/// starting the server**.
///
/// `npx --yes --package <pkg> -- node -e ""` installs `<pkg>` and then runs
/// `node` — not the package's own binary. `--yes` suppresses the install prompt
/// so the pass cannot block on stdin, which matters because jig may be running
/// non-interactively in CI.
pub fn prewarm_args(package: &str) -> Vec<String> {
    vec![
        "--yes".to_string(),
        "--package".to_string(),
        package.to_string(),
        "--".to_string(),
        "node".to_string(),
        "-e".to_string(),
        String::new(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- npx detection -----------------------------------------------------

    #[test]
    fn recognizes_every_npx_spelling() {
        for program in [
            "npx",
            "NPX",
            "npx.cmd",
            "npx.exe",
            "/usr/local/bin/npx",
            "C:\\Program Files\\nodejs\\npx.cmd",
        ] {
            assert!(is_npx(program), "not recognized: {program}");
        }
    }

    #[test]
    fn does_not_mistake_other_programs_for_npx() {
        for program in ["node", "npm", "pnpx", "my-npx-wrapper", "uvx", "python"] {
            assert!(!is_npx(program), "wrongly recognized: {program}");
        }
    }

    // ---- package extraction ------------------------------------------------

    #[test]
    fn extracts_the_package_from_common_invocations() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["-y", "@modelcontextprotocol/server-everything"],
                "@modelcontextprotocol/server-everything",
            ),
            (&["--yes", "server-filesystem"], "server-filesystem"),
            (&["@scope/pkg@1.2.3"], "@scope/pkg@1.2.3"),
            (&["--package", "some-pkg", "run-me"], "some-pkg"),
            (&["--package=some-pkg", "run-me"], "some-pkg"),
            (&["-p", "some-pkg", "run-me"], "some-pkg"),
            (&["-p=some-pkg"], "some-pkg"),
            // Flags before the package are skipped, not mistaken for it.
            (&["-y", "--quiet", "the-pkg"], "the-pkg"),
        ];
        for (a, expected) in cases {
            assert_eq!(
                npx_package("npx", &args(a)).as_deref(),
                Some(*expected),
                "failed on {a:?}"
            );
        }
    }

    /// `--package` names the package explicitly, so it wins over any later bare
    /// token — which is the *command*, not something to install.
    #[test]
    fn explicit_package_flag_beats_a_later_bare_token() {
        assert_eq!(
            npx_package("npx", &args(&["--package", "real-pkg", "some-command"])).as_deref(),
            Some("real-pkg")
        );
    }

    #[test]
    fn returns_none_when_there_is_nothing_to_prewarm() {
        // Not npx at all.
        assert_eq!(npx_package("node", &args(&["server.js"])), None);
        // Everything after `--` is the command, not a package.
        assert_eq!(npx_package("npx", &args(&["--", "node", "s.js"])), None);
        // Flags only.
        assert_eq!(npx_package("npx", &args(&["-y"])), None);
        assert_eq!(npx_package("npx", &args(&[])), None);
    }

    #[test]
    fn package_extraction_is_total() {
        // Arbitrary junk must never panic, and must not invent a package.
        for a in [
            vec!["--package"],
            vec!["-p"],
            vec!["--package="],
            vec![""],
            vec!["--", "--package", "x"],
        ] {
            let _ = npx_package("npx", &args(&a));
        }
        assert_eq!(npx_package("npx", &args(&["--package"])), None);
        assert_eq!(npx_package("npx", &args(&["--package="])), None);
    }

    // ---- the pre-warm command ---------------------------------------------

    /// The pre-warm pass must install the package and then run *node*, never the
    /// package's own binary — otherwise it would start the very server it is
    /// supposed to avoid starting, and the split would be meaningless.
    #[test]
    fn prewarm_args_install_without_starting_the_server() {
        let a = prewarm_args("@scope/pkg");
        assert_eq!(
            a,
            vec![
                "--yes".to_string(),
                "--package".to_string(),
                "@scope/pkg".to_string(),
                "--".to_string(),
                "node".to_string(),
                "-e".to_string(),
                String::new(),
            ]
        );
        // The `--` separator is load-bearing: without it npx would treat `node`
        // as another package rather than as the command to run.
        let sep = a.iter().position(|s| s == "--").expect("separator present");
        assert_eq!(a[sep + 1], "node");
        // `--yes` must come first so the pass can never block on a prompt.
        assert_eq!(a[0], "--yes");
    }

    // ---- rendering ---------------------------------------------------------

    #[test]
    fn renders_both_halves_when_both_are_known() {
        let t = Timing {
            install: Some(Duration::from_millis(7_400)),
            boot: Some(Duration::from_millis(600)),
            prewarm_skipped: false,
            launcher: None,
        };
        assert_eq!(t.line(), "install 7.4s · boot 0.6s");
    }

    /// A non-npx command has nothing to install, and says so — `n/a` rather than
    /// a zero that would read as "instant".
    #[test]
    fn renders_na_when_there_was_nothing_to_install() {
        let t = Timing {
            install: None,
            boot: Some(Duration::from_millis(1_300)),
            prewarm_skipped: false,
            launcher: None,
        };
        assert_eq!(t.line(), "install n/a · boot 1.3s");
    }

    /// "We did not look" must be distinguishable from "there was nothing to
    /// look at", or `--no-prewarm` would silently misreport an npx server as
    /// having no install cost.
    #[test]
    fn skipped_prewarm_is_distinct_from_not_applicable() {
        let t = Timing {
            install: None,
            boot: Some(Duration::from_millis(500)),
            prewarm_skipped: true,
            launcher: None,
        };
        assert_eq!(t.line(), "install skipped · boot 0.5s");
    }

    #[test]
    fn renders_when_nothing_was_measured() {
        assert_eq!(Timing::default().line(), "install n/a · boot n/a");
    }

    // ---- rubric-v1.4: the launcher floor -----------------------------------

    fn timing(boot_ms: u64, launcher_ms: Option<u64>) -> Timing {
        Timing {
            install: Some(Duration::from_millis(12_500)),
            boot: Some(Duration::from_millis(boot_ms)),
            prewarm_skipped: false,
            launcher: launcher_ms.map(Duration::from_millis),
        }
    }

    /// The headline `rubric-v1.4` number, using the figures the `rubric-v1.3`
    /// changelog itself recorded: ~2.9s of reported boot for
    /// `server-everything`, of which ~2.6s was the npx shim and ~0.3s was the
    /// server. The scored figure is now the 0.3s.
    #[test]
    fn the_launcher_floor_is_subtracted_from_boot() {
        let t = timing(2_900, Some(2_600));
        assert_eq!(t.server_boot(), Some(Duration::from_millis(300)));
    }

    /// With no floor measured nothing is subtracted — the `rubric-v1.3`
    /// behaviour, and the safe direction for a grade.
    #[test]
    fn without_a_measured_floor_boot_is_unchanged() {
        let t = timing(2_900, None);
        assert_eq!(t.server_boot(), t.boot);
    }

    /// Launcher cost is noisy. A server that beat the null program is at the
    /// floor of measurement, not below zero.
    #[test]
    fn a_boot_under_the_floor_saturates_at_zero() {
        assert_eq!(
            timing(1_000, Some(2_600)).server_boot(),
            Some(Duration::ZERO)
        );
    }

    /// Subtraction is never silent: the line states the raw launch and the floor
    /// alongside the graded figure, so a reader can check the arithmetic — the
    /// same discipline `rubric-v1.2` applied to the context cap.
    #[test]
    fn the_subtraction_is_stated_not_silent() {
        let line = timing(2_900, Some(2_600)).line();
        assert_eq!(
            line,
            "install 12.5s · boot 0.3s (2.9s launch − 2.6s npx shim)"
        );
        // …and is absent when there was nothing to subtract.
        assert_eq!(timing(2_900, None).line(), "install 12.5s · boot 2.9s");
    }

    /// Subtracting a floor can only ever *lower* the reported boot, so
    /// `rubric-v1.4` cannot punish a server the previous release let through.
    #[test]
    fn subtraction_is_monotone_and_never_increases_boot() {
        for boot_ms in [0u64, 100, 300, 1_000, 2_900, 8_800, 60_000] {
            for floor_ms in [0u64, 50, 500, 2_600, 5_000] {
                let t = timing(boot_ms, Some(floor_ms));
                assert!(
                    t.server_boot().unwrap() <= t.boot.unwrap(),
                    "boot rose: {boot_ms}ms with a {floor_ms}ms floor"
                );
            }
            // A larger floor never yields a larger scored boot.
            let a = timing(boot_ms, Some(500)).server_boot().unwrap();
            let b = timing(boot_ms, Some(2_600)).server_boot().unwrap();
            assert!(b <= a, "a larger floor raised boot at {boot_ms}ms");
        }
    }
}
