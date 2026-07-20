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
//! # Honesty about what boot still contains
//!
//! Even after the split, "boot" for an `npx` command includes npm's own
//! process-launch overhead (resolving the cached bin shim, spawning node) — tens
//! to low hundreds of milliseconds that belong to the toolchain rather than to
//! the server. jig does not subtract it, because doing so would require timing a
//! null server through the same path and asserting the difference is constant.
//! The number is therefore a slight *over*-estimate of server boot, which is the
//! safe direction for a grade.

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
}

impl Timing {
    /// The one-line rendering every surface uses: `install 7.4s · boot 0.6s`,
    /// or `install n/a · boot 0.6s` when there was nothing to pre-warm.
    pub fn line(&self) -> String {
        let install = match (self.install, self.prewarm_skipped) {
            (Some(d), _) => format_secs(d),
            (None, true) => "skipped".to_string(),
            (None, false) => "n/a".to_string(),
        };
        let boot = self.boot.map_or("n/a".to_string(), format_secs);
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
        };
        assert_eq!(t.line(), "install skipped · boot 0.5s");
    }

    #[test]
    fn renders_when_nothing_was_measured() {
        assert_eq!(Timing::default().line(), "install n/a · boot n/a");
    }
}
