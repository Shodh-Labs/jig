//! `jig auth` — **deterministic OAuth conformance probing** for Streamable HTTP
//! MCP servers.
//!
//! This module performs no authorization flows, opens no browser, and needs no
//! credentials. It probes the *discoverable* auth surfaces of a remote MCP
//! server and grades their spec conformance — everything checkable, nothing
//! probabilistic. That is Jig's house style: an instrument, not a guess.
//!
//! Running the flow for real — Dynamic Client Registration, PKCE, the browser
//! round trip, the token exchange, and an authenticated session to prove the
//! token — lives next door in [`mod@crate::login`], which reuses this module's
//! well-known URL builders, metadata parsers, and redacted HTTP recorder so the
//! two can never disagree about where a document lives or what it says.
//!
//! # What it checks (each probe → a typed [`AuthFinding`])
//!
//! 1. **Unauthenticated POST** ([`Probe::Challenge`]) — send `initialize` with no
//!    credentials. A `401` is graded against RFC 6750 / RFC 9728 §5.1 (is there a
//!    `WWW-Authenticate`? scheme `Bearer`? does it carry a `resource_metadata`
//!    URL, as the MCP spec requires?). A `200` is itself a finding: the server
//!    requires no auth (informational — not a failure).
//! 2. **Protected Resource Metadata** ([`Probe::ProtectedResourceMetadata`],
//!    RFC 9728) — `GET .well-known/oauth-protected-resource` (both the root and
//!    the path-appended form). Parse `resource`, `authorization_servers`,
//!    `scopes_supported`, `bearer_methods_supported`, and validate that
//!    `resource` matches the target URL (RFC 8707 audience-confusion check).
//! 3. **Authorization Server Metadata** ([`Probe::AuthServerMetadata`], RFC 8414)
//!    — for each advertised auth server, `GET
//!    .well-known/oauth-authorization-server` (with the OIDC
//!    `.well-known/openid-configuration` fallback). Grade PKCE `S256` (the MCP
//!    spec **requires** it), Dynamic Client Registration (RFC 7591), the
//!    authorization/token endpoints, and the RFC 9207 `iss` parameter.
//! 4. **Header passthrough** ([`Probe::HeaderPassthrough`]) — only when the user
//!    supplies an `Authorization` header: does `initialize` succeed where the
//!    bare probe got `401`? Jig never fabricates a token.
//!
//! Every HTTP exchange is captured (tokens redacted) for `--json` and recorded to
//! the protocol tap.
//!
//! # Purity
//!
//! The wire I/O lives in [`probe`]; every *grading* rule is a pure function of
//! already-parsed data ([`WwwAuthenticate`], [`ProtectedResourceMetadata`],
//! [`AuthServerMetadata`]) so each is unit-testable and the parsers can be
//! property-tested to never panic on arbitrary JSON.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::tap::{Direction, ProtocolTap};

/// The MCP specification revision whose authorization model this prober grades
/// against (Protected Resource Metadata / Authorization Server split).
pub const MCP_AUTH_SPEC_REVISION: &str = "2025-06-18";

/// The `.well-known` suffix for OAuth 2.0 Protected Resource Metadata (RFC 9728).
const WELL_KNOWN_PRM: &str = "/.well-known/oauth-protected-resource";
/// The `.well-known` suffix for OAuth 2.0 Authorization Server Metadata (RFC 8414).
const WELL_KNOWN_ASM: &str = "/.well-known/oauth-authorization-server";
/// The OpenID Connect discovery suffix (RFC 8414 §5 fallback).
const WELL_KNOWN_OIDC: &str = "/.well-known/openid-configuration";

/// How much of an HTTP body to capture for the `--json` record and the tap.
const BODY_CAPTURE_LEN: usize = 8 * 1024;

/// The request body of a probe request. Both variants are recorded to the tap
/// with their secret-bearing members redacted; see [`redact_json`] and
/// [`redact_form`].
pub(crate) enum RequestBody {
    /// A JSON body (`application/json`).
    Json(Value),
    /// An `application/x-www-form-urlencoded` body — the OAuth token,
    /// registration-error, and revocation shapes.
    Form(Vec<(String, String)>),
}

// ---------------------------------------------------------------------------
// Finding data model
// ---------------------------------------------------------------------------

/// The outcome of a single conformance check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// The surface exists and conforms.
    Pass,
    /// The surface exists but violates a normative requirement (a `MUST`).
    Fail,
    /// An optional/recommended surface (a `SHOULD`) was not advertised.
    NotAdvertised,
    /// The surface could not be reached (network/HTTP error, or a referenced
    /// document that 404s).
    Unreachable,
    /// Informational — reported, never counted against conformance.
    Info,
}

impl Status {
    /// A short glyph for the human table.
    pub fn glyph(self) -> char {
        match self {
            Status::Pass => '✓',
            Status::Fail => '✗',
            Status::NotAdvertised => '·',
            Status::Unreachable => '?',
            Status::Info => 'i',
        }
    }

    /// A short uppercase tag (`PASS`, `FAIL`, …).
    pub fn tag(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::NotAdvertised => "NOT-ADVERTISED",
            Status::Unreachable => "UNREACHABLE",
            Status::Info => "INFO",
        }
    }
}

/// Which probe a finding belongs to (the conformance-table grouping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Probe {
    /// The unauthenticated-request challenge (RFC 6750 / RFC 9728 §5.1).
    Challenge,
    /// Protected Resource Metadata (RFC 9728).
    ProtectedResourceMetadata,
    /// Authorization Server Metadata (RFC 8414).
    AuthServerMetadata,
    /// Header passthrough with a user-supplied token.
    HeaderPassthrough,
}

impl Probe {
    /// A human label for the table section header.
    pub fn label(self) -> &'static str {
        match self {
            Probe::Challenge => "Unauthenticated challenge",
            Probe::ProtectedResourceMetadata => "Protected Resource Metadata",
            Probe::AuthServerMetadata => "Authorization Server Metadata",
            Probe::HeaderPassthrough => "Header passthrough",
        }
    }
}

/// One graded conformance finding. The `message` always names the concrete
/// observation; the `citation` is the spec section that defines the requirement.
#[derive(Debug, Clone, Serialize)]
pub struct AuthFinding {
    /// A stable machine key (`unauth_post`, `pkce_s256`, …).
    pub key: &'static str,
    /// The probe this finding belongs to.
    pub probe: Probe,
    /// The graded outcome.
    pub status: Status,
    /// What was observed, in one line.
    pub message: String,
    /// The spec section the requirement comes from (e.g. `RFC 9728 §5.1`).
    pub citation: &'static str,
}

