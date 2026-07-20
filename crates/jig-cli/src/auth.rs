//! `jig auth` — probe and grade the discoverable OAuth conformance of a remote
//! Streamable HTTP MCP server.
//!
//! By default this is a **diagnostic**: it performs no authorization flow, opens
//! no browser, and fabricates no token. It sends one unauthenticated
//! `initialize`, follows the challenge to the RFC 9728 / RFC 8414 metadata, and
//! renders a conformance table plus an overall verdict. See
//! [`jig_core::auth`] for the probe engine and the spec citations.
//!
//! With `--login` it runs the real OAuth 2.1 authorization-code flow instead
//! ([`run_login`]), rendering a numbered flow trace and — the payoff — the
//! result of an authenticated `initialize` + `tools/list`. See
//! [`mod@jig_core::login`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use jig_core::auth::{AuthFinding, AuthReport, Probe, Status, Verdict};
use jig_core::login::{LoginConfig, LoginOutcome, LoginStep, Secret};
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
// `jig auth --login` — the real authorization-code flow
// ---------------------------------------------------------------------------

/// The login-specific flags, bundled so `run_login` keeps a readable signature.
pub(crate) struct LoginFlags {
    /// `--client-id`: a pre-registered client, for an AS without DCR.
    pub client_id: Option<String>,
    /// `--client-secret`: for a confidential pre-registered client.
    pub client_secret: Option<String>,
    /// `--scope`: overrides the PRM's `scopes_supported`.
    pub scope: Option<String>,
    /// `--no-browser`: print the authorization URL, do not launch anything.
    pub no_browser: bool,
    /// `--token-out`: the one and only path by which a token reaches disk.
    pub token_out: Option<PathBuf>,
}

/// Run `jig auth --login`.
pub(crate) async fn run_login(
    target: &Target,
    flags: LoginFlags,
    as_json: bool,
    tap_path: Option<&Path>,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    let url = match target {
        Target::Http { url, .. } => url.as_str(),
        Target::Stdio { .. } => {
            return Err(
                "`jig auth --login` runs an OAuth 2.1 flow, which the MCP spec defines \
                        only for HTTP transports — pass --http <url>. (stdio servers read \
                        credentials from the environment; there is nothing to log in to.)"
                    .to_string(),
            )
        }
    };

    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs));
    let cfg = LoginConfig {
        client_id: flags.client_id,
        client_secret: flags.client_secret.map(Secret::new),
        scope: flags.scope,
        no_browser: flags.no_browser,
        // The browser round trip is a human-speed operation, so it gets the same
        // budget as a request only because `--timeout` is the one knob; a user
        // who needs longer raises it.
        callback_timeout: timeout.unwrap_or(Duration::from_secs(u64::from(u32::MAX))),
        http_timeout: timeout,
    };

    let tap = ProtocolTap::new();
    // Progress goes to stderr so stdout stays exactly the report — `jig auth
    // --login --json | jq` must work while the flow is still printing a URL.
    let progress = |line: &str| {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(line.as_bytes());
        let _ = err.flush();
    };
    let outcome = jig_core::login::login(url, &cfg, &tap, &progress).await;

    if as_json {
        emit(&render_login_json(&outcome));
    } else {
        emit(&render_login_human(&outcome));
    }

    // The token touches disk only here, only on request, and only after the
    // user has been told which file now holds a credential.
    if let (Some(path), Some(token)) = (&flags.token_out, outcome.access_token()) {
        match write_token_file(path, &outcome, token) {
            Ok(note) => eprintln!("jig: {note}"),
            Err(e) => eprintln!("jig: could not write --token-out {}: {e}", path.display()),
        }
    }

    write_tap_if_requested(&tap, tap_path);

    Ok(if outcome.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Write the minted token to `path` as JSON, restricted to the owner where the
/// platform supports it. Returns the warning line to print.
fn write_token_file(
    path: &Path,
    outcome: &LoginOutcome,
    token: &Secret,
) -> std::io::Result<String> {
    let mut doc = serde_json::Map::new();
    doc.insert("access_token".into(), json!(token.expose()));
    doc.insert(
        "token_type".into(),
        json!(outcome
            .token_type
            .clone()
            .unwrap_or_else(|| "Bearer".into())),
    );
    if let Some(e) = outcome.expires_in {
        doc.insert("expires_in".into(), json!(e));
    }
    if let Some(s) = &outcome.granted_scope {
        doc.insert("scope".into(), json!(s));
    }
    if let Some(r) = outcome.refresh_token() {
        doc.insert("refresh_token".into(), json!(r.expose()));
    }
    doc.insert("resource".into(), json!(outcome.canonical_resource));

    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_else(|_| "{}".into())
    );

    // Create the file with owner-only permissions from the outset on Unix —
    // writing first and chmod-ing after leaves a window in which the token is
    // world-readable.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(body.as_bytes())?;
        Ok(format!(
            "WARNING: an access token was written to {} (mode 0600). Anything that can read that \
             file can act as you against this MCP server; delete it when you are done.",
            path.display()
        ))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, body)?;
        Ok(format!(
            "WARNING: an access token was written to {} — this platform has no POSIX mode bits, \
             so the file inherits the directory's ACL. Anything that can read it can act as you \
             against this MCP server; delete it when you are done.",
            path.display()
        ))
    }
}

