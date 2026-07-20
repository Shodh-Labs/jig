//! `jig auth` — probe and grade the discoverable OAuth conformance of a remote
//! Streamable HTTP MCP server.
//!
//! This is a **diagnostic**, not a login: it performs no authorization flow,
//! opens no browser, and fabricates no token. It sends one unauthenticated
//! `initialize`, follows the challenge to the RFC 9728 / RFC 8414 metadata, and
//! renders a conformance table plus an overall verdict. See
//! [`jig_core::auth`] for the probe engine and the spec citations.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use jig_core::auth::{AuthFinding, AuthReport, Probe, Status, Verdict};
use jig_core::ProtocolTap;
use serde_json::{json, Value};

use crate::{emit, write_tap_if_requested, Target};

/// Run `jig auth`.
pub async fn run(
    target: &Target,
    as_json: bool,
    tap_path: Option<&Path>,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    // Auth probing is an HTTP-transport concern: OAuth is defined by the MCP
    // spec only for HTTP-based transports (stdio servers take credentials from
    // the environment). Reject a stdio target with a clear message.
    let (url, headers) = match target {
        Target::Http { url, headers } => (url.as_str(), headers.as_slice()),
        Target::Stdio { .. } => {
            return Err(
                "`jig auth` probes OAuth conformance, which the MCP spec defines only for \
                        HTTP transports — pass --http <url>. (stdio servers read credentials from \
                        the environment; there is no discoverable auth surface to probe.)"
                    .to_string(),
            );
        }
    };

    let tap = ProtocolTap::new();
    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs));
    let report = jig_core::auth::probe(url, headers, &tap, timeout).await;

    if as_json {
        emit(&render_json(&report));
    } else {
        emit(&render_human(&report, !headers.is_empty()));
    }

    write_tap_if_requested(&tap, tap_path);

    // Exit code mirrors the verdict so `jig auth` is CI-usable: a broken or
    // unreachable auth surface exits nonzero; conformant, open (no auth), and
    // partial-with-passes exit 0 (partial is a warning, not a gate failure).
    let code = match report.verdict {
        Verdict::NonConformant | Verdict::Unreachable => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    };
    Ok(code)
}

// ---------------------------------------------------------------------------
// Human report
// ---------------------------------------------------------------------------

/// Render the conformance table + verdict. Pure over the [`AuthReport`], so it is
/// snapshot-lockable.
pub(crate) fn render_human(report: &AuthReport, header_supplied: bool) -> String {
    let mut s = String::new();

    s.push_str(&format!("jig auth · {}\n", report.url));
    s.push_str(&format!(
        "resource {} · MCP auth spec {}\n\n",
        report.canonical_resource,
        jig_core::auth::MCP_AUTH_SPEC_REVISION
    ));

    s.push_str(&format!("  verdict: {}\n", report.verdict.label()));

    // Tally line.
    s.push_str(&format!(
        "  {} pass · {} fail · {} not-advertised · {} unreachable\n\n",
        report.count(Status::Pass),
        report.count(Status::Fail),
        report.count(Status::NotAdvertised),
        report.count(Status::Unreachable),
    ));

    // One section per probe, in fixed order, listing only probes that ran.
    for probe in [
        Probe::Challenge,
        Probe::ProtectedResourceMetadata,
        Probe::AuthServerMetadata,
        Probe::HeaderPassthrough,
    ] {
        let findings = report.findings_for(probe);
        if findings.is_empty() {
            continue;
        }
        s.push_str(&format!("{}\n", probe.label()));
        for f in findings {
            s.push_str(&finding_line(f));
        }
        s.push('\n');
    }

    // Honest framing + a hint to complete the passthrough probe.
    s.push_str(&footer(report, header_supplied));
    s
}

/// One finding line: glyph, padded status tag, message, and its spec citation.
fn finding_line(f: &AuthFinding) -> String {
    format!(
        "  {} {:<14}  {}  [{}]\n",
        f.status.glyph(),
        f.status.tag(),
        f.message,
        f.citation
    )
}

/// The report footer: what was (not) probed and the honest V1 scope.
fn footer(report: &AuthReport, header_supplied: bool) -> String {
    let mut notes: Vec<String> = Vec::new();
    if report.auth_required && !header_supplied {
        notes.push(
            "Pass --header \"Authorization: Bearer <token>\" to also test that a real token is \
             accepted (header passthrough)."
                .to_string(),
        );
    }
    notes.push(
        "jig auth probes discoverable auth surfaces; it performs no login flow (roadmap)."
            .to_string(),
    );
    notes.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Compact summary for `jig check` (HTTP targets)
// ---------------------------------------------------------------------------

/// Probe `url` and render a compact, informational auth section for `jig check`.
/// The auth dimension is **not** scored into the rubric-v1.1 composite in this
/// milestone — this is a heads-up section only. Shares `tap` so the probe's HTTP
/// traffic is captured alongside the rest of the session.
pub(crate) async fn check_summary(
    url: &str,
    headers: &[(String, String)],
    tap: &ProtocolTap,
    timeout_secs: u64,
) -> String {
    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs));
    let report = jig_core::auth::probe(url, headers, tap, timeout).await;
    render_check_summary(&report)
}