impl AuthFinding {
    fn new(
        key: &'static str,
        probe: Probe,
        status: Status,
        message: impl Into<String>,
        citation: &'static str,
    ) -> Self {
        AuthFinding {
            key,
            probe,
            status,
            message: message.into(),
            citation,
        }
    }
}

/// The overall auth verdict for a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Every graded requirement passed.
    Conformant,
    /// A mix of passing and failing requirements.
    PartiallyConformant,
    /// No graded requirement passed (the auth surface is broken or absent).
    NonConformant,
    /// The server required no authentication (a `200` to the bare probe).
    NoAuth,
    /// The server could not be reached at all.
    Unreachable,
}

impl Verdict {
    /// A one-line human verdict.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Conformant => "CONFORMANT — the discoverable OAuth surface matches the spec",
            Verdict::PartiallyConformant => {
                "PARTIALLY CONFORMANT — some auth surfaces are missing or non-conformant"
            }
            Verdict::NonConformant => {
                "NON-CONFORMANT — the server challenges for auth but the OAuth surface is broken"
            }
            Verdict::NoAuth => "NO AUTH — the server accepted an unauthenticated request (open)",
            Verdict::Unreachable => "UNREACHABLE — could not probe the endpoint",
        }
    }

    /// A short machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Verdict::Conformant => "conformant",
            Verdict::PartiallyConformant => "partially-conformant",
            Verdict::NonConformant => "non-conformant",
            Verdict::NoAuth => "no-auth",
            Verdict::Unreachable => "unreachable",
        }
    }
}

/// One captured HTTP exchange, with secrets redacted, for the `--json` record.
#[derive(Debug, Clone, Serialize)]
pub struct HttpExchange {
    /// A human label (`unauthenticated initialize`, `GET protected-resource-metadata`).
    pub label: String,
    /// The HTTP method.
    pub method: String,
    /// The absolute URL.
    pub url: String,
    /// Request headers actually sent (auth values redacted).
    pub request_headers: Vec<(String, String)>,
    /// The HTTP status code, or `None` if the request never completed.
    pub status: Option<u16>,
    /// Response headers of interest (`WWW-Authenticate`, `Content-Type`).
    pub response_headers: Vec<(String, String)>,
    /// The (truncated, redacted) response body, if any.
    pub body: Option<String>,
    /// A transport error message, if the request failed to complete.
    pub error: Option<String>,
}

/// The complete result of an auth probe.
#[derive(Debug, Clone)]
pub struct AuthReport {
    /// The MCP endpoint URL probed.
    pub url: String,
    /// The canonical resource identifier derived from the URL (RFC 8707 §2).
    pub canonical_resource: String,
    /// Whether the server required authentication (a `401` to the bare probe).
    /// `false` means the server answered `200` — an open server.
    pub auth_required: bool,
    /// The overall verdict.
    pub verdict: Verdict,
    /// Every graded finding, in probe order.
    pub findings: Vec<AuthFinding>,
    /// Every HTTP exchange performed (redacted), for `--json` and the tap.
    pub exchanges: Vec<HttpExchange>,
}

impl AuthReport {
    /// Findings for one probe, in order.
    pub fn findings_for(&self, probe: Probe) -> Vec<&AuthFinding> {
        self.findings.iter().filter(|f| f.probe == probe).collect()
    }

    /// Count of findings with a given status.
    pub fn count(&self, status: Status) -> usize {
        self.findings.iter().filter(|f| f.status == status).count()
    }
}

// ---------------------------------------------------------------------------
// Parsed metadata (pure)
// ---------------------------------------------------------------------------

/// A parsed `WWW-Authenticate` challenge (RFC 6750 §3 / RFC 7235).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WwwAuthenticate {
    /// The auth-scheme token (e.g. `Bearer`), as sent (case preserved).
    pub scheme: String,
    /// The `key=value` auth-params (values unquoted).
    pub params: Vec<(String, String)>,
}

impl WwwAuthenticate {
    /// Parse a single `WWW-Authenticate` header value.
    ///
    /// Handles the common `Bearer key="value", key2="value2"` shape used by MCP
    /// servers. Total over arbitrary input: any string yields either `Some`
    /// challenge or `None` (empty/opaque) — never a panic.
    pub fn parse(raw: &str) -> Option<WwwAuthenticate> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        // The scheme is the first whitespace-delimited token.
        let (scheme, rest) = match raw.split_once(char::is_whitespace) {
            Some((s, r)) => (s.to_string(), r.trim()),
            None => (raw.to_string(), ""),
        };
        let params = parse_auth_params(rest);
        Some(WwwAuthenticate { scheme, params })
    }

    /// Whether the scheme is `Bearer` (case-insensitive, per RFC 7235).
    pub fn is_bearer(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("Bearer")
    }

    /// The value of an auth-param, matched case-insensitively.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse `key="value", key2=value2` auth-params, tolerating quoted and bare
/// values and arbitrary whitespace. Never panics.
fn parse_auth_params(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Split on commas that are not inside quotes.
    let mut in_quotes = false;
    let mut current = String::new();
    let mut segments = Vec::new();
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    for seg in segments {
        if let Some((k, v)) = seg.split_once('=') {
            let key = k.trim().to_string();
            let value = v.trim().trim_matches('"').to_string();
            if !key.is_empty() {
                out.push((key, value));
            }
        }
    }
    out
}

/// Parsed OAuth 2.0 Protected Resource Metadata (RFC 9728 §2).
#[derive(Debug, Clone, Default)]
pub struct ProtectedResourceMetadata {
    /// `resource` — REQUIRED. The protected resource's identifier.
    pub resource: Option<String>,
    /// `authorization_servers` — the advertised AS issuer identifiers.
    pub authorization_servers: Vec<String>,
    /// `scopes_supported` — RECOMMENDED.
    pub scopes_supported: Vec<String>,
    /// `bearer_methods_supported` — how tokens may be presented.
    pub bearer_methods_supported: Vec<String>,
}

impl ProtectedResourceMetadata {
    /// Parse leniently from a JSON value. Never panics on arbitrary input; any
    /// absent or mistyped field is simply left empty.
    pub fn from_json(v: &Value) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: string_field(v, "resource"),
            authorization_servers: string_array(v, "authorization_servers"),
            scopes_supported: string_array(v, "scopes_supported"),
            bearer_methods_supported: string_array(v, "bearer_methods_supported"),
        }
    }
}

