//! **Credential-failure UX grading** (SOP 26, `rubric-v1.3`) — how a stdio
//! server behaves when it cannot start because a credential is missing.
//!
//! # Why this is worth a rule
//!
//! Failing to start is not itself a defect: a server that needs an API key and
//! does not have one *should* refuse. What varies — and what the user actually
//! experiences — is the **shape** of the refusal. The census measured 29 servers
//! over stdio; **14 died on a missing credential and 2 hung until the timeout
//! fired**. Those two populations look identical in a score that only records
//! "did not start", and they are not remotely the same product:
//!
//! - A server that exits non-zero saying `GITHUB_TOKEN is not set` has told the
//!   user exactly what to do. There is nothing to fix.
//! - A server that exits non-zero saying `Error: undefined` has done the right
//!   thing mutely; the user now bisects a config file.
//! - A server that **hangs** has given the client no signal at all. The user
//!   waits out a timeout and reads it as "jig is broken".
//! - A server that **exits zero** has actively lied. A supervisor reads success
//!   and does not restart; a client cannot distinguish it from a clean shutdown.
//!
//! Until `rubric-v1.3` SOP 26's "Verify" line honestly read *partially
//! machine-checkable* — jig could show which env keys a client passes, but not
//! grade the failure UX. This module closes that half.
//!
//! # Purity
//!
//! [`grade`] is a pure function of a [`StartupObservation`] — plain data the
//! caller gathers by watching the child process. It opens nothing and parses
//! only strings, so every verdict is unit-testable against a constructed
//! fixture, exactly like [`crate::check::evaluate`].
//!
//! # What it does *not* claim
//!
//! Naming a variable in stderr is not proof the server documents it, and this
//! module cannot tell a genuine credential failure from any other non-zero exit
//! that happens to mention a capitalized identifier. The [`Verdict::NamedVariable`]
//! outcome is therefore informational and carries **no deduction** — the rule
//! only ever *penalizes* the three shapes that are unambiguously worse for the
//! user, and never rewards the good one with points it cannot justify.

use crate::check::{Dimension, Finding, Severity};

/// Words that look like environment variables but are log furniture. Matched
/// case-sensitively against the whole candidate token, so `ERROR_CHANNEL` (a
/// plausible variable) survives while a bare `ERROR` does not.
const STOPWORDS: &[&str] = &[
    "ERROR",
    "ERR",
    "WARN",
    "WARNING",
    "FATAL",
    "INFO",
    "DEBUG",
    "TRACE",
    "PANIC",
    "FAIL",
    "FAILED",
    "FAILURE",
    "EXIT",
    "NULL",
    "NONE",
    "TRUE",
    "FALSE",
    "JSON",
    "HTTP",
    "HTTPS",
    "MCP",
    "TODO",
    "NOTE",
    "STDIN",
    "STDOUT",
    "STDERR",
    "EOF",
    "USAGE",
    "MISSING",
    "REQUIRED",
    "SET",
    "ENV",
    "ENVIRONMENT",
    "VARIABLE",
    "CONFIG",
    "OK",
    "GET",
    "POST",
    "PUT",
    "DELETE",
];

/// Lowercase cues that mark a line as *talking about* configuration. A candidate
/// token in a line containing one of these is taken to be a variable name even
/// without a `KEY=value` shape.
const CONTEXT_CUES: &[&str] = &[
    "env",
    "environment",
    "variable",
    "var ",
    "set ",
    "unset",
    "missing",
    "required",
    "not set",
    "not configured",
    "not provided",
    "must be",
    "expected",
    "credential",
    "api key",
    "token",
    "secret",
    "please provide",
    "export ",
];

/// Suffixes that make a candidate token *look* like a credential variable.
/// Used only to rank competing candidates on one line, never to reject.
const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASS",
    "_API_KEY",
    "_URL",
    "_URI",
    "_ID",
    "_ACCESS_KEY",
    "_CREDENTIALS",
    "_DSN",
    "_ENDPOINT",
    "_HOST",
    "_ACCOUNT",
];

/// Minimum length of a candidate environment-variable token — the `{2,}` tail of
/// the `[A-Z][A-Z0-9_]{2,}` shape, i.e. three characters total. Shorter runs of
/// capitals in prose ("IO", "DB" on its own) are too weak to act on.
const MIN_TOKEN_LEN: usize = 3;