/// Render the compact auth section from a report. Pure, so it is testable.
pub(crate) fn render_check_summary(report: &AuthReport) -> String {
    let mut s = String::new();
    s.push_str("\nAuth (informational — not scored into the grade)\n");
    s.push_str(&format!("  verdict: {}\n", report.verdict.label()));
    if report.auth_required {
        s.push_str(&format!(
            "  {} pass · {} fail · {} not-advertised · {} unreachable\n",
            report.count(Status::Pass),
            report.count(Status::Fail),
            report.count(Status::NotAdvertised),
            report.count(Status::Unreachable),
        ));
    }
    s.push_str("  run `jig auth --http <url>` for the full OAuth conformance table\n");
    s
}

// ---------------------------------------------------------------------------
// JSON report
// ---------------------------------------------------------------------------

/// Render the full machine-readable report: every finding and every captured
/// HTTP exchange (tokens redacted).
fn render_json(report: &AuthReport) -> String {
    let findings: Vec<Value> = report
        .findings
        .iter()
        .map(|f| {
            json!({
                "key": f.key,
                "probe": f.probe,
                "status": f.status,
                "message": f.message,
                "citation": f.citation,
            })
        })
        .collect();

    let exchanges: Vec<Value> = report
        .exchanges
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();

    let doc = json!({
        "url": report.url,
        "canonicalResource": report.canonical_resource,
        "mcpAuthSpecRevision": jig_core::auth::MCP_AUTH_SPEC_REVISION,
        "authRequired": report.auth_required,
        "verdict": report.verdict.tag(),
        "summary": {
            "pass": report.count(Status::Pass),
            "fail": report.count(Status::Fail),
            "notAdvertised": report.count(Status::NotAdvertised),
            "unreachable": report.count(Status::Unreachable),
            "info": report.count(Status::Info),
        },
        "findings": findings,
        "exchanges": exchanges,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::auth::{HttpExchange, Probe, Status};

    fn finding(key: &'static str, probe: Probe, status: Status, msg: &str) -> AuthFinding {
        AuthFinding {
            key,
            probe,
            status,
            message: msg.to_string(),
            citation: "RFC 9728 §5.1",
        }
    }

    fn well_configured() -> AuthReport {
        AuthReport {
            url: "http://127.0.0.1:PORT/mcp".to_string(),
            canonical_resource: "http://127.0.0.1:PORT/mcp".to_string(),
            auth_required: true,
            verdict: Verdict::Conformant,
            findings: vec![
                finding(
                    "unauth_post",
                    Probe::Challenge,
                    Status::Pass,
                    "unauthenticated initialize returned HTTP 401 Unauthorized",
                ),
                finding(
                    "www_auth_resource_metadata",
                    Probe::Challenge,
                    Status::Pass,
                    "challenge advertises `resource_metadata=…`",
                ),
                finding(
                    "prm_resource_audience",
                    Probe::ProtectedResourceMetadata,
                    Status::Pass,
                    "`resource` matches the probed server (audience binding)",
                ),
                finding(
                    "asm_pkce_s256",
                    Probe::AuthServerMetadata,
                    Status::Pass,
                    "advertises PKCE `S256` in code_challenge_methods_supported",
                ),
            ],
            exchanges: vec![HttpExchange {
                label: "unauthenticated initialize".to_string(),
                method: "POST".to_string(),
                url: "http://127.0.0.1:PORT/mcp".to_string(),
                request_headers: vec![("Accept".to_string(), "application/json".to_string())],
                status: Some(401),
                response_headers: vec![(
                    "WWW-Authenticate".to_string(),
                    "Bearer resource_metadata=\"…\"".to_string(),
                )],
                body: None,
                error: None,
            }],
        }
    }

    fn broken_no_challenge() -> AuthReport {
        AuthReport {
            url: "http://127.0.0.1:PORT/mcp".to_string(),
            canonical_resource: "http://127.0.0.1:PORT/mcp".to_string(),
            auth_required: true,
            verdict: Verdict::NonConformant,
            findings: vec![
                finding(
                    "unauth_post",
                    Probe::Challenge,
                    Status::Pass,
                    "unauthenticated initialize returned HTTP 401 Unauthorized",
                ),
                finding(
                    "www_authenticate",
                    Probe::Challenge,
                    Status::Fail,
                    "401 carried no `WWW-Authenticate` header",
                ),
                finding(
                    "prm_reachable",
                    Probe::ProtectedResourceMetadata,
                    Status::NotAdvertised,
                    "no protected-resource metadata at the RFC 9728 well-known locations",
                ),
            ],
            exchanges: vec![],
        }
    }

    #[test]
    fn well_configured_human_snapshot() {
        insta::assert_snapshot!(
            "auth_human_well_configured",
            render_human(&well_configured(), false)
        );
    }

    #[test]
    fn broken_human_snapshot() {
        insta::assert_snapshot!(
            "auth_human_no_challenge",
            render_human(&broken_no_challenge(), false)
        );
    }

    #[test]
    fn json_report_is_valid_and_redacts() {
        let doc = render_json(&well_configured());
        let v: Value = serde_json::from_str(&doc).expect("valid JSON");
        assert_eq!(v["verdict"], "conformant");
        assert_eq!(v["authRequired"], true);
        assert_eq!(v["summary"]["pass"], 4);
        assert!(v["findings"].as_array().unwrap().len() >= 4);
        assert_eq!(v["mcpAuthSpecRevision"], "2025-06-18");
    }

    #[test]
    fn footer_hints_passthrough_when_no_header() {
        let out = render_human(&well_configured(), false);
        assert!(out.contains("header passthrough"));
        let out2 = render_human(&well_configured(), true);
        assert!(!out2.contains("header passthrough"));
    }
}