/// Parsed OAuth 2.0 Authorization Server Metadata (RFC 8414 §2), with the fields
/// the MCP spec cares about.
#[derive(Debug, Clone, Default)]
pub struct AuthServerMetadata {
    /// `issuer` — REQUIRED.
    pub issuer: Option<String>,
    /// `authorization_endpoint`.
    pub authorization_endpoint: Option<String>,
    /// `token_endpoint`.
    pub token_endpoint: Option<String>,
    /// `registration_endpoint` — the RFC 7591 DCR endpoint.
    pub registration_endpoint: Option<String>,
    /// `code_challenge_methods_supported` — the PKCE methods.
    pub code_challenge_methods_supported: Vec<String>,
    /// `authorization_response_iss_parameter_supported` (RFC 9207).
    pub iss_parameter_supported: bool,
}

impl AuthServerMetadata {
    /// Parse leniently from a JSON value. Never panics on arbitrary input.
    pub fn from_json(v: &Value) -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: string_field(v, "issuer"),
            authorization_endpoint: string_field(v, "authorization_endpoint"),
            token_endpoint: string_field(v, "token_endpoint"),
            registration_endpoint: string_field(v, "registration_endpoint"),
            code_challenge_methods_supported: string_array(v, "code_challenge_methods_supported"),
            iss_parameter_supported: v
                .get("authorization_response_iss_parameter_supported")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Whether PKCE `S256` is advertised (case-insensitive).
    pub fn supports_s256(&self) -> bool {
        self.code_challenge_methods_supported
            .iter()
            .any(|m| m.eq_ignore_ascii_case("S256"))
    }
}

/// A string field, if present and a string.
pub(crate) fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A JSON array of strings, dropping any non-string entries. `[]` if absent.
pub(crate) fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// URL construction (pure)
// ---------------------------------------------------------------------------

/// The canonical resource identifier for an MCP endpoint URL, per RFC 8707 §2 and
/// the MCP spec: lowercase scheme/host, no fragment, and no gratuitous trailing
/// slash. Returns the input unchanged if it does not parse as a URL.
pub fn canonical_resource_uri(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return url.to_string();
    };
    parsed.set_fragment(None);
    let mut out = parsed.to_string();
    // Drop a lone trailing slash: the MCP spec prefers the form without it
    // ("https://host/" -> "https://host", "https://host/mcp/" -> "https://host/mcp").
    if out.ends_with('/') {
        out.pop();
    }
    out
}

/// The candidate Protected Resource Metadata URLs for a target, per RFC 9728 §3
/// and the MCP spec: the path-appended form first (most specific), then the
/// root form. An explicit `resource_metadata` URL from the challenge should be
/// preferred over both.
pub fn protected_resource_metadata_urls(target: &str) -> Vec<String> {
    let Ok(parsed) = reqwest::Url::parse(target) else {
        return Vec::new();
    };
    let origin = origin_string(&parsed);
    let path = parsed.path().trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        // Path-appended: insert the well-known between host and path.
        urls.push(format!("{origin}{WELL_KNOWN_PRM}{path}"));
    }
    // Root form.
    let root = format!("{origin}{WELL_KNOWN_PRM}");
    if !urls.contains(&root) {
        urls.push(root);
    }
    urls
}

/// The candidate Authorization Server Metadata URLs for an issuer, per RFC 8414
/// §3 (well-known inserted before the path) with the OIDC discovery fallback
/// (appended to the end).
pub fn auth_server_metadata_urls(issuer: &str) -> Vec<String> {
    let Ok(parsed) = reqwest::Url::parse(issuer) else {
        return Vec::new();
    };
    let origin = origin_string(&parsed);
    let path = parsed.path().trim_end_matches('/');
    let mut urls = Vec::new();
    // RFC 8414: /.well-known/oauth-authorization-server{path}
    urls.push(format!("{origin}{WELL_KNOWN_ASM}{path}"));
    // OIDC: {issuer}/.well-known/openid-configuration (appended to the end).
    let oidc_appended = format!("{origin}{path}{WELL_KNOWN_OIDC}");
    push_unique(&mut urls, oidc_appended);
    // RFC 8414 OIDC form: /.well-known/openid-configuration{path}
    push_unique(&mut urls, format!("{origin}{WELL_KNOWN_OIDC}{path}"));
    urls
}

/// `scheme://host[:port]` for a URL (its origin, without path).
fn origin_string(u: &reqwest::Url) -> String {
    let scheme = u.scheme();
    let host = u.host_str().unwrap_or("");
    match u.port() {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    }
}

pub(crate) fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

// ---------------------------------------------------------------------------
// The probe (impure: performs HTTP, records to the tap)
// ---------------------------------------------------------------------------

/// The response of one probe request, distilled to what grading needs.
pub(crate) struct FetchResult {
    /// The HTTP status, or `None` if the request never completed.
    pub(crate) status: Option<u16>,
    /// The `WWW-Authenticate` header value, if any.
    pub(crate) www_authenticate: Option<String>,
    /// The parsed response body **with secrets redacted** — safe to log, but
    /// therefore useless for reading a token out of. Use [`FetchResult::raw_json`]
    /// for that, and never let its contents escape into a record.
    pub(crate) json: Option<Value>,
    /// The parsed response body as received, secrets intact. Only the token and
    /// registration steps look at this, and only to pull out the credential.
    pub(crate) raw_json: Option<Value>,
    /// A transport error message, if the request failed to complete.
    pub(crate) error: Option<String>,
}

/// Header names whose values are secrets and must be redacted in any record.
fn is_secret_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

/// Redact a header value if the header carries a secret.
fn redact_header(name: &str, value: &str) -> String {
    if is_secret_header(name) {
        // Preserve the scheme token (e.g. "Bearer") but hide the credential.
        match value.split_once(char::is_whitespace) {
            Some((scheme, _)) => format!("{scheme} <redacted>"),
            None => "<redacted>".to_string(),
        }
    } else {
        value.to_string()
    }
}