/// Render the numbered flow trace + the authenticated-probe result. Pure over
/// the outcome, so it is snapshot-lockable.
pub(crate) fn render_login_human(outcome: &LoginOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("jig auth --login · {}\n", outcome.url));
    s.push_str(&format!(
        "resource {} · MCP auth spec {}\n\n",
        outcome.canonical_resource,
        jig_core::auth::MCP_AUTH_SPEC_REVISION
    ));

    s.push_str(&format!("  result: {}\n\n", login_verdict(outcome)));

    s.push_str("Flow trace\n");
    for step in &outcome.steps {
        s.push_str(&login_step_line(step));
    }
    s.push('\n');

    match &outcome.session {
        Some(session) => {
            s.push_str("Authenticated probe\n");
            s.push_str(&format!(
                "  server    {} {} (protocol {})\n",
                session.server_name, session.server_version, session.protocol_version
            ));
            s.push_str(&format!("  tools     {} visible", session.tool_count));
            if !session.tool_names.is_empty() {
                s.push_str(&format!(" — {}", session.tool_names.join(", ")));
            }
            s.push('\n');
            if let Some(scope) = &outcome.granted_scope {
                s.push_str(&format!("  scope     {scope}\n"));
            }
            if let Some(expires) = outcome.expires_in {
                s.push_str(&format!("  expires   in {expires}s\n"));
            }
            s.push('\n');
        }
        None => {
            if let Some(failed) = outcome.failure() {
                s.push_str(&format!(
                    "The flow stopped at step {} ({}). No authenticated session was established.\n\n",
                    failed.n, failed.label
                ));
            }
        }
    }

    s.push_str(&login_footer(outcome));
    s
}

/// The one-line verdict for a login run.
fn login_verdict(outcome: &LoginOutcome) -> String {
    if outcome.succeeded() {
        return "AUTHENTICATED — the OAuth 2.1 authorization-code flow completed and the token \
                opens an MCP session"
            .to_string();
    }
    match outcome.failure() {
        Some(step) => format!(
            "FAILED at step {} of {} — {}",
            step.n,
            outcome.steps.len(),
            step.label
        ),
        None => "INCOMPLETE — the flow ended without establishing a session".to_string(),
    }
}

/// One flow-trace line: number, glyph, status tag, the step's name, what
/// happened, and the clause that required it.
fn login_step_line(step: &LoginStep) -> String {
    format!(
        // 29 is the longest step label ("Authorization Server Metadata"), so the
        // message column lines up for every step.
        "  {:>2} {} {:<4}  {:<29}  {}  [{}]\n",
        step.n,
        step.status.glyph(),
        step.status.tag(),
        step.label,
        step.message,
        step.citation
    )
}

/// The login footer: the secrets posture, and the honest boundary.
fn login_footer(outcome: &LoginOutcome) -> String {
    let mut notes: Vec<String> = vec![
        "Secrets: the access token, refresh token, authorization code and PKCE verifier are not \
         printed here, are not in --json, and are redacted in the tap. `--token-out <file>` is \
         the only way any of them reaches disk."
            .to_string(),
    ];
    if outcome.refresh_token_issued {
        notes.push(
            "A refresh token was issued. jig does not refresh: this command mints one token and \
             proves it. Re-run to get another."
                .to_string(),
        );
    }
    if outcome.failure().is_some() {
        notes.push(
            "Run `jig auth --http <url>` without --login to grade the discoverable auth surface \
             and see which requirement is missing."
                .to_string(),
        );
    }
    notes.join("\n") + "\n"
}

/// Render the machine-readable login report. Structurally incapable of carrying
/// a token: [`LoginOutcome`] does not implement `Serialize`, and every field
/// written here is named individually below.
fn render_login_json(outcome: &LoginOutcome) -> String {
    let steps: Vec<Value> = outcome
        .steps
        .iter()
        .map(|s| {
            json!({
                "n": s.n,
                "key": s.key,
                "label": s.label,
                "status": s.status,
                "message": s.message,
                "citation": s.citation,
            })
        })
        .collect();

    let exchanges: Vec<Value> = outcome
        .exchanges
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();

    let session = match &outcome.session {
        Some(s) => json!({
            "serverName": s.server_name,
            "serverVersion": s.server_version,
            "protocolVersion": s.protocol_version,
            "toolCount": s.tool_count,
            "toolNames": s.tool_names,
        }),
        None => Value::Null,
    };

    let doc = json!({
        "url": outcome.url,
        "canonicalResource": outcome.canonical_resource,
        "mcpAuthSpecRevision": jig_core::auth::MCP_AUTH_SPEC_REVISION,
        "mode": "login",
        "result": if outcome.succeeded() { "authenticated" } else { "failed" },
        "issuer": outcome.issuer,
        "clientId": outcome.client_id,
        "clientRegistered": outcome.client_registered,
        // Everything about the token except the token. There is deliberately no
        // field that could hold one.
        "token": {
            "tokenType": outcome.token_type,
            "expiresIn": outcome.expires_in,
            "scope": outcome.granted_scope,
            "refreshTokenIssued": outcome.refresh_token_issued,
        },
        "steps": steps,
        "session": session,
        "exchanges": exchanges,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    )
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
        "This run probed the discoverable auth surface only. Add --login to run the OAuth 2.1 \
         authorization-code flow for real and prove a token opens a session."
            .to_string(),
    );
    notes.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Compact summary for `jig check` (HTTP targets)
// ---------------------------------------------------------------------------

/// Probe `url` and render a compact, informational auth section for `jig check`.
/// The auth dimension is **not** scored into the rubric-v1 composite in this
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