/// How the server behaved when it failed to start. Plain observation, no
/// judgement — [`grade`] supplies that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupObservation {
    /// The child's exit status, if it exited. `None` means it was still running
    /// when the caller gave up (see [`hung`](Self::hung)).
    pub exit_code: Option<i32>,
    /// Whether the caller stopped waiting because its timeout fired rather than
    /// because the child exited or answered.
    pub hung: bool,
    /// The child's retained stderr lines, oldest first — the transport's ring
    /// buffer, verbatim.
    pub stderr: Vec<String>,
}

/// The graded verdict on a failed startup (`rubric-v1.3`, SOP 26).
///
/// Ordered best-to-worst. See the [module docs](self#why-this-is-worth-a-rule)
/// for why these four and not a single "did not start".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Verdict {
    /// The server was not observed failing to start — the overwhelmingly common
    /// case, and the reason this dimension contributes no sub-score at all
    /// unless something actually went wrong.
    #[default]
    NotObserved,
    /// Exited non-zero **and** named the environment variable it needed. This is
    /// the target behaviour: informational, no deduction.
    NamedVariable {
        /// The variable the server named.
        variable: String,
        /// The exit code it used.
        exit_code: i32,
    },
    /// Exited non-zero without naming any variable. Fail-fast is right; saying
    /// which variable is the missing half.
    UnnamedVariable {
        /// The exit code it used.
        exit_code: i32,
    },
    /// Never exited — the caller's timeout fired first.
    Hung,
    /// Exited with status **zero** after failing to start.
    ExitedZero,
}

impl Verdict {
    /// A short machine tag (`named_variable`, `unnamed_variable`, `hung`,
    /// `exited_zero`, `not_observed`).
    pub fn tag(&self) -> &'static str {
        match self {
            Verdict::NotObserved => "not_observed",
            Verdict::NamedVariable { .. } => "named_variable",
            Verdict::UnnamedVariable { .. } => "unnamed_variable",
            Verdict::Hung => "hung",
            Verdict::ExitedZero => "exited_zero",
        }
    }

    /// The one-line verdict `jig check` and `jig info --probe` both print, so
    /// the two commands can never disagree about what was observed.
    pub fn line(&self) -> String {
        match self {
            Verdict::NotObserved => "startup: not observed".to_string(),
            Verdict::NamedVariable {
                variable,
                exit_code,
            } => format!(
                "credential UX: PASS — exited {exit_code} and named the missing variable \
                 `{variable}`"
            ),
            Verdict::UnnamedVariable { exit_code } => format!(
                "credential UX: exited {exit_code} but named no environment variable — \
                 fail-fast is right; say which variable"
            ),
            Verdict::Hung => "credential UX: HUNG — the server never exited and never answered \
                              `initialize`"
                .to_string(),
            Verdict::ExitedZero => "credential UX: exited 0 after failing to start — a client \
                                    cannot distinguish this from success"
                .to_string(),
        }
    }

    /// The robustness sub-score this verdict contributes, or `None` when it
    /// contributes none (nothing observed, or the PASS case, which is
    /// informational by design — see the [module docs](self#what-it-does-not-claim)).
    pub fn subscore(&self) -> Option<f64> {
        match self {
            Verdict::NotObserved | Verdict::NamedVariable { .. } => None,
            Verdict::UnnamedVariable { .. } => Some(crate::check::ROBUST_CRED_UNNAMED_SCORE),
            Verdict::Hung => Some(crate::check::ROBUST_CRED_HANG_SCORE),
            Verdict::ExitedZero => Some(crate::check::ROBUST_CRED_EXIT_ZERO_SCORE),
        }
    }

    /// The [`Finding`] this verdict produces, or `None` when nothing was
    /// observed. Attached to [`Dimension::Robustness`], which is where the
    /// sub-score lands.
    pub fn finding(&self) -> Option<Finding> {
        let (severity, message, fix) = match self {
            Verdict::NotObserved => return None,
            Verdict::NamedVariable { variable, .. } => (
                Severity::Info,
                self.line(),
                format!(
                    "no action needed — exiting non-zero with a message naming `{variable}` is \
                     exactly the credential UX SOP 26 asks for"
                ),
            ),
            Verdict::UnnamedVariable { .. } => (
                Severity::Medium,
                self.line(),
                "name the missing environment variable in the failure message, e.g. \
                 `MYSERVICE_API_KEY is not set — see README`. The user is holding a config \
                 file with several keys in it and cannot tell which one you wanted (SOP 26)"
                    .to_string(),
            ),
            Verdict::Hung => (
                Severity::High,
                self.line(),
                "never block on a missing credential — check for it before opening the \
                 transport and exit non-zero with a message naming the variable. A hang gives \
                 the client no signal to act on, so the user waits out a timeout and blames \
                 the client (SOP 26)"
                    .to_string(),
            ),
            Verdict::ExitedZero => (
                Severity::High,
                self.line(),
                "exit with a non-zero status when startup fails. A zero exit tells a \
                 supervisor the run succeeded, so it will not restart, and tells a client \
                 nothing went wrong (SOP 26)"
                    .to_string(),
            ),
        };
        Some(Finding {
            dimension: Dimension::Robustness,
            severity,
            message,
            fix,
            points: self.subscore().map_or(0.0, |s| 100.0 - s),
            rank_points: None,
            // A credential failure is the reason the session ended; nothing else
            // in the report can matter more to the reader.
            pinned: severity != Severity::Info,
        })
    }
}