/// Probe the discoverable auth surfaces of an MCP endpoint. Always returns a
/// report: an unreachable endpoint is itself a finding, not an error.
///
/// `headers` are any user-supplied extra headers (e.g. `Authorization: Bearer
/// …`); they are sent on the *passthrough* probe only, never fabricated. Every
/// exchange is recorded to `tap`.
pub async fn probe(
    url: &str,
    headers: &[(String, String)],
    tap: &ProtocolTap,
    timeout: Option<Duration>,
) -> AuthReport {
    let client = reqwest::Client::builder().build();
    let canonical_resource = canonical_resource_uri(url);

    let mut findings = Vec::new();
    let mut exchanges = Vec::new();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            findings.push(AuthFinding::new(
                "http_client",
                Probe::Challenge,
                Status::Unreachable,
                format!("could not build an HTTP client: {e}"),
                "n/a",
            ));
            return AuthReport {
                url: url.to_string(),
                canonical_resource,
                auth_required: false,
                verdict: Verdict::Unreachable,
                findings,
                exchanges,
            };
        }
    };

    // ---- Probe 1: unauthenticated initialize ------------------------------
    let init_body = initialize_body();
    let bare = fetch(
        &client,
        tap,
        &mut exchanges,
        "unauthenticated initialize",
        reqwest::Method::POST,
        url,
        &[],
        Some(RequestBody::Json(init_body.clone())),
        timeout,
    )
    .await;

    let status = match bare.status {
        Some(s) => s,
        None => {
            findings.push(AuthFinding::new(
                "unauth_post",
                Probe::Challenge,
                Status::Unreachable,
                "the endpoint could not be reached for an unauthenticated request",
                "MCP 2025-06-18 Authorization",
            ));
            return AuthReport {
                url: url.to_string(),
                canonical_resource,
                auth_required: false,
                verdict: Verdict::Unreachable,
                findings,
                exchanges,
            };
        }
    };

    // A 2xx means the server required no auth — a finding, not a failure.
    if (200..300).contains(&status) {
        findings.push(AuthFinding::new(
            "unauth_post",
            Probe::Challenge,
            Status::Info,
            format!(
                "unauthenticated initialize returned HTTP {status} — the server requires no \
                 authentication (auth is OPTIONAL in MCP)"
            ),
            "MCP 2025-06-18 Authorization",
        ));
        return AuthReport {
            url: url.to_string(),
            canonical_resource,
            auth_required: false,
            verdict: Verdict::NoAuth,
            findings,
            exchanges,
        };
    }

    // From here the server is treated as requiring auth.
    let auth_required = true;

    // Grade the challenge itself (RFC 6750 / RFC 9728 §5.1).
    if status == 401 {
        findings.push(AuthFinding::new(
            "unauth_post",
            Probe::Challenge,
            Status::Pass,
            "unauthenticated initialize returned HTTP 401 Unauthorized",
            "MCP 2025-06-18 · RFC 6750 §3",
        ));
    } else {
        findings.push(AuthFinding::new(
            "unauth_post",
            Probe::Challenge,
            Status::Fail,
            format!(
                "unauthenticated initialize returned HTTP {status}; the spec requires 401 to \
                 challenge for authorization"
            ),
            "MCP 2025-06-18 · RFC 6750 §3",
        ));
    }

    let challenge = bare
        .www_authenticate
        .as_deref()
        .and_then(WwwAuthenticate::parse);
    let mut explicit_prm: Option<String> = None;
    match &challenge {
        Some(ch) => {
            findings.push(AuthFinding::new(
                "www_authenticate",
                Probe::Challenge,
                Status::Pass,
                format!("WWW-Authenticate header present (scheme `{}`)", ch.scheme),
                "RFC 9728 §5.1 · RFC 6750 §3",
            ));
            if ch.is_bearer() {
                findings.push(AuthFinding::new(
                    "www_auth_bearer",
                    Probe::Challenge,
                    Status::Pass,
                    "challenge uses the `Bearer` auth-scheme",
                    "RFC 6750 §3",
                ));
            } else {
                findings.push(AuthFinding::new(
                    "www_auth_bearer",
                    Probe::Challenge,
                    Status::Fail,
                    format!(
                        "challenge scheme is `{}`, not `Bearer`; MCP access tokens are Bearer \
                         tokens",
                        ch.scheme
                    ),
                    "RFC 6750 §3",
                ));
            }
            match ch.param("resource_metadata") {
                Some(rm) if !rm.is_empty() => {
                    explicit_prm = Some(rm.to_string());
                    findings.push(AuthFinding::new(
                        "www_auth_resource_metadata",
                        Probe::Challenge,
                        Status::Pass,
                        format!("challenge advertises `resource_metadata={rm}`"),
                        "RFC 9728 §5.1",
                    ));
                }
                _ => {
                    findings.push(AuthFinding::new(
                        "www_auth_resource_metadata",
                        Probe::Challenge,
                        Status::Fail,
                        "challenge carries no `resource_metadata` parameter; MCP requires the 401 \
                         to point clients at the protected-resource metadata",
                        "RFC 9728 §5.1",
                    ));
                }
            }
        }
        None => {
            findings.push(AuthFinding::new(
                "www_authenticate",
                Probe::Challenge,
                Status::Fail,
                "401 carried no `WWW-Authenticate` header; a compliant server MUST send one to \
                 locate its resource metadata",
                "RFC 9728 §5.1 · RFC 6750 §3",
            ));
        }
    }

    // ---- Probe 2: Protected Resource Metadata (RFC 9728) ------------------
    let mut prm_candidates: Vec<String> = Vec::new();
    if let Some(rm) = &explicit_prm {
        prm_candidates.push(rm.clone());
    }
    for u in protected_resource_metadata_urls(url) {
        push_unique(&mut prm_candidates, u);
    }

    let mut prm: Option<ProtectedResourceMetadata> = None;
    let mut prm_source: Option<String> = None;
    for candidate in &prm_candidates {
        let res = fetch(
            &client,
            tap,
            &mut exchanges,
            "GET protected-resource-metadata",
            reqwest::Method::GET,
            candidate,
            &[],
            None,
            timeout,
        )
        .await;
        if let (Some(200..=299), Some(v)) = (res.status, &res.json) {
            prm = Some(ProtectedResourceMetadata::from_json(v));
            prm_source = Some(candidate.clone());
            break;
        }
    }

    match &prm {
        Some(meta) => {
            let source = prm_source.clone().unwrap_or_default();
            findings.push(AuthFinding::new(
                "prm_reachable",
                Probe::ProtectedResourceMetadata,
                Status::Pass,
                format!("fetched protected-resource metadata from {source}"),
                "RFC 9728 §3",
            ));
            grade_prm(meta, &canonical_resource, &mut findings);
        }
        None => {
            // Distinguish "the challenge pointed nowhere" from "never advertised".
            let (status, msg) = if explicit_prm.is_some() {
                (
                    Status::Fail,
                    format!(
                        "the advertised `resource_metadata` URL ({}) did not return usable RFC \
                         9728 metadata",
                        explicit_prm.clone().unwrap_or_default()
                    ),
                )
            } else {
                (
                    Status::NotAdvertised,
                    "no protected-resource metadata at the RFC 9728 well-known locations"
                        .to_string(),
                )
            };
            findings.push(AuthFinding::new(
                "prm_reachable",
                Probe::ProtectedResourceMetadata,
                status,
                msg,
                "RFC 9728 §3",
            ));
        }
    }

    // ---- Probe 3: Authorization Server Metadata (RFC 8414) ----------------
    let auth_servers = prm
        .as_ref()
        .map(|m| m.authorization_servers.clone())
        .unwrap_or_default();

    if auth_servers.is_empty() {
        findings.push(AuthFinding::new(
            "asm_reachable",
            Probe::AuthServerMetadata,
            Status::NotAdvertised,
            "no authorization server was advertised, so its metadata cannot be graded",
            "RFC 8414 §3",
        ));
    } else {
        // Grade the first advertised authorization server (the common case).
        let issuer = &auth_servers[0];
        let mut asm: Option<AuthServerMetadata> = None;
        let mut asm_source: Option<String> = None;
        for candidate in auth_server_metadata_urls(issuer) {
            let res = fetch(
                &client,
                tap,
                &mut exchanges,
                "GET authorization-server-metadata",
                reqwest::Method::GET,
                &candidate,
                &[],
                None,
                timeout,
            )
            .await;
            if let (Some(200..=299), Some(v)) = (res.status, &res.json) {
                asm = Some(AuthServerMetadata::from_json(v));
                asm_source = Some(candidate.clone());
                break;
            }
        }
        match &asm {
            Some(meta) => {
                findings.push(AuthFinding::new(
                    "asm_reachable",
                    Probe::AuthServerMetadata,
                    Status::Pass,
                    format!(
                        "fetched authorization-server metadata for {issuer} from {}",
                        asm_source.clone().unwrap_or_default()
                    ),
                    "RFC 8414 §3",
                ));
                grade_asm(meta, &mut findings);
            }
            None => {
                findings.push(AuthFinding::new(
                    "asm_reachable",
                    Probe::AuthServerMetadata,
                    Status::Unreachable,
                    format!(
                        "the advertised authorization server ({issuer}) served no RFC 8414 / OIDC \
                         metadata at its well-known locations"
                    ),
                    "RFC 8414 §3",
                ));
            }
        }
    }

    // ---- Probe 4: header passthrough (only with a user-supplied token) ----
    let has_authorization = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    if has_authorization {
        let authed = fetch(
            &client,
            tap,
            &mut exchanges,
            "authenticated initialize",
            reqwest::Method::POST,
            url,
            headers,
            Some(RequestBody::Json(init_body)),
            timeout,
        )
        .await;
        match authed.status {
            Some(s) if (200..300).contains(&s) => findings.push(AuthFinding::new(
                "header_passthrough",
                Probe::HeaderPassthrough,
                Status::Pass,
                format!(
                    "initialize with the supplied Authorization header returned HTTP {s} where \
                     the bare probe got {status}"
                ),
                "RFC 6750 §2.1",
            )),
            Some(s) => findings.push(AuthFinding::new(
                "header_passthrough",
                Probe::HeaderPassthrough,
                Status::Fail,
                format!(
                    "initialize with the supplied Authorization header still returned HTTP {s} — \
                     the token was rejected (expired, wrong audience, or wrong scheme)"
                ),
                "RFC 6750 §2.1",
            )),
            None => findings.push(AuthFinding::new(
                "header_passthrough",
                Probe::HeaderPassthrough,
                Status::Unreachable,
                "the authenticated request could not be completed",
                "RFC 6750 §2.1",
            )),
        }
    }

    let verdict = compute_verdict(auth_required, &findings);

    AuthReport {
        url: url.to_string(),
        canonical_resource,
        auth_required,
        verdict,
        findings,
        exchanges,
    }
}

