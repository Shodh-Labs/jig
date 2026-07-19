//! Shared rendering for the **tool-set advisor** section.
//!
//! Both `jig check` (always) and `jig budget --advise` surface the same
//! deterministic advisories computed by [`jig_core::advisor`]. This module is
//! the single place the finding list is turned into text, so the two commands
//! can never drift on format — and neither re-implements the analysis: it lives
//! entirely in the core `advisor` module.

use jig_core::check::Finding;

/// Render the advisor section from a pre-sorted finding list, or `None` when the
/// list is empty (callers omit the section entirely rather than print a header
/// over nothing). Findings arrive already stably sorted by the core analyzer, so
/// this function neither sorts nor filters — it only formats.
pub(crate) fn render_section(findings: &[Finding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let mut s = String::from("Advisor (tool-set)\n");
    for f in findings {
        s.push_str(&format!("  [{}] {}\n", f.severity.tag(), f.message));
        s.push_str(&format!("    → {}\n", f.fix));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::check::{Dimension, Severity};

    fn finding(sev: Severity, msg: &str, fix: &str) -> Finding {
        Finding {
            dimension: Dimension::ToolSet,
            severity: sev,
            message: msg.to_string(),
            fix: fix.to_string(),
            points: 0.0,
        }
    }

    #[test]
    fn empty_renders_nothing() {
        assert!(render_section(&[]).is_none());
    }

    #[test]
    fn renders_header_and_each_finding() {
        let findings = vec![
            finding(Severity::High, "collision here", "merge them"),
            finding(Severity::Medium, "overlap here", "sharpen it"),
        ];
        let out = render_section(&findings).unwrap();
        assert!(out.starts_with("Advisor (tool-set)\n"));
        assert!(out.contains("  [high] collision here\n    → merge them\n"));
        assert!(out.contains("  [medium] overlap here\n    → sharpen it\n"));
    }
}