/// Grade an observed startup failure (`rubric-v1.3`, SOP 26). Pure and total.
///
/// The order of tests matters and is deliberate: a hang is checked **before**
/// the exit code, because a caller that timed out may also have reaped an exit
/// status while tearing the child down, and what the *user* experienced was the
/// hang.
pub fn grade(obs: &StartupObservation) -> Verdict {
    if obs.hung {
        return Verdict::Hung;
    }
    match obs.exit_code {
        None => Verdict::Hung,
        Some(0) => Verdict::ExitedZero,
        Some(code) => match named_variable(&obs.stderr) {
            Some(variable) => Verdict::NamedVariable {
                variable,
                exit_code: code,
            },
            None => Verdict::UnnamedVariable { exit_code: code },
        },
    }
}

/// The environment variable a server named in its stderr, if it named one.
///
/// Two shapes are accepted, and neither uses a regex — the grammar is small
/// enough to read as code:
///
/// 1. **`KEY=value` form.** A candidate token immediately followed by `=`. This
///    is self-evidencing: nothing else in a log line looks like that.
/// 2. **Prose form.** A candidate token on a line that also contains one of
///    [`CONTEXT_CUES`] (`env`, `missing`, `required`, `set`, …), which is what
///    makes it a statement *about configuration* rather than an acronym that
///    happens to be capitalized.
///
/// A candidate token is `[A-Z][A-Z0-9_]{2,}` — the shape the brief specifies —
/// less the [`STOPWORDS`] that are log furniture. When a line offers several,
/// the one that looks most like a credential wins (see [`CREDENTIAL_SUFFIXES`]),
/// then the longest, then the first: a stable total order, so the same stderr
/// always yields the same variable.
pub fn named_variable(stderr: &[String]) -> Option<String> {
    let mut best: Option<(u8, usize, String)> = None;
    for line in stderr {
        let lower = line.to_lowercase();
        let has_cue = CONTEXT_CUES.iter().any(|c| lower.contains(c));
        for (token, assigned) in candidate_tokens(line) {
            if !assigned && !has_cue {
                continue;
            }
            // Rank: `KEY=` form beats prose, credential-suffixed beats plain.
            let rank = u8::from(assigned) * 2 + u8::from(is_credential_shaped(&token));
            let candidate = (rank, token.len(), token);
            let better = match &best {
                None => true,
                Some(current) => (candidate.0, candidate.1) > (current.0, current.1),
            };
            if better {
                best = Some(candidate);
            }
        }
    }
    best.map(|(_, _, token)| token)
}