/// Grade a parsed Protected Resource Metadata document (RFC 9728 / RFC 8707).
fn grade_prm(
    meta: &ProtectedResourceMetadata,
    canonical_resource: &str,
    findings: &mut Vec<AuthFinding>,
) {
    match &meta.resource {
        Some(res) => {
            // RFC 8707 audience-confusion check: the advertised `resource` MUST
            // identify this server.
            if resource_matches(res, canonical_resource) {
                findings.push(AuthFinding::new(
                    "prm_resource_audience",
                    Probe::ProtectedResourceMetadata,
                    Status::Pass,
                    format!("`resource` ({res}) matches the probed server (audience binding)"),
                    "RFC 9728 §2 · RFC 8707 §2",
                ));
            } else {
                findings.push(AuthFinding::new(
                    "prm_resource_audience",
                    Probe::ProtectedResourceMetadata,
                    Status::Fail,
                    format!(
                        "`resource` is `{res}` but the probed server is `{canonical_resource}` — \
                         an audience mismatch invites token-confusion attacks"
                    ),
                    "RFC 9728 §2 · RFC 8707 §2",
                ));
            }
        }
        None => findings.push(AuthFinding::new(
            "prm_resource_audience",
            Probe::ProtectedResourceMetadata,
            Status::Fail,
            "protected-resource metadata omits the REQUIRED `resource` field",
            "RFC 9728 §2",
        )),
    }

    if meta.authorization_servers.is_empty() {
        findings.push(AuthFinding::new(
            "prm_authorization_servers",
            Probe::ProtectedResourceMetadata,
            Status::Fail,
            "metadata lists no `authorization_servers`; MCP requires at least one",
            "RFC 9728 §2 · MCP 2025-06-18",
        ));
    } else {
        findings.push(AuthFinding::new(
            "prm_authorization_servers",
            Probe::ProtectedResourceMetadata,
            Status::Pass,
            format!(
                "advertises {} authorization server(s): {}",
                meta.authorization_servers.len(),
                meta.authorization_servers.join(", ")
            ),
            "RFC 9728 §2 · MCP 2025-06-18",
        ));
    }

    // Bearer methods are informational: if present, the `query` method is
    // discouraged by RFC 6750 §2.3.
    if !meta.bearer_methods_supported.is_empty() {
        let method_status = if meta
            .bearer_methods_supported
            .iter()
            .any(|m| m.eq_ignore_ascii_case("query"))
        {
            Status::Info
        } else {
            Status::Pass
        };
        findings.push(AuthFinding::new(
            "prm_bearer_methods",
            Probe::ProtectedResourceMetadata,
            method_status,
            format!(
                "bearer_methods_supported = [{}]",
                meta.bearer_methods_supported.join(", ")
            ),
            "RFC 9728 §2 · RFC 6750 §2",
        ));
    }
}

/// Grade a parsed Authorization Server Metadata document (RFC 8414 + friends).
fn grade_asm(meta: &AuthServerMetadata, findings: &mut Vec<AuthFinding>) {
    // PKCE S256 — the MCP spec REQUIRES it.
    if meta.supports_s256() {
        findings.push(AuthFinding::new(
            "asm_pkce_s256",
            Probe::AuthServerMetadata,
            Status::Pass,
            "advertises PKCE `S256` in code_challenge_methods_supported",
            "MCP 2025-06-18 (PKCE) · RFC 8414 §2",
        ));
    } else if meta.code_challenge_methods_supported.is_empty() {
        findings.push(AuthFinding::new(
            "asm_pkce_s256",
            Probe::AuthServerMetadata,
            Status::Fail,
            "no `code_challenge_methods_supported`; MCP requires PKCE with `S256`",
            "MCP 2025-06-18 (PKCE) · RFC 8414 §2",
        ));
    } else {
        findings.push(AuthFinding::new(
            "asm_pkce_s256",
            Probe::AuthServerMetadata,
            Status::Fail,
            format!(
                "code_challenge_methods_supported = [{}] does not include the REQUIRED `S256`",
                meta.code_challenge_methods_supported.join(", ")
            ),
            "MCP 2025-06-18 (PKCE) · RFC 8414 §2",
        ));
    }

    // Authorization + token endpoints (RFC 8414 required for the code flow).
    if meta.authorization_endpoint.is_some() && meta.token_endpoint.is_some() {
        findings.push(AuthFinding::new(
            "asm_endpoints",
            Probe::AuthServerMetadata,
            Status::Pass,
            "advertises both `authorization_endpoint` and `token_endpoint`",
            "RFC 8414 §2",
        ));
    } else {
        let mut missing = Vec::new();
        if meta.authorization_endpoint.is_none() {
            missing.push("authorization_endpoint");
        }
        if meta.token_endpoint.is_none() {
            missing.push("token_endpoint");
        }
        findings.push(AuthFinding::new(
            "asm_endpoints",
            Probe::AuthServerMetadata,
            Status::Fail,
            format!("missing required endpoint(s): {}", missing.join(", ")),
            "RFC 8414 §2",
        ));
    }

    // Dynamic Client Registration (RFC 7591) — a SHOULD in MCP.
    if meta.registration_endpoint.is_some() {
        findings.push(AuthFinding::new(
            "asm_dcr",
            Probe::AuthServerMetadata,
            Status::Pass,
            "advertises a `registration_endpoint` (Dynamic Client Registration)",
            "RFC 7591 · MCP 2025-06-18",
        ));
    } else {
        findings.push(AuthFinding::new(
            "asm_dcr",
            Probe::AuthServerMetadata,
            Status::NotAdvertised,
            "no `registration_endpoint`; clients cannot self-register (RFC 7591 DCR is a SHOULD)",
            "RFC 7591 · MCP 2025-06-18",
        ));
    }

    // RFC 9207 issuer identification — a SHOULD-strength defence.
    if meta.iss_parameter_supported {
        findings.push(AuthFinding::new(
            "asm_iss",
            Probe::AuthServerMetadata,
            Status::Pass,
            "sets `authorization_response_iss_parameter_supported` (RFC 9207 mix-up defence)",
            "RFC 9207 §3",
        ));
    } else {
        findings.push(AuthFinding::new(
            "asm_iss",
            Probe::AuthServerMetadata,
            Status::NotAdvertised,
            "does not advertise `authorization_response_iss_parameter_supported` (RFC 9207 \
             hardens against AS mix-up)",
            "RFC 9207 §3",
        ));
    }
}

/// Whether an advertised `resource` identifies the probed server. Compared on the
/// canonical form, tolerating a trailing slash and case in scheme/host.
fn resource_matches(advertised: &str, canonical: &str) -> bool {
    canonical_resource_uri(advertised).eq_ignore_ascii_case(canonical)
}

/// Derive the overall verdict from the graded findings.
fn compute_verdict(auth_required: bool, findings: &[AuthFinding]) -> Verdict {
    if !auth_required {
        return Verdict::NoAuth;
    }
    let passes = findings.iter().filter(|f| f.status == Status::Pass).count();
    let fails = findings.iter().filter(|f| f.status == Status::Fail).count();
    match (passes, fails) {
        (0, _) => Verdict::NonConformant,
        (_, 0) => Verdict::Conformant,
        _ => Verdict::PartiallyConformant,
    }
}

/// The minimal `initialize` request body used for both probes.
pub(crate) fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": crate::protocol::LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "jig-auth", "version": env!("CARGO_PKG_VERSION") }
        }
    })
}