/// Every `[A-Z][A-Z0-9_]{2,}` token in a line, paired with whether it was
/// immediately followed by `=` (the `KEY=value` shape).
fn candidate_tokens(line: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_uppercase() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len()
            && (chars[i].is_ascii_uppercase() || chars[i].is_ascii_digit() || chars[i] == '_')
        {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        // Trailing underscores are punctuation, not part of the name.
        let token = token.trim_end_matches('_').to_string();
        let assigned = chars.get(i) == Some(&'=');
        if token.chars().count() >= MIN_TOKEN_LEN && !STOPWORDS.contains(&token.as_str()) {
            out.push((token, assigned));
        }
        // Skip whatever non-candidate character stopped the run, so a token that
        // ends the line cannot loop.
        if i == start {
            i += 1;
        }
    }
    out
}

/// Whether a token carries a suffix that marks it as a credential-ish variable.
fn is_credential_shaped(token: &str) -> bool {
    CREDENTIAL_SUFFIXES.iter().any(|s| token.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(exit_code: Option<i32>, hung: bool, stderr: &[&str]) -> StartupObservation {
        StartupObservation {
            exit_code,
            hung,
            stderr: stderr.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ---- the verdict matrix ------------------------------------------------

    #[test]
    fn nonzero_exit_naming_the_variable_is_a_pass() {
        let v = grade(&obs(
            Some(1),
            false,
            &["Error: GITHUB_TOKEN is not set. See the README."],
        ));
        assert_eq!(
            v,
            Verdict::NamedVariable {
                variable: "GITHUB_TOKEN".to_string(),
                exit_code: 1,
            }
        );
        // PASS is informational: no sub-score, no deduction, not pinned.
        assert_eq!(v.subscore(), None);
        let f = v.finding().expect("a finding is still reported");
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.points, 0.0);
        assert!(!f.pinned);
        assert!(f.message.contains("PASS"));
    }

    #[test]
    fn nonzero_exit_without_a_variable_is_medium() {
        let v = grade(&obs(
            Some(1),
            false,
            &["Error: undefined", "  at Object.<anonymous>"],
        ));
        assert_eq!(v, Verdict::UnnamedVariable { exit_code: 1 });
        assert_eq!(v.subscore(), Some(crate::check::ROBUST_CRED_UNNAMED_SCORE));
        let f = v.finding().expect("finding");
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.fix.contains("SOP 26"));
        assert!(f.pinned);
    }

    #[test]
    fn hanging_is_high_and_scores_zero() {
        let v = grade(&obs(None, true, &[]));
        assert_eq!(v, Verdict::Hung);
        assert_eq!(v.subscore(), Some(0.0));
        let f = v.finding().expect("finding");
        assert_eq!(f.severity, Severity::High);
        assert!(f.message.contains("HUNG"));
    }

    #[test]
    fn exiting_zero_on_a_failed_start_is_high_and_scores_zero() {
        let v = grade(&obs(Some(0), false, &["missing API_KEY"]));
        assert_eq!(v, Verdict::ExitedZero);
        assert_eq!(v.subscore(), Some(0.0));
        assert_eq!(v.finding().expect("finding").severity, Severity::High);
    }

    /// Exiting zero is graded on the exit status alone — naming the variable
    /// does not redeem it, because the client never sees the message as a
    /// failure signal in the first place.
    #[test]
    fn exit_zero_beats_a_named_variable() {
        assert_eq!(
            grade(&obs(Some(0), false, &["GITHUB_TOKEN is required"])),
            Verdict::ExitedZero
        );
    }

    /// A hang is what the *user* experienced, so it is graded ahead of any exit
    /// status the caller reaped while tearing the child down.
    #[test]
    fn hang_takes_precedence_over_a_late_exit_status() {
        assert_eq!(
            grade(&obs(Some(1), true, &["TOKEN missing"])),
            Verdict::Hung
        );
    }

    #[test]
    fn no_exit_and_no_timeout_is_still_a_hang() {
        assert_eq!(grade(&obs(None, false, &[])), Verdict::Hung);
    }

    #[test]
    fn nothing_observed_produces_nothing() {
        let v = Verdict::NotObserved;
        assert_eq!(v.subscore(), None);
        assert!(v.finding().is_none());
    }

    // ---- variable extraction ----------------------------------------------

    /// The shapes real servers actually emit, gathered from the census's
    /// credential-failure cohort.
    #[test]
    fn extracts_variables_from_realistic_stderr() {
        let cases: &[(&str, &str)] = &[
            ("Error: GITHUB_TOKEN is not set", "GITHUB_TOKEN"),
            (
                "missing required environment variable SLACK_BOT_TOKEN",
                "SLACK_BOT_TOKEN",
            ),
            ("BRAVE_API_KEY=", "BRAVE_API_KEY"),
            ("please set OPENAI_API_KEY and retry", "OPENAI_API_KEY"),
            ("FATAL: env NOTION_API_KEY required", "NOTION_API_KEY"),
            ("export DATABASE_URL=postgres://...", "DATABASE_URL"),
            (
                "Configuration error: STRIPE_SECRET must be provided",
                "STRIPE_SECRET",
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                named_variable(&[line.to_string()]).as_deref(),
                Some(*expected),
                "failed on {line:?}"
            );
        }
    }

    /// Log furniture is not an environment variable. Without the stopword list
    /// every stack trace would "name a variable" and the PASS verdict would be
    /// worthless.
    #[test]
    fn log_furniture_is_not_mistaken_for_a_variable() {
        for line in [
            "ERROR: something went wrong",
            "FATAL failure in HTTP handler",
            "WARN: JSON parse failed",
            "    at Object.<anonymous> (/app/index.js:1:1)",
            "Error: ECONNREFUSED",
        ] {
            assert_eq!(named_variable(&[line.to_string()]), None, "on {line:?}");
        }
    }

    /// A capitalized token with no configuration cue anywhere on the line is
    /// not evidence: `Error: TIMEOUT` names no variable.
    #[test]
    fn a_bare_token_without_a_context_cue_is_not_a_variable() {
        assert_eq!(
            named_variable(&["Startup aborted: TIMEOUT".to_string()]),
            None
        );
        // …but the same token *with* a cue is.
        assert_eq!(
            named_variable(&["missing env TIMEOUT_MS".to_string()]).as_deref(),
            Some("TIMEOUT_MS")
        );
    }

    /// `KEY=value` is self-evidencing and needs no cue.
    #[test]
    fn assignment_form_needs_no_context_cue() {
        assert_eq!(
            named_variable(&["ACME_TOKEN=<redacted>".to_string()]).as_deref(),
            Some("ACME_TOKEN")
        );
    }

    /// When a line offers several candidates the credential-shaped one wins, so
    /// the fix text names the variable the user actually has to set.
    #[test]
    fn credential_shaped_candidates_outrank_plain_ones() {
        assert_eq!(
            named_variable(&["missing env: set HOME and ACME_API_KEY".to_string()]).as_deref(),
            Some("ACME_API_KEY")
        );
    }

    #[test]
    fn extraction_is_total_and_deterministic() {
        for line in [
            "",
            "   ",
            "=",
            "A",
            "AB",
            "___",
            "A=",
            "\u{1f600} MISSING env",
        ] {
            let once = named_variable(&[line.to_string()]);
            let twice = named_variable(&[line.to_string()]);
            assert_eq!(once, twice);
        }
        // Below the three-character minimum, so never a candidate.
        assert_eq!(named_variable(&["env AB missing".to_string()]), None);
    }

    #[test]
    fn every_penalizing_verdict_carries_a_fix_and_a_deduction() {
        for v in [
            Verdict::UnnamedVariable { exit_code: 1 },
            Verdict::Hung,
            Verdict::ExitedZero,
        ] {
            let f = v.finding().expect("finding");
            assert!(!f.fix.is_empty());
            assert!(f.fix.contains("SOP 26"));
            assert!(f.points > 0.0);
            assert!(f.pinned);
            assert_eq!(f.dimension, Dimension::Robustness);
        }
    }

    /// The verdict line is shared by `jig check` and `jig info --probe`, so the
    /// two can never disagree about what was observed.
    #[test]
    fn every_verdict_has_a_nonempty_line_and_tag() {
        for v in [
            Verdict::NotObserved,
            Verdict::NamedVariable {
                variable: "X_TOKEN".to_string(),
                exit_code: 1,
            },
            Verdict::UnnamedVariable { exit_code: 2 },
            Verdict::Hung,
            Verdict::ExitedZero,
        ] {
            assert!(!v.line().is_empty());
            assert!(!v.tag().is_empty());
        }
    }
}