/// Perform one HTTP request, record it (redacted) to the tap and the exchange
/// log, and distill the response. Never panics; a transport failure yields a
/// [`FetchResult`] with `status: None` and the exchange's `error` set.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch(
    client: &reqwest::Client,
    tap: &ProtocolTap,
    exchanges: &mut Vec<HttpExchange>,
    label: &str,
    method: reqwest::Method,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<RequestBody>,
    timeout: Option<Duration>,
) -> FetchResult {
    // Assemble the request headers we will send (for the record + the wire).
    let mut sent_headers: Vec<(String, String)> = Vec::new();
    sent_headers.push(("Accept".to_string(), "application/json".to_string()));
    match &body {
        Some(RequestBody::Json(_)) => {
            sent_headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        Some(RequestBody::Form(_)) => sent_headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )),
        None => {}
    }
    for (k, v) in extra_headers {
        sent_headers.push((k.clone(), v.clone()));
    }
    let redacted_request: Vec<(String, String)> = sent_headers
        .iter()
        .map(|(k, v)| (k.clone(), redact_header(k, v)))
        .collect();

    // The request body as it will appear in the record: never the real thing.
    // A token request carries the authorization code, the PKCE verifier, and
    // possibly a client secret; a registration request is harmless but goes
    // through the same door, so there is exactly one redaction path.
    let recorded_body = match &body {
        Some(RequestBody::Json(v)) => redact_json(v.clone()),
        Some(RequestBody::Form(pairs)) => redact_form(pairs),
        None => Value::Null,
    };

    // Record the outbound request to the tap (redacted).
    tap.record(
        Direction::Outbound,
        json!({
            "jig/http_request": {
                "label": label,
                "method": method.as_str(),
                "url": url,
                "headers": headers_to_json(&redacted_request),
                "body": recorded_body,
            }
        }),
    );

    let mut req = client.request(method.clone(), url);
    for (k, v) in &sent_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match &body {
        Some(RequestBody::Json(b)) => req = req.json(b),
        Some(RequestBody::Form(pairs)) => req = req.form(pairs),
        None => {}
    }
    if let Some(t) = timeout {
        req = req.timeout(t);
    }

    let mut exchange = HttpExchange {
        label: label.to_string(),
        method: method.as_str().to_string(),
        url: url.to_string(),
        request_headers: redacted_request,
        status: None,
        response_headers: Vec::new(),
        body: None,
        error: None,
    };

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{e}");
            exchange.error = Some(msg.clone());
            tap.record(
                Direction::Inbound,
                json!({ "jig/http_response": { "label": label, "error": msg } }),
            );
            exchanges.push(exchange);
            return FetchResult {
                status: None,
                www_authenticate: None,
                json: None,
                raw_json: None,
                error: Some(msg),
            };
        }
    };

    let status = resp.status().as_u16();
    let www_authenticate = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut response_headers = Vec::new();
    if let Some(wa) = &www_authenticate {
        response_headers.push(("WWW-Authenticate".to_string(), wa.clone()));
    }
    if let Some(ct) = &content_type {
        response_headers.push(("Content-Type".to_string(), ct.clone()));
    }

    let raw_body = resp.text().await.unwrap_or_default();
    let raw_json = serde_json::from_str::<Value>(&raw_body).ok();
    let json = raw_json.clone().map(redact_json);

    // What goes into the record. A token-endpoint response is JSON carrying an
    // access token, so the *redacted* re-serialization is the only form allowed
    // out of this function into a record; the raw text is captured only when the
    // body is not JSON at all (an HTML error page, a plain-text 404).
    let body_text = match (&json, raw_body.is_empty()) {
        (Some(v), _) => Some(truncate(
            &serde_json::to_string(v).unwrap_or_default(),
            BODY_CAPTURE_LEN,
        )),
        (None, false) => Some(truncate(&raw_body, BODY_CAPTURE_LEN)),
        (None, true) => None,
    };

    exchange.status = Some(status);
    exchange.response_headers = response_headers;
    exchange.body = body_text.clone();

    tap.record(
        Direction::Inbound,
        json!({
            "jig/http_response": {
                "label": label,
                "status": status,
                "headers": headers_to_json(&exchange.response_headers),
                "body": json.clone().unwrap_or_else(|| Value::String(body_text.clone().unwrap_or_default())),
            }
        }),
    );

    exchanges.push(exchange);

    FetchResult {
        status: Some(status),
        www_authenticate,
        json,
        raw_json,
        error: None,
    }
}

/// Redact the secret-bearing members of a form body for the record. OAuth's
/// form-encoded requests carry the authorization code, the PKCE verifier, and
/// the client secret — each of which is enough to mint or replay a token.
pub(crate) fn redact_form(pairs: &[(String, String)]) -> Value {
    const SECRET_FIELDS: &[&str] = &[
        "code",
        "code_verifier",
        "client_secret",
        "refresh_token",
        "assertion",
        "client_assertion",
        "password",
    ];
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        let value = if SECRET_FIELDS.contains(&k.to_ascii_lowercase().as_str()) {
            "<redacted>".to_string()
        } else {
            v.clone()
        };
        map.insert(k.clone(), Value::String(value));
    }
    Value::Object(map)
}

/// Redact obvious secrets from a parsed JSON body (defence in depth — metadata
/// documents are public, but a mis-scripted server might echo a token).
pub(crate) fn redact_json(mut v: Value) -> Value {
    const SECRET_KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "id_token",
        "client_secret",
        "authorization",
    ];
    if let Value::Object(map) = &mut v {
        for (k, val) in map.iter_mut() {
            if SECRET_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
                *val = Value::String("<redacted>".to_string());
            } else {
                let owned = std::mem::take(val);
                *val = redact_json(owned);
            }
        }
    } else if let Value::Array(items) = &mut v {
        for item in items.iter_mut() {
            let owned = std::mem::take(item);
            *item = redact_json(owned);
        }
    }
    v
}

/// A `(name, value)` header list as a JSON object.
fn headers_to_json(headers: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in headers {
        map.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(map)
}

/// Truncate a string to at most `max` bytes on a char boundary, appending `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_challenge_with_resource_metadata() {
        let raw = r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", error="invalid_token""#;
        let ch = WwwAuthenticate::parse(raw).expect("parses");
        assert!(ch.is_bearer());
        assert_eq!(
            ch.param("resource_metadata"),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(ch.param("error"), Some("invalid_token"));
        // Case-insensitive param lookup.
        assert!(ch.param("RESOURCE_METADATA").is_some());
    }

    #[test]
    fn parses_bare_bearer_challenge() {
        let ch = WwwAuthenticate::parse("Bearer").expect("parses");
        assert!(ch.is_bearer());
        assert!(ch.params.is_empty());
        assert_eq!(ch.param("resource_metadata"), None);
    }

    #[test]
    fn empty_challenge_is_none() {
        assert!(WwwAuthenticate::parse("").is_none());
        assert!(WwwAuthenticate::parse("   ").is_none());
    }

    #[test]
    fn non_bearer_scheme_detected() {
        let ch = WwwAuthenticate::parse("Basic realm=\"x\"").expect("parses");
        assert!(!ch.is_bearer());
        assert_eq!(ch.scheme, "Basic");
    }

    #[test]
    fn prm_parses_fields_leniently() {
        let v = json!({
            "resource": "https://mcp.example.com/mcp",
            "authorization_servers": ["https://as.example.com", 42],
            "scopes_supported": ["read", "write"],
            "bearer_methods_supported": ["header"]
        });
        let m = ProtectedResourceMetadata::from_json(&v);
        assert_eq!(m.resource.as_deref(), Some("https://mcp.example.com/mcp"));
        // The non-string entry is dropped, not fatal.
        assert_eq!(m.authorization_servers, vec!["https://as.example.com"]);
        assert_eq!(m.scopes_supported, vec!["read", "write"]);
        assert_eq!(m.bearer_methods_supported, vec!["header"]);
    }

    #[test]
    fn prm_missing_fields_default_empty() {
        let m = ProtectedResourceMetadata::from_json(&json!({}));
        assert!(m.resource.is_none());
        assert!(m.authorization_servers.is_empty());
    }

    #[test]
    fn asm_parses_pkce_and_iss() {
        let v = json!({
            "issuer": "https://as.example.com",
            "authorization_endpoint": "https://as.example.com/authorize",
            "token_endpoint": "https://as.example.com/token",
            "registration_endpoint": "https://as.example.com/register",
            "code_challenge_methods_supported": ["S256", "plain"],
            "authorization_response_iss_parameter_supported": true
        });
        let m = AuthServerMetadata::from_json(&v);
        assert!(m.supports_s256());
        assert!(m.iss_parameter_supported);
        assert!(m.registration_endpoint.is_some());
    }

    #[test]
    fn asm_without_s256_detected() {
        let v = json!({ "code_challenge_methods_supported": ["plain"] });
        let m = AuthServerMetadata::from_json(&v);
        assert!(!m.supports_s256());
        assert!(!m.iss_parameter_supported);
    }

    #[test]
    fn canonical_uri_strips_fragment_and_trailing_slash() {
        assert_eq!(
            canonical_resource_uri("https://mcp.example.com/mcp/"),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(
            canonical_resource_uri("https://mcp.example.com/"),
            "https://mcp.example.com"
        );
        assert_eq!(
            canonical_resource_uri("https://mcp.example.com/mcp#frag"),
            "https://mcp.example.com/mcp"
        );
    }

    #[test]
    fn prm_urls_try_path_appended_then_root() {
        let urls = protected_resource_metadata_urls("https://mcp.example.com/mcp");
        assert_eq!(
            urls,
            vec![
                "https://mcp.example.com/.well-known/oauth-protected-resource/mcp".to_string(),
                "https://mcp.example.com/.well-known/oauth-protected-resource".to_string(),
            ]
        );
    }

    #[test]
    fn prm_urls_rootless_target_only_root() {
        let urls = protected_resource_metadata_urls("https://mcp.example.com");
        assert_eq!(
            urls,
            vec!["https://mcp.example.com/.well-known/oauth-protected-resource".to_string()]
        );
    }

    #[test]
    fn asm_urls_include_rfc8414_and_oidc() {
        let urls = auth_server_metadata_urls("https://as.example.com");
        assert!(urls.contains(
            &"https://as.example.com/.well-known/oauth-authorization-server".to_string()
        ));
        assert!(
            urls.contains(&"https://as.example.com/.well-known/openid-configuration".to_string())
        );
    }

    #[test]
    fn asm_urls_with_path_use_rfc8414_insertion() {
        let urls = auth_server_metadata_urls("https://as.example.com/tenant1");
        // RFC 8414 inserts the well-known before the path.
        assert_eq!(
            urls[0],
            "https://as.example.com/.well-known/oauth-authorization-server/tenant1"
        );
        // OIDC appends to the end.
        assert!(urls.contains(
            &"https://as.example.com/tenant1/.well-known/openid-configuration".to_string()
        ));
    }

    #[test]
    fn resource_match_tolerates_trailing_slash() {
        assert!(resource_matches(
            "https://mcp.example.com/mcp/",
            "https://mcp.example.com/mcp"
        ));
        assert!(!resource_matches(
            "https://evil.example.com/mcp",
            "https://mcp.example.com/mcp"
        ));
    }

    #[test]
    fn grade_prm_flags_audience_mismatch() {
        let meta = ProtectedResourceMetadata {
            resource: Some("https://evil.example.com/mcp".to_string()),
            authorization_servers: vec!["https://as.example.com".to_string()],
            scopes_supported: vec![],
            bearer_methods_supported: vec![],
        };
        let mut findings = Vec::new();
        grade_prm(&meta, "https://mcp.example.com/mcp", &mut findings);
        let audience = findings
            .iter()
            .find(|f| f.key == "prm_resource_audience")
            .unwrap();
        assert_eq!(audience.status, Status::Fail);
    }

    #[test]
    fn grade_asm_full_pass() {
        let meta = AuthServerMetadata {
            issuer: Some("https://as.example.com".to_string()),
            authorization_endpoint: Some("https://as.example.com/authorize".to_string()),
            token_endpoint: Some("https://as.example.com/token".to_string()),
            registration_endpoint: Some("https://as.example.com/register".to_string()),
            code_challenge_methods_supported: vec!["S256".to_string()],
            iss_parameter_supported: true,
        };
        let mut findings = Vec::new();
        grade_asm(&meta, &mut findings);
        assert!(findings.iter().all(|f| f.status == Status::Pass));
        assert!(findings.iter().any(|f| f.key == "asm_pkce_s256"));
    }

    #[test]
    fn grade_asm_no_pkce_fails() {
        let meta = AuthServerMetadata {
            issuer: Some("https://as.example.com".to_string()),
            authorization_endpoint: Some("https://as.example.com/authorize".to_string()),
            token_endpoint: Some("https://as.example.com/token".to_string()),
            registration_endpoint: None,
            code_challenge_methods_supported: vec!["plain".to_string()],
            iss_parameter_supported: false,
        };
        let mut findings = Vec::new();
        grade_asm(&meta, &mut findings);
        let pkce = findings.iter().find(|f| f.key == "asm_pkce_s256").unwrap();
        assert_eq!(pkce.status, Status::Fail);
        // DCR + iss are NOT-ADVERTISED, not failures.
        let dcr = findings.iter().find(|f| f.key == "asm_dcr").unwrap();
        assert_eq!(dcr.status, Status::NotAdvertised);
    }

    #[test]
    fn verdict_reflects_finding_mix() {
        let pass = AuthFinding::new("a", Probe::Challenge, Status::Pass, "", "");
        let fail = AuthFinding::new("b", Probe::Challenge, Status::Fail, "", "");
        assert_eq!(
            compute_verdict(true, std::slice::from_ref(&pass)),
            Verdict::Conformant
        );
        assert_eq!(
            compute_verdict(true, std::slice::from_ref(&fail)),
            Verdict::NonConformant
        );
        assert_eq!(
            compute_verdict(true, &[pass, fail]),
            Verdict::PartiallyConformant
        );
        assert_eq!(compute_verdict(false, &[]), Verdict::NoAuth);
    }

    #[test]
    fn redact_json_hides_tokens() {
        let v = json!({ "access_token": "secret", "nested": { "client_secret": "x", "ok": 1 } });
        let r = redact_json(v);
        assert_eq!(r["access_token"], "<redacted>");
        assert_eq!(r["nested"]["client_secret"], "<redacted>");
        assert_eq!(r["nested"]["ok"], 1);
    }

    #[test]
    fn redact_header_preserves_scheme() {
        assert_eq!(
            redact_header("Authorization", "Bearer abc.def"),
            "Bearer <redacted>"
        );
        assert_eq!(
            redact_header("Content-Type", "application/json"),
            "application/json"
        );
    }
}
