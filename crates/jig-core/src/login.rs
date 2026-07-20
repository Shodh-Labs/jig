//! `jig auth --login` — the **real** OAuth 2.1 authorization-code flow against an
//! MCP server, end to end, and then a proof that the token actually works.
//!
//! Where [`crate::auth`] grades the *discoverable* auth surface without ever
//! touching a credential, this module drives the whole thing: discovery →
//! Dynamic Client Registration → PKCE → browser → loopback callback → token
//! exchange → an authenticated `initialize` + `tools/list`. The payoff is the
//! last step: "authenticated session established: N tools visible" is a claim
//! nothing but a completed flow can make.
//!
//! # The flow, and the clause behind each step
//!
//! | # | Step | Normative source |
//! |---|------|------------------|
//! | 1 | unauthenticated `initialize` → `401` + `WWW-Authenticate` | MCP 2025-06-18 · RFC 9728 §5.1 |
//! | 2 | Protected Resource Metadata | RFC 9728 §3 |
//! | 3 | Authorization Server Metadata | RFC 8414 §3 |
//! | 4 | PKCE `S256` verifier + challenge | RFC 7636 §4.1–§4.2 |
//! | 5 | loopback redirect on `127.0.0.1:0` | RFC 8252 §7.3 |
//! | 6 | client identity: DCR, else `--client-id` | RFC 7591 §3.1 |
//! | 7 | authorization request (`resource`, `state`, `code_challenge`) | MCP 2025-06-18 · RFC 8707 §2 · RFC 7636 §4.3 |
//! | 8 | callback validation: `state`, then `iss` | OAuth 2.1 §7.12 · RFC 9207 §2.4 |
//! | 9 | token exchange | OAuth 2.1 §4.1.3 · RFC 7636 §4.5 · RFC 8707 §2 |
//! | 10 | authenticated `initialize` + `tools/list` | MCP 2025-06-18 (Access Token Usage) |
//!
//! Any step that fails stops the flow: there is no "proceed anyway" path, because
//! every failure here is a security property (a mismatched `state` is CSRF, a
//! mismatched `iss` is an AS mix-up, a missing `S256` is code interception).
//!
//! # Secrets
//!
//! The access token, refresh token, authorization code, PKCE verifier, and
//! `state` nonce are **never** rendered, never serialized, and never recorded.
//! [`Secret`] exists so that even a stray `{:?}` cannot print one, and the token
//! is reachable only through [`LoginOutcome::access_token`], which the CLI calls
//! in exactly two places: the authenticated probe, and `--token-out`.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};

use crate::auth::{
    auth_server_metadata_urls, canonical_resource_uri, fetch, initialize_body,
    protected_resource_metadata_urls, push_unique, string_array, string_field, AuthServerMetadata,
    HttpExchange, ProtectedResourceMetadata, RequestBody, Status, WwwAuthenticate,
};
use crate::tap::ProtocolTap;

/// The client name Jig registers itself under via RFC 7591 Dynamic Client
/// Registration, and sends as `clientInfo.name` on the authenticated handshake.
pub const LOGIN_CLIENT_NAME: &str = "jig";

/// The loopback path the authorization response is redirected to. RFC 8252 §7.3
/// leaves the path free; a fixed, obviously-named one makes the redirect URI
/// readable in an AS consent screen.
const CALLBACK_PATH: &str = "/jig/callback";

/// The largest callback request Jig will read off the loopback socket. A browser
/// GET is a few hundred bytes; anything past this is not a callback and is
/// refused rather than buffered.
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// A string that must never be printed. `Debug` renders `<redacted>`, and there
/// is deliberately no `Display`, no `Serialize`, and no `AsRef<str>` — the only
/// way out is [`Secret::expose`], which is easy to grep for and rare in the
/// tree.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The underlying value. Every call site is a place a secret can escape;
    /// there are three in Jig (the authenticated probe's `Authorization` header,
    /// `--token-out`, and the flow's own internal comparisons).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time equality, for comparing a returned `state` against the one
    /// we generated without leaking its prefix through timing.
    ///
    /// The *length* is not secret — Jig fixes it at 43 characters — so an early
    /// return on a length mismatch reveals nothing. The contents are secret, so
    /// every byte is folded into one accumulator with no branch between them: a
    /// value differing in the first byte costs exactly what one differing in the
    /// last byte costs.
    pub fn ct_eq(&self, other: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), other.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What the caller asked for. Everything here comes from a CLI flag.
#[derive(Debug, Clone, Default)]
pub struct LoginConfig {
    /// A pre-registered client id, used when the AS advertises no
    /// `registration_endpoint` (RFC 7591 DCR is a `SHOULD`, not a `MUST`).
    pub client_id: Option<String>,
    /// An optional client secret for a confidential pre-registered client.
    /// Public clients (the DCR path) never have one.
    pub client_secret: Option<Secret>,
    /// An explicit space-delimited scope string, overriding the PRM's
    /// `scopes_supported`.
    pub scope: Option<String>,
    /// Print the authorization URL but do not launch a browser.
    pub no_browser: bool,
    /// How long to wait for the browser to come back to the loopback listener.
    pub callback_timeout: Duration,
    /// Per-request HTTP timeout, or `None` to wait forever.
    pub http_timeout: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Result data model
// ---------------------------------------------------------------------------

/// One numbered step of the flow trace.
#[derive(Debug, Clone, Serialize)]
pub struct LoginStep {
    /// 1-based position in the flow.
    pub n: usize,
    /// A stable machine key (`pkce`, `callback_state`, …).
    pub key: &'static str,
    /// A short human label for the step.
    pub label: &'static str,
    /// The graded outcome. Only [`Status::Pass`], [`Status::Fail`] and
    /// [`Status::Info`] occur here.
    pub status: Status,
    /// What happened, in one line — never containing a secret.
    pub message: String,
    /// The spec clause the step implements.
    pub citation: &'static str,
}

/// The proof that the minted token works: a real MCP session opened with it.
#[derive(Debug, Clone, Serialize)]
pub struct AuthenticatedSession {
    /// The server's self-reported name.
    pub server_name: String,
    /// The server's self-reported version.
    pub server_version: String,
    /// The negotiated protocol revision.
    pub protocol_version: String,
    /// How many tools `tools/list` returned.
    pub tool_count: usize,
    /// The tool names, in the order the server listed them.
    pub tool_names: Vec<String>,
}

/// The complete result of a login attempt. Always produced, success or not: a
/// failure is a trace with a failing step, not an error string.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    /// The MCP endpoint the flow targeted.
    pub url: String,
    /// The RFC 8707 canonical resource identifier sent as `resource`.
    pub canonical_resource: String,
    /// Every step attempted, in order.
    pub steps: Vec<LoginStep>,
    /// The issuer the authorization server metadata declared.
    pub issuer: Option<String>,
    /// The client id used — from DCR or from `--client-id`. Not a secret: a
    /// public client's id is sent in the browser URL.
    pub client_id: Option<String>,
    /// Whether that client id came from Dynamic Client Registration.
    pub client_registered: bool,
    /// The `token_type` the AS returned (expected `Bearer`).
    pub token_type: Option<String>,
    /// The token lifetime in seconds, if the AS reported one.
    pub expires_in: Option<u64>,
    /// The scope actually granted, if the AS echoed one.
    pub granted_scope: Option<String>,
    /// Whether a refresh token came back. The token itself is held but never
    /// used — see the module docs and the README's honest-boundary section.
    pub refresh_token_issued: bool,
    /// The authenticated MCP session, if the flow got that far.
    pub session: Option<AuthenticatedSession>,
    /// Every HTTP exchange the flow performed, redacted.
    pub exchanges: Vec<HttpExchange>,
    /// The access token. Private: reachable only via
    /// [`LoginOutcome::access_token`].
    access_token: Option<Secret>,
    /// The refresh token, held for the same reason and equally unreachable.
    refresh_token: Option<Secret>,
}

impl LoginOutcome {
    /// The minted access token, if the exchange succeeded. The only door out;
    /// the CLI uses it for the authenticated probe and for `--token-out`.
    pub fn access_token(&self) -> Option<&Secret> {
        self.access_token.as_ref()
    }

    /// The refresh token, if one was issued.
    pub fn refresh_token(&self) -> Option<&Secret> {
        self.refresh_token.as_ref()
    }

    /// Whether every step passed — i.e. an authenticated session was proven.
    pub fn succeeded(&self) -> bool {
        self.session.is_some() && !self.steps.iter().any(|s| s.status == Status::Fail)
    }

    /// The first failing step's message, if any.
    pub fn failure(&self) -> Option<&LoginStep> {
        self.steps.iter().find(|s| s.status == Status::Fail)
    }
}

/// Builder state threaded through the flow so each step can append to the trace.
struct Trace {
    steps: Vec<LoginStep>,
}

impl Trace {
    fn new() -> Trace {
        Trace { steps: Vec::new() }
    }

    fn push(
        &mut self,
        key: &'static str,
        label: &'static str,
        status: Status,
        message: impl Into<String>,
        citation: &'static str,
    ) {
        self.steps.push(LoginStep {
            n: self.steps.len() + 1,
            key,
            label,
            status,
            message: message.into(),
            citation,
        });
    }
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636) — pure
// ---------------------------------------------------------------------------

/// A PKCE verifier/challenge pair (RFC 7636 §4.1–§4.2). Only `S256` is ever
/// produced: `plain` offers no protection against a proxy that can read the
/// authorization request, and the MCP spec requires PKCE outright.
#[derive(Debug, Clone)]
pub struct Pkce {
    /// The high-entropy code verifier — a secret.
    pub verifier: Secret,
    /// `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`, safe to publish.
    pub challenge: String,
}

/// The `S256` challenge for a verifier: `BASE64URL-ENCODE(SHA256(ASCII(verifier)))`
/// exactly as RFC 7636 §4.2 defines it. Pure, and locked to the RFC's own
/// Appendix B test vector in the unit tests.
pub fn s256_challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// Generate a PKCE pair from the system CSPRNG.
///
/// RFC 7636 §4.1 recommends "the output of a suitable random number generator
/// ... to create a 32-octet sequence"; base64url-encoded that is 43 characters,
/// the minimum the RFC permits and the shortest length that carries the full 256
/// bits.
///
/// # Errors
///
/// Returns `Err` if the operating system's randomness source is unavailable —
/// in which case refusing is the only safe answer.
pub fn generate_pkce() -> Result<Pkce, String> {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
    let challenge = s256_challenge(&verifier);
    Ok(Pkce {
        verifier: Secret::new(verifier),
        challenge,
    })
}

/// A base64url-encoded CSPRNG nonce, used for the `state` parameter.
///
/// # Errors
///
/// Returns `Err` if the system randomness source is unavailable.
pub fn generate_state() -> Result<Secret, String> {
    Ok(Secret::new(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?)))
}

/// `N` bytes from the operating system CSPRNG.
fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    use ring::rand::SecureRandom;
    let mut buf = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| "the operating system's secure random source is unavailable".to_string())?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Callback parsing (pure, total)
// ---------------------------------------------------------------------------

/// The parameters an authorization response can carry back to the redirect URI:
/// either a success (`code`, `state`, and optionally `iss`) or an error
/// (`error`, `error_description`, `error_uri`), per OAuth 2.1 §4.1.2 and
/// RFC 9207 §2.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallbackParams {
    /// The authorization code.
    pub code: Option<String>,
    /// The `state` echoed back by the authorization server.
    pub state: Option<String>,
    /// The RFC 9207 issuer identifier.
    pub iss: Option<String>,
    /// An OAuth error code, if the authorization failed.
    pub error: Option<String>,
    /// A human-readable description of that error.
    pub error_description: Option<String>,
    /// A URI with more information about the error.
    pub error_uri: Option<String>,
}

impl CallbackParams {
    /// Whether this response carries anything a redirect handler should act on.
    /// A browser's `GET /favicon.ico` alongside the real callback carries
    /// neither, and is ignored rather than mistaken for a failed authorization.
    pub fn is_authorization_response(&self) -> bool {
        self.code.is_some() || self.error.is_some()
    }
}

/// Parse an authorization response's query string.
///
/// **Total over arbitrary bytes**: every input yields a [`CallbackParams`],
/// never a panic — a redirect target is attacker-reachable (anything on the
/// machine can hit the loopback port), so the parser is property-tested against
/// arbitrary strings.
pub fn parse_callback_query(query: &str) -> CallbackParams {
    let mut out = CallbackParams::default();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = form_decode(raw_key);
        let value = form_decode(raw_value);
        // First occurrence wins: a duplicated parameter is a smuggling attempt,
        // and taking the first keeps us aligned with what a validating AS saw.
        let slot = match key.as_str() {
            "code" => &mut out.code,
            "state" => &mut out.state,
            "iss" => &mut out.iss,
            "error" => &mut out.error,
            "error_description" => &mut out.error_description,
            "error_uri" => &mut out.error_uri,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value);
        }
    }
    out
}

/// Decode one `application/x-www-form-urlencoded` component: `+` is a space and
/// `%XX` is a byte. A malformed escape is kept literally rather than dropped, so
/// no input is rejected and no input panics. Invalid UTF-8 becomes U+FFFD.
fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match hex_pair(bytes[i + 1], bytes[i + 2]) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Two ASCII hex digits as a byte, or `None` if either is not hex.
fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_digit(hi)? << 4) | hex_digit(lo)?)
}

/// One ASCII hex digit as its value.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Extract the query string from an HTTP request line's target
/// (`GET /jig/callback?code=… HTTP/1.1` → `code=…`). Returns `""` when the
/// target carries no query. Total over arbitrary input.
pub fn query_from_request_line(line: &str) -> &str {
    let target = line.split_whitespace().nth(1).unwrap_or_default();
    match target.split_once('?') {
        Some((_, q)) => q,
        None => "",
    }
}

// ---------------------------------------------------------------------------
// Authorization URL (pure)
// ---------------------------------------------------------------------------

/// Build the authorization request URL (OAuth 2.1 §4.1.1, RFC 7636 §4.3,
/// RFC 8707 §2). `scope` is omitted entirely when empty rather than sent blank.
///
/// # Errors
///
/// Returns `Err` if `authorization_endpoint` is not a parseable absolute URL.
#[allow(clippy::too_many_arguments)]
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    resource: &str,
    scope: Option<&str>,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(authorization_endpoint).map_err(|e| {
        format!("the authorization_endpoint `{authorization_endpoint}` is not a valid URL: {e}")
    })?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("state", state);
        q.append_pair("code_challenge", code_challenge);
        // Never `plain`. RFC 7636 §4.2: a client capable of S256 MUST use S256,
        // and the MCP spec requires PKCE unconditionally.
        q.append_pair("code_challenge_method", "S256");
        // RFC 8707 §2 / MCP: sent regardless of whether the AS advertises
        // support, so a future-conformant AS binds the token's audience.
        q.append_pair("resource", resource);
        if let Some(s) = scope.filter(|s| !s.trim().is_empty()) {
            q.append_pair("scope", s);
        }
    }
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// The loopback redirect listener (RFC 8252 §7.3)
// ---------------------------------------------------------------------------

/// A bound loopback listener waiting for exactly one authorization response.
pub struct Loopback {
    listener: tokio::net::TcpListener,
    /// The redirect URI to register and send — `http://127.0.0.1:<port>/jig/callback`.
    redirect_uri: String,
}

impl Loopback {
    /// The redirect URI this listener answers on.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }
}

/// Bind an ephemeral loopback port for the authorization response.
///
/// RFC 8252 §7.3 has native apps use a loopback interface redirect with a port
/// the OS assigns at request time; `127.0.0.1:0` is exactly that. Binding the
/// literal address rather than `localhost` avoids a DNS lookup resolving to
/// something else on the machine.
///
/// # Errors
///
/// Returns `Err` if the loopback port cannot be bound.
pub async fn bind_loopback() -> Result<Loopback, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("could not bind a loopback port for the OAuth redirect: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read the bound loopback port: {e}"))?
        .port();
    Ok(Loopback {
        listener,
        redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
    })
}

/// Serve the loopback listener until an authorization response arrives, then
/// shut it down. Requests that are not authorization responses (a browser's
/// speculative `/favicon.ico`, a port scanner) are answered `404` and the wait
/// continues.
///
/// # Errors
///
/// Returns `Err` on timeout or if the listener fails.
pub async fn wait_for_callback(
    loopback: Loopback,
    timeout: Duration,
) -> Result<CallbackParams, String> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => {
                return Err(format!(
                    "timed out after {}s waiting for the authorization response on {} — \
                     complete the login in the browser, or raise --timeout",
                    timeout.as_secs(),
                    loopback.redirect_uri
                ));
            }
            accepted = loopback.listener.accept() => {
                let (stream, _peer) = accepted
                    .map_err(|e| format!("the loopback redirect listener failed: {e}"))?;
                match serve_one(stream).await {
                    Some(params) => return Ok(params),
                    // Not a callback: keep listening on the same port.
                    None => continue,
                }
            }
        }
    }
}

/// Read one HTTP request off `stream`, answer it, and return its authorization
/// parameters if it was in fact the callback.
async fn serve_one(mut stream: tokio::net::TcpStream) -> Option<CallbackParams> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read until the end of the request head. We never need the body: an
    // authorization response is a GET.
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if find_head_end(&buf).is_some() || buf.len() >= MAX_CALLBACK_REQUEST_BYTES {
            break;
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or_default();
    let params = parse_callback_query(query_from_request_line(request_line));

    let (status, body) = if params.is_authorization_response() {
        ("200 OK", CALLBACK_PAGE)
    } else {
        ("404 Not Found", NOT_THE_CALLBACK_PAGE)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    params.is_authorization_response().then_some(params)
}

/// The index of the end of an HTTP request head (`\r\n\r\n`), if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The page the browser lands on after a successful authorization. Deliberately
/// plain: no scripts, no fonts, no network, nothing that could exfiltrate the
/// query string it was just handed.
const CALLBACK_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>jig — \
     authorization received</title><body style=\"font:16px system-ui;margin:4rem auto;max-width:32rem\">\
     <h1>Authorization received</h1><p>jig has the authorization code. \
     You can close this tab and return to your terminal.</p></body>";

/// The page for any other request that reaches the one-shot listener.
const NOT_THE_CALLBACK_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>jig</title>\
     <body><p>Not the OAuth callback. jig is still waiting for it.</p></body>";

// ---------------------------------------------------------------------------
// Browser launch (best effort)
// ---------------------------------------------------------------------------

/// Open `url` in the user's default browser, best effort.
///
/// Failure is never fatal: the URL has already been printed, so a user on a
/// headless box can paste it themselves. That is also exactly what `--no-browser`
/// does deliberately.
///
/// # Errors
///
/// Returns `Err` with the launcher's own message if the platform opener could
/// not be spawned.
pub fn open_browser(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        // `start` is a cmd builtin, and an authorization URL is full of `&`,
        // which cmd would otherwise read as a command separator. `raw_arg`
        // passes the quoted command line through verbatim.
        c.arg("/C").raw_arg(format!("start \"\" \"{url}\""));
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("could not launch a browser: {e}"))
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// Discovery results carried between steps 1–3.
struct Discovered {
    prm: Option<ProtectedResourceMetadata>,
    asm: AuthServerMetadata,
}

/// A hook the caller supplies so the flow can report progress (the authorization
/// URL, the "waiting for the browser" line) while it is still running. Jig
/// writes these to stderr so stdout stays the report.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Run the full authorization-code flow against `url` and prove the resulting
/// token by opening an authenticated MCP session.
///
/// Always returns a [`LoginOutcome`]: a failure is a trace whose last step is
/// [`Status::Fail`], never an `Err`, so the caller renders one thing.
pub async fn login(
    url: &str,
    cfg: &LoginConfig,
    tap: &ProtocolTap,
    progress: Progress<'_>,
) -> LoginOutcome {
    let canonical_resource = canonical_resource_uri(url);
    let mut trace = Trace::new();
    let mut exchanges: Vec<HttpExchange> = Vec::new();

    let mut outcome = LoginOutcome {
        url: url.to_string(),
        canonical_resource: canonical_resource.clone(),
        steps: Vec::new(),
        issuer: None,
        client_id: None,
        client_registered: false,
        token_type: None,
        expires_in: None,
        granted_scope: None,
        refresh_token_issued: false,
        session: None,
        exchanges: Vec::new(),
        access_token: None,
        refresh_token: None,
    };

    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            trace.push(
                "http_client",
                "HTTP client",
                Status::Fail,
                format!("could not build an HTTP client: {e}"),
                "n/a",
            );
            outcome.steps = trace.steps;
            return outcome;
        }
    };

    // ---- Steps 1–3: discovery -------------------------------------------
    let discovered = match discover(
        &client,
        url,
        tap,
        &mut exchanges,
        &mut trace,
        cfg.http_timeout,
    )
    .await
    {
        Some(d) => d,
        None => return finish(outcome, trace, exchanges),
    };
    outcome.issuer = discovered.asm.issuer.clone();

    // ---- Step 4: PKCE ----------------------------------------------------
    // Checked before anything is registered or any browser opens: if the AS
    // cannot do S256 there is no safe flow to start, and starting one anyway
    // would train users to accept a downgrade.
    if !discovered.asm.supports_s256() {
        let advertised = if discovered.asm.code_challenge_methods_supported.is_empty() {
            "none at all".to_string()
        } else {
            format!(
                "[{}]",
                discovered.asm.code_challenge_methods_supported.join(", ")
            )
        };
        trace.push(
            "pkce",
            "PKCE S256",
            Status::Fail,
            format!(
                "the authorization server advertises code_challenge_methods_supported = \
                 {advertised}, which does not include `S256`. Jig refuses to fall back to \
                 `plain`: the MCP specification requires PKCE, and RFC 7636 §4.2 requires a \
                 client capable of `S256` to use it."
            ),
            "MCP 2025-06-18 (Authorization Code Protection) · RFC 7636 §4.2",
        );
        return finish(outcome, trace, exchanges);
    }
    let pkce = match generate_pkce() {
        Ok(p) => p,
        Err(e) => {
            trace.push("pkce", "PKCE S256", Status::Fail, e, "RFC 7636 §4.1");
            return finish(outcome, trace, exchanges);
        }
    };
    let state = match generate_state() {
        Ok(s) => s,
        Err(e) => {
            trace.push("pkce", "PKCE S256", Status::Fail, e, "RFC 7636 §4.1");
            return finish(outcome, trace, exchanges);
        }
    };
    trace.push(
        "pkce",
        "PKCE S256",
        Status::Pass,
        format!(
            "generated a {}-character code verifier from the system CSPRNG and its S256 \
             challenge; `state` is an independent 256-bit nonce",
            pkce.verifier.expose().len()
        ),
        "RFC 7636 §4.1–§4.2",
    );

    // ---- Step 5: loopback redirect ---------------------------------------
    let loopback = match bind_loopback().await {
        Ok(l) => l,
        Err(e) => {
            trace.push(
                "loopback",
                "Loopback redirect",
                Status::Fail,
                e,
                "RFC 8252 §7.3",
            );
            return finish(outcome, trace, exchanges);
        }
    };
    let redirect_uri = loopback.redirect_uri().to_string();
    trace.push(
        "loopback",
        "Loopback redirect",
        Status::Pass,
        format!("listening for the authorization response on {redirect_uri}"),
        "RFC 8252 §7.3 · MCP 2025-06-18 (redirect URIs MUST be localhost or HTTPS)",
    );

    // ---- Step 6: client identity -----------------------------------------
    let client_id = match establish_client_identity(
        &client,
        &discovered.asm,
        &redirect_uri,
        cfg,
        tap,
        &mut exchanges,
        &mut trace,
        &mut outcome,
    )
    .await
    {
        Some(id) => id,
        None => return finish(outcome, trace, exchanges),
    };

    // ---- Step 7: the authorization request -------------------------------
    let scope = cfg.scope.clone().or_else(|| {
        discovered
            .prm
            .as_ref()
            .filter(|p| !p.scopes_supported.is_empty())
            .map(|p| p.scopes_supported.join(" "))
    });
    let authorize_endpoint = match &discovered.asm.authorization_endpoint {
        Some(e) => e.clone(),
        None => {
            trace.push(
                "authorize",
                "Authorization request",
                Status::Fail,
                "the authorization server metadata declares no `authorization_endpoint`, so \
                 there is nowhere to send the user",
                "RFC 8414 §2",
            );
            return finish(outcome, trace, exchanges);
        }
    };
    let authorize_url = match build_authorization_url(
        &authorize_endpoint,
        &client_id,
        &redirect_uri,
        state.expose(),
        &pkce.challenge,
        &canonical_resource,
        scope.as_deref(),
    ) {
        Ok(u) => u,
        Err(e) => {
            trace.push(
                "authorize",
                "Authorization request",
                Status::Fail,
                e,
                "OAuth 2.1 §4.1.1",
            );
            return finish(outcome, trace, exchanges);
        }
    };

    // The URL is printed before the browser opens, so a failed launch, a
    // headless box, and `--no-browser` all leave the user something to paste.
    progress(&format!(
        "\nopen this URL to authorize:\n\n  {authorize_url}\n"
    ));
    let browser = if cfg.no_browser {
        "--no-browser: the URL was printed, not opened".to_string()
    } else {
        match open_browser(&authorize_url) {
            Ok(()) => "opened the system browser".to_string(),
            Err(e) => format!("{e} — open the printed URL by hand"),
        }
    };
    trace.push(
        "authorize",
        "Authorization request",
        Status::Pass,
        format!(
            "sent response_type=code with code_challenge_method=S256, a CSPRNG `state`, \
             resource={canonical_resource}{} — {browser}",
            match &scope {
                Some(s) => format!(", scope={s}"),
                None => String::new(),
            }
        ),
        "OAuth 2.1 §4.1.1 · RFC 7636 §4.3 · RFC 8707 §2",
    );

    // ---- Step 8: the callback --------------------------------------------
    progress(&format!(
        "waiting up to {}s for the authorization response…\n",
        cfg.callback_timeout.as_secs()
    ));
    let params = match wait_for_callback(loopback, cfg.callback_timeout).await {
        Ok(p) => p,
        Err(e) => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                e,
                "OAuth 2.1 §4.1.2",
            );
            return finish(outcome, trace, exchanges);
        }
    };
    let code = match validate_callback(&params, &state, &discovered.asm, &mut trace) {
        Some(c) => c,
        None => return finish(outcome, trace, exchanges),
    };

    // ---- Step 9: token exchange ------------------------------------------
    let token_endpoint = match &discovered.asm.token_endpoint {
        Some(e) => e.clone(),
        None => {
            trace.push(
                "token",
                "Token exchange",
                Status::Fail,
                "the authorization server metadata declares no `token_endpoint`",
                "RFC 8414 §2",
            );
            return finish(outcome, trace, exchanges);
        }
    };
    let ok = exchange_code(
        &client,
        &token_endpoint,
        &code,
        &pkce.verifier,
        &redirect_uri,
        &client_id,
        &canonical_resource,
        cfg,
        tap,
        &mut exchanges,
        &mut trace,
        &mut outcome,
    )
    .await;
    if !ok {
        return finish(outcome, trace, exchanges);
    }

    // ---- Step 10: prove the token works ----------------------------------
    prove_session(url, cfg, tap, &mut trace, &mut outcome).await;

    finish(outcome, trace, exchanges)
}

/// Fold the trace and exchange log into the outcome.
fn finish(mut outcome: LoginOutcome, trace: Trace, exchanges: Vec<HttpExchange>) -> LoginOutcome {
    outcome.steps = trace.steps;
    outcome.exchanges = exchanges;
    outcome
}

/// Steps 1–3: the unauthenticated challenge, then the two metadata documents.
/// Reuses [`crate::auth`]'s well-known URL construction and lenient parsers, so
/// the login flow and the conformance prober can never disagree about where a
/// document lives or what it says.
async fn discover(
    client: &reqwest::Client,
    url: &str,
    tap: &ProtocolTap,
    exchanges: &mut Vec<HttpExchange>,
    trace: &mut Trace,
    timeout: Option<Duration>,
) -> Option<Discovered> {
    // Step 1: the challenge.
    let bare = fetch(
        client,
        tap,
        exchanges,
        "unauthenticated initialize",
        reqwest::Method::POST,
        url,
        &[],
        Some(RequestBody::Json(initialize_body())),
        timeout,
    )
    .await;

    let mut explicit_prm: Option<String> = None;
    match bare.status {
        None => {
            trace.push(
                "challenge",
                "Unauthenticated challenge",
                Status::Fail,
                format!(
                    "could not reach {url}: {}",
                    bare.error.unwrap_or_else(|| "no response".to_string())
                ),
                "MCP 2025-06-18 (Authorization)",
            );
            return None;
        }
        Some(s) if (200..300).contains(&s) => {
            trace.push(
                "challenge",
                "Unauthenticated challenge",
                Status::Fail,
                format!(
                    "the server answered an unauthenticated `initialize` with HTTP {s} — it \
                     requires no authorization, so there is no flow to run. Drop `--login` to \
                     grade its (absent) auth surface instead."
                ),
                "MCP 2025-06-18 (Authorization is OPTIONAL)",
            );
            return None;
        }
        Some(s) => {
            let challenge = bare
                .www_authenticate
                .as_deref()
                .and_then(WwwAuthenticate::parse);
            if let Some(ch) = &challenge {
                if let Some(rm) = ch.param("resource_metadata").filter(|r| !r.is_empty()) {
                    explicit_prm = Some(rm.to_string());
                }
            }
            let detail = match &explicit_prm {
                Some(rm) => format!("and pointed at its resource metadata ({rm})"),
                None => "but carried no `resource_metadata` parameter; falling back to the \
                         RFC 9728 well-known locations"
                    .to_string(),
            };
            trace.push(
                "challenge",
                "Unauthenticated challenge",
                Status::Pass,
                format!(
                    "the server challenged an unauthenticated `initialize` with HTTP {s} {detail}"
                ),
                "MCP 2025-06-18 · RFC 9728 §5.1",
            );
        }
    }

    // Step 2: Protected Resource Metadata.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(rm) = &explicit_prm {
        candidates.push(rm.clone());
    }
    for u in protected_resource_metadata_urls(url) {
        push_unique(&mut candidates, u);
    }
    let mut prm: Option<ProtectedResourceMetadata> = None;
    let mut prm_source = String::new();
    for candidate in &candidates {
        let res = fetch(
            client,
            tap,
            exchanges,
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
            prm_source = candidate.clone();
            break;
        }
    }

    // The AS issuers to try. RFC 9728 says the PRM names them; without a PRM the
    // flow cannot know where to send the user, and guessing an issuer is exactly
    // the mix-up RFC 9207 exists to prevent.
    let issuers = match &prm {
        Some(meta) if !meta.authorization_servers.is_empty() => {
            trace.push(
                "prm",
                "Protected Resource Metadata",
                Status::Pass,
                format!(
                    "fetched from {prm_source}: resource={}, authorization_servers=[{}]{}",
                    meta.resource.clone().unwrap_or_else(|| "<absent>".into()),
                    meta.authorization_servers.join(", "),
                    if meta.scopes_supported.is_empty() {
                        String::new()
                    } else {
                        format!(", scopes_supported=[{}]", meta.scopes_supported.join(", "))
                    }
                ),
                "RFC 9728 §3",
            );
            meta.authorization_servers.clone()
        }
        Some(_) => {
            trace.push(
                "prm",
                "Protected Resource Metadata",
                Status::Fail,
                format!(
                    "the metadata at {prm_source} lists no `authorization_servers`; MCP requires \
                     at least one, and Jig will not guess an issuer"
                ),
                "RFC 9728 §2 · MCP 2025-06-18",
            );
            return None;
        }
        None => {
            trace.push(
                "prm",
                "Protected Resource Metadata",
                Status::Fail,
                format!(
                    "no RFC 9728 protected-resource metadata at any of {} candidate location(s); \
                     without it there is no way to learn which authorization server to use",
                    candidates.len()
                ),
                "RFC 9728 §3 · MCP 2025-06-18",
            );
            return None;
        }
    };

    // Step 3: Authorization Server Metadata, for the first advertised issuer.
    let issuer = issuers[0].clone();
    for candidate in auth_server_metadata_urls(&issuer) {
        let res = fetch(
            client,
            tap,
            exchanges,
            "GET authorization-server-metadata",
            reqwest::Method::GET,
            &candidate,
            &[],
            None,
            timeout,
        )
        .await;
        if let (Some(200..=299), Some(v)) = (res.status, &res.json) {
            let asm = AuthServerMetadata::from_json(v);
            trace.push(
                "asm",
                "Authorization Server Metadata",
                Status::Pass,
                format!(
                    "fetched from {candidate}: issuer={}, code_challenge_methods_supported=[{}], \
                     registration_endpoint={}",
                    asm.issuer.clone().unwrap_or_else(|| "<absent>".into()),
                    asm.code_challenge_methods_supported.join(", "),
                    if asm.registration_endpoint.is_some() {
                        "present"
                    } else {
                        "absent"
                    }
                ),
                "RFC 8414 §3",
            );
            return Some(Discovered { prm, asm });
        }
    }
    trace.push(
        "asm",
        "Authorization Server Metadata",
        Status::Fail,
        format!(
            "the advertised authorization server ({issuer}) served no RFC 8414 or OIDC metadata \
             at its well-known locations"
        ),
        "RFC 8414 §3 · MCP 2025-06-18",
    );
    None
}

/// Step 6: obtain a client id, by Dynamic Client Registration where the AS
/// offers it and from `--client-id` otherwise.
#[allow(clippy::too_many_arguments)]
async fn establish_client_identity(
    client: &reqwest::Client,
    asm: &AuthServerMetadata,
    redirect_uri: &str,
    cfg: &LoginConfig,
    tap: &ProtocolTap,
    exchanges: &mut Vec<HttpExchange>,
    trace: &mut Trace,
    outcome: &mut LoginOutcome,
) -> Option<String> {
    // An explicit --client-id always wins: the user knows their own
    // pre-registration, and re-registering on every run would litter the AS.
    if let Some(id) = &cfg.client_id {
        outcome.client_id = Some(id.clone());
        trace.push(
            "client",
            "Client identity",
            Status::Pass,
            format!(
                "using the supplied --client-id `{id}`{}",
                if cfg.client_secret.is_some() {
                    " with a client secret (confidential client)"
                } else {
                    " as a public client"
                }
            ),
            "OAuth 2.1 §2.2",
        );
        return Some(id.clone());
    }

    let Some(registration_endpoint) = &asm.registration_endpoint else {
        trace.push(
            "client",
            "Client identity",
            Status::Fail,
            "the authorization server advertises no `registration_endpoint`, so Jig cannot \
             self-register (RFC 7591 Dynamic Client Registration is a SHOULD, not a MUST). \
             Register a client with this authorization server yourself and pass \
             `--client-id <id>` (plus `--client-secret <s>` if it issued one).",
            "RFC 7591 · MCP 2025-06-18 (Dynamic Client Registration)",
        );
        return None;
    };

    let body = json!({
        "client_name": LOGIN_CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        // A CLI cannot keep a secret, so it registers as a public client and
        // relies on PKCE instead (OAuth 2.1 §2.2 / RFC 8252 §8.4).
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    let res = fetch(
        client,
        tap,
        exchanges,
        "POST register (RFC 7591 DCR)",
        reqwest::Method::POST,
        registration_endpoint,
        &[],
        Some(RequestBody::Json(body)),
        cfg.http_timeout,
    )
    .await;

    let registered_id = res
        .raw_json
        .as_ref()
        .and_then(|v| string_field(v, "client_id"));
    match (res.status, registered_id) {
        (Some(200..=299), Some(id)) => {
            outcome.client_id = Some(id.clone());
            outcome.client_registered = true;
            trace.push(
                "client",
                "Client identity",
                Status::Pass,
                format!(
                    "registered dynamically at {registration_endpoint} as a public native client \
                     (token_endpoint_auth_method=none, redirect_uris=[{redirect_uri}]) → \
                     client_id `{id}`"
                ),
                "RFC 7591 §3.1–§3.2.1",
            );
            Some(id)
        }
        (Some(s), _) => {
            let detail = res
                .json
                .as_ref()
                .and_then(|v| {
                    let err = string_field(v, "error")?;
                    Some(match string_field(v, "error_description") {
                        Some(d) => format!("{err}: {d}"),
                        None => err,
                    })
                })
                .unwrap_or_else(|| "no OAuth error object in the response".to_string());
            trace.push(
                "client",
                "Client identity",
                Status::Fail,
                format!(
                    "dynamic client registration at {registration_endpoint} returned HTTP {s} \
                     ({detail}). Pass `--client-id <id>` to use a client you registered yourself."
                ),
                "RFC 7591 §3.2.2",
            );
            None
        }
        (None, _) => {
            trace.push(
                "client",
                "Client identity",
                Status::Fail,
                format!(
                    "could not reach the registration endpoint {registration_endpoint}: {}",
                    res.error.unwrap_or_else(|| "no response".to_string())
                ),
                "RFC 7591 §3.1",
            );
            None
        }
    }
}

/// Step 8: validate the authorization response before it is worth anything.
///
/// Returns the authorization code only when `state` matches in constant time
/// and — when the AS said it would send one — `iss` equals the issuer.
fn validate_callback(
    params: &CallbackParams,
    state: &Secret,
    asm: &AuthServerMetadata,
    trace: &mut Trace,
) -> Option<String> {
    // An error response is surfaced verbatim: the AS's own words are more useful
    // than any paraphrase, and inventing one hides the real cause.
    if let Some(err) = &params.error {
        let mut msg = format!("the authorization server returned `error={err}`");
        if let Some(d) = &params.error_description {
            msg.push_str(&format!(", `error_description={d}`"));
        }
        if let Some(u) = &params.error_uri {
            msg.push_str(&format!(", `error_uri={u}`"));
        }
        trace.push(
            "callback",
            "Authorization response",
            Status::Fail,
            msg,
            "OAuth 2.1 §4.1.2.1",
        );
        return None;
    }

    // `state`: the CSRF binding. Compared in constant time, and a missing one is
    // as fatal as a wrong one.
    match &params.state {
        Some(s) if state.ct_eq(s) => {}
        Some(_) => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                "the `state` in the authorization response does not match the nonce Jig \
                 generated — the response belongs to a different authorization request. This is \
                 what CSRF against the redirect endpoint looks like; the code was discarded.",
                "OAuth 2.1 §7.12 · MCP 2025-06-18 (Open Redirection)",
            );
            return None;
        }
        None => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                "the authorization response carried no `state` parameter, so it cannot be bound \
                 to Jig's request; the code was discarded.",
                "OAuth 2.1 §7.12 · MCP 2025-06-18 (Open Redirection)",
            );
            return None;
        }
    }

    // `iss`: the AS mix-up defence. RFC 9207 §2.4 — a client MUST reject a
    // response whose `iss` does not match the expected issuer. We enforce it
    // whenever an `iss` is present, and additionally require one when the AS's
    // own metadata promised it.
    let expected = asm.issuer.as_deref();
    match (&params.iss, expected) {
        (Some(got), Some(want)) if got == want => {}
        (Some(got), Some(want)) => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                format!(
                    "the authorization response's `iss` is `{got}` but the authorization server \
                     metadata declares `issuer` `{want}`. RFC 9207 §2.4 requires the client to \
                     reject the response and not proceed with the grant — a mismatch is the \
                     signature of an authorization-server mix-up attack."
                ),
                "RFC 9207 §2.4",
            );
            return None;
        }
        (Some(got), None) => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                format!(
                    "the authorization response carries `iss={got}` but the authorization server \
                     metadata declares no `issuer` to check it against, so the mix-up defence \
                     cannot be evaluated"
                ),
                "RFC 9207 §2.3–§2.4",
            );
            return None;
        }
        (None, _) if asm.iss_parameter_supported => {
            trace.push(
                "callback",
                "Authorization response",
                Status::Fail,
                "the authorization server sets \
                 `authorization_response_iss_parameter_supported=true` but sent no `iss` in the \
                 authorization response, so its own mix-up defence is not in force",
                "RFC 9207 §2.3",
            );
            return None;
        }
        (None, _) => {}
    }

    let Some(code) = params.code.clone().filter(|c| !c.is_empty()) else {
        trace.push(
            "callback",
            "Authorization response",
            Status::Fail,
            "the authorization response carried neither an authorization `code` nor an `error`",
            "OAuth 2.1 §4.1.2",
        );
        return None;
    };

    let iss_note = match &params.iss {
        Some(_) => " and its `iss` matches the metadata issuer (RFC 9207)",
        None => " (the authorization server sends no `iss`; RFC 9207 is not in force here)",
    };
    trace.push(
        "callback",
        "Authorization response",
        Status::Pass,
        format!(
            "received an authorization code on the loopback redirect; its `state` matches the \
             generated nonce (constant-time){iss_note}"
        ),
        "OAuth 2.1 §4.1.2 · §7.12 · RFC 9207 §2.4",
    );
    Some(code)
}

/// Step 9: redeem the authorization code for an access token.
#[allow(clippy::too_many_arguments)]
async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    verifier: &Secret,
    redirect_uri: &str,
    client_id: &str,
    resource: &str,
    cfg: &LoginConfig,
    tap: &ProtocolTap,
    exchanges: &mut Vec<HttpExchange>,
    trace: &mut Trace,
    outcome: &mut LoginOutcome,
) -> bool {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.to_string()),
        ("redirect_uri".into(), redirect_uri.to_string()),
        ("client_id".into(), client_id.to_string()),
        ("code_verifier".into(), verifier.expose().to_string()),
        // RFC 8707 §2 / MCP: the same resource sent on the authorization
        // request, so the AS can bind the token's audience to this MCP server.
        ("resource".into(), resource.to_string()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret".into(), secret.expose().to_string()));
    }

    let res = fetch(
        client,
        tap,
        exchanges,
        "POST token",
        reqwest::Method::POST,
        token_endpoint,
        &[],
        Some(RequestBody::Form(form)),
        cfg.http_timeout,
    )
    .await;

    let Some(status) = res.status else {
        trace.push(
            "token",
            "Token exchange",
            Status::Fail,
            format!(
                "could not reach the token endpoint {token_endpoint}: {}",
                res.error.unwrap_or_else(|| "no response".to_string())
            ),
            "OAuth 2.1 §4.1.3",
        );
        return false;
    };

    if !(200..300).contains(&status) {
        // The AS's own error, verbatim (RFC 6749 §5.2 shape).
        let detail = res
            .json
            .as_ref()
            .and_then(|v| {
                let err = string_field(v, "error")?;
                Some(match string_field(v, "error_description") {
                    Some(d) => format!("`error={err}`, `error_description={d}`"),
                    None => format!("`error={err}`"),
                })
            })
            .unwrap_or_else(|| "no OAuth error object in the response".to_string());
        trace.push(
            "token",
            "Token exchange",
            Status::Fail,
            format!("the token endpoint returned HTTP {status}: {detail}"),
            "OAuth 2.1 §4.1.3 · RFC 6749 §5.2",
        );
        return false;
    }

    let Some(body) = res.raw_json.as_ref() else {
        trace.push(
            "token",
            "Token exchange",
            Status::Fail,
            format!("the token endpoint returned HTTP {status} with a body that is not JSON"),
            "OAuth 2.1 §4.1.4",
        );
        return false;
    };

    let Some(access_token) = string_field(body, "access_token").filter(|t| !t.is_empty()) else {
        trace.push(
            "token",
            "Token exchange",
            Status::Fail,
            "the token response carries no `access_token`",
            "OAuth 2.1 §4.1.4",
        );
        return false;
    };

    let token_type = string_field(body, "token_type").unwrap_or_else(|| "Bearer".to_string());
    outcome.token_type = Some(token_type.clone());
    outcome.expires_in = body.get("expires_in").and_then(Value::as_u64);
    outcome.granted_scope = string_field(body, "scope");
    let refresh = string_field(body, "refresh_token").filter(|t| !t.is_empty());
    outcome.refresh_token_issued = refresh.is_some();
    outcome.refresh_token = refresh.map(Secret::new);
    outcome.access_token = Some(Secret::new(access_token));

    if !token_type.eq_ignore_ascii_case("Bearer") {
        trace.push(
            "token",
            "Token exchange",
            Status::Fail,
            format!(
                "the token endpoint issued a `{token_type}` token; MCP access tokens are Bearer \
                 tokens presented in the `Authorization` header"
            ),
            "MCP 2025-06-18 (Token Requirements) · RFC 6750 §2.1",
        );
        return false;
    }

    trace.push(
        "token",
        "Token exchange",
        Status::Pass,
        format!(
            "redeemed the authorization code with the PKCE verifier at {token_endpoint} → a \
             {token_type} token{}{}{} (the token itself is never printed)",
            match outcome.expires_in {
                Some(s) => format!(", expires_in={s}s"),
                None => ", no expiry reported".to_string(),
            },
            match &outcome.granted_scope {
                Some(s) => format!(", scope={s}"),
                None => String::new(),
            },
            if outcome.refresh_token_issued {
                ", refresh token issued"
            } else {
                ""
            }
        ),
        "OAuth 2.1 §4.1.3–§4.1.4 · RFC 7636 §4.5 · RFC 8707 §2",
    );
    true
}

/// Step 10: the payoff — open a real MCP session with the minted token.
async fn prove_session(
    url: &str,
    cfg: &LoginConfig,
    tap: &ProtocolTap,
    trace: &mut Trace,
    outcome: &mut LoginOutcome,
) {
    let Some(token) = outcome.access_token.as_ref() else {
        return;
    };
    let headers = vec![(
        "Authorization".to_string(),
        format!("Bearer {}", token.expose()),
    )];
    let options = crate::client::ClientOptions {
        request_timeout: cfg.http_timeout,
        ..Default::default()
    };

    let client =
        match crate::client::Client::connect_http_with_options(url, headers, tap.clone(), options)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                trace.push(
                    "session",
                    "Authenticated MCP session",
                    Status::Fail,
                    format!(
                        "the minted token was rejected by {url}: {e}. The flow completed but the \
                         token does not open a session — check that the authorization server \
                         issued it for this resource (RFC 8707 audience binding)."
                    ),
                    "MCP 2025-06-18 (Access Token Usage) · RFC 6750 §2.1",
                );
                return;
            }
        };

    let tools = match client.list_tools().await {
        Ok(t) => t,
        Err(e) => {
            trace.push(
                "session",
                "Authenticated MCP session",
                Status::Fail,
                format!("the authenticated handshake succeeded but `tools/list` failed: {e}"),
                "MCP 2025-06-18 (Access Token Usage)",
            );
            let _ = client.shutdown().await;
            return;
        }
    };

    let info = client.server_info().clone();
    let protocol_version = client.protocol_version().to_string();
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    trace.push(
        "session",
        "Authenticated MCP session",
        Status::Pass,
        format!(
            "authenticated session established with {} {} (protocol {protocol_version}): {} \
             tool(s) visible",
            info.name,
            info.version,
            tool_names.len()
        ),
        "MCP 2025-06-18 (Access Token Usage)",
    );
    outcome.session = Some(AuthenticatedSession {
        server_name: info.name,
        server_version: info.version,
        protocol_version,
        tool_count: tool_names.len(),
        tool_names,
    });
    let _ = client.shutdown().await;
}

/// The scopes a login should request for a resource, given the PRM and an
/// explicit `--scope`. Pure; exposed for testing the precedence rule.
pub fn resolve_scope(explicit: Option<&str>, prm_scopes: &[String]) -> Option<String> {
    if let Some(s) = explicit.filter(|s| !s.trim().is_empty()) {
        return Some(s.to_string());
    }
    (!prm_scopes.is_empty()).then(|| prm_scopes.join(" "))
}

/// Read a `scopes_supported` array out of a raw PRM document. Thin wrapper over
/// [`crate::auth`]'s lenient array reader, kept here so the login path has one
/// obvious entry point.
pub fn scopes_from_prm(v: &Value) -> Vec<String> {
    string_array(v, "scopes_supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PKCE ------------------------------------------------------------

    #[test]
    fn s256_matches_rfc7636_appendix_b_vector() {
        // RFC 7636 Appendix B, verbatim.
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_rfc7636_shaped() {
        let pkce = generate_pkce().expect("CSPRNG available");
        let v = pkce.verifier.expose();
        // RFC 7636 §4.1: 43..=128 characters from the unreserved set.
        assert!(
            (43..=128).contains(&v.len()),
            "verifier length {} out of range",
            v.len()
        );
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
            "verifier `{v}` contains a reserved character"
        );
        assert_eq!(pkce.challenge, s256_challenge(v));
    }

    #[test]
    fn two_pkce_pairs_differ() {
        let a = generate_pkce().expect("CSPRNG");
        let b = generate_pkce().expect("CSPRNG");
        assert_ne!(a.verifier.expose(), b.verifier.expose());
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn state_is_a_fresh_256_bit_nonce() {
        let a = generate_state().expect("CSPRNG");
        let b = generate_state().expect("CSPRNG");
        assert_eq!(a.expose().len(), 43);
        assert_ne!(a.expose(), b.expose());
        assert!(a.ct_eq(a.expose()));
        assert!(!a.ct_eq(b.expose()));
    }

    #[test]
    fn ct_eq_rejects_different_lengths_without_panicking() {
        let s = Secret::new("abc");
        assert!(!s.ct_eq(""));
        assert!(!s.ct_eq("abcd"));
        assert!(s.ct_eq("abc"));
    }

    // ---- Secrets ---------------------------------------------------------

    #[test]
    fn secret_debug_never_prints_the_value() {
        let s = Secret::new("super-secret-token");
        assert_eq!(format!("{s:?}"), "<redacted>");
        // And through a struct that derives Debug — the case a stray
        // `dbg!(config)` would hit.
        #[derive(Debug)]
        struct Holder {
            token: Secret,
        }
        let holder = Holder {
            token: Secret::new("super-secret-token"),
        };
        assert_eq!(holder.token.expose(), "super-secret-token");
        let out = format!("{holder:?}");
        assert!(!out.contains("super-secret-token"), "{out}");
    }

    #[test]
    fn login_outcome_debug_never_prints_a_token() {
        let mut outcome = blank_outcome();
        outcome.access_token = Some(Secret::new("at-XYZ"));
        outcome.refresh_token = Some(Secret::new("rt-XYZ"));
        let dumped = format!("{outcome:?}");
        assert!(!dumped.contains("at-XYZ"), "{dumped}");
        assert!(!dumped.contains("rt-XYZ"), "{dumped}");
    }

    fn blank_outcome() -> LoginOutcome {
        LoginOutcome {
            url: "http://127.0.0.1:1/mcp".into(),
            canonical_resource: "http://127.0.0.1:1/mcp".into(),
            steps: Vec::new(),
            issuer: None,
            client_id: None,
            client_registered: false,
            token_type: None,
            expires_in: None,
            granted_scope: None,
            refresh_token_issued: false,
            session: None,
            exchanges: Vec::new(),
            access_token: None,
            refresh_token: None,
        }
    }

    // ---- Callback parsing ------------------------------------------------

    #[test]
    fn parses_a_success_callback() {
        let p = parse_callback_query("code=abc123&state=xyz&iss=https%3A%2F%2Fas.example.com");
        assert_eq!(p.code.as_deref(), Some("abc123"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
        assert_eq!(p.iss.as_deref(), Some("https://as.example.com"));
        assert!(p.is_authorization_response());
    }

    #[test]
    fn parses_an_error_callback_verbatim() {
        let p = parse_callback_query("error=access_denied&error_description=User+said+no");
        assert_eq!(p.error.as_deref(), Some("access_denied"));
        assert_eq!(p.error_description.as_deref(), Some("User said no"));
        assert!(p.is_authorization_response());
    }

    #[test]
    fn a_non_callback_request_is_not_an_authorization_response() {
        assert!(!parse_callback_query("").is_authorization_response());
        assert!(!parse_callback_query("foo=bar").is_authorization_response());
    }

    #[test]
    fn duplicate_parameters_take_the_first() {
        let p = parse_callback_query("code=first&code=second");
        assert_eq!(p.code.as_deref(), Some("first"));
    }

    #[test]
    fn malformed_percent_escapes_survive_literally() {
        let p = parse_callback_query("code=a%ZZb&state=%");
        assert_eq!(p.code.as_deref(), Some("a%ZZb"));
        assert_eq!(p.state.as_deref(), Some("%"));
    }

    #[test]
    fn request_line_query_extraction() {
        assert_eq!(
            query_from_request_line("GET /jig/callback?code=1&state=2 HTTP/1.1"),
            "code=1&state=2"
        );
        assert_eq!(query_from_request_line("GET /favicon.ico HTTP/1.1"), "");
        assert_eq!(query_from_request_line(""), "");
        assert_eq!(query_from_request_line("GET"), "");
    }

    // ---- Authorization URL ------------------------------------------------

    #[test]
    fn authorization_url_carries_every_required_parameter() {
        let url = build_authorization_url(
            "https://as.example.com/authorize",
            "client-1",
            "http://127.0.0.1:5555/jig/callback",
            "the-state",
            "the-challenge",
            "https://mcp.example.com/mcp",
            Some("mcp:tools"),
        )
        .expect("valid endpoint");
        let parsed = reqwest::Url::parse(&url).expect("valid URL");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client-1");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/jig/callback");
        assert_eq!(q["state"], "the-state");
        assert_eq!(q["code_challenge"], "the-challenge");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], "https://mcp.example.com/mcp");
        assert_eq!(q["scope"], "mcp:tools");
    }

    #[test]
    fn authorization_url_omits_an_empty_scope() {
        let url = build_authorization_url(
            "https://as.example.com/authorize",
            "c",
            "http://127.0.0.1:1/jig/callback",
            "s",
            "ch",
            "https://mcp.example.com",
            Some("   "),
        )
        .expect("valid");
        assert!(!url.contains("scope="), "{url}");
        let url2 = build_authorization_url(
            "https://as.example.com/authorize",
            "c",
            "http://127.0.0.1:1/jig/callback",
            "s",
            "ch",
            "https://mcp.example.com",
            None,
        )
        .expect("valid");
        assert!(!url2.contains("scope="), "{url2}");
    }

    #[test]
    fn authorization_url_preserves_an_existing_query() {
        let url = build_authorization_url(
            "https://as.example.com/authorize?tenant=acme",
            "c",
            "http://127.0.0.1:1/jig/callback",
            "s",
            "ch",
            "https://mcp.example.com",
            None,
        )
        .expect("valid");
        assert!(url.contains("tenant=acme"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
    }

    #[test]
    fn authorization_url_rejects_a_non_url_endpoint() {
        assert!(build_authorization_url("not a url", "c", "r", "s", "ch", "res", None).is_err());
    }

    // ---- Callback validation ---------------------------------------------

    fn asm_with(issuer: Option<&str>, iss_supported: bool) -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: issuer.map(str::to_string),
            authorization_endpoint: Some("https://as.example.com/authorize".into()),
            token_endpoint: Some("https://as.example.com/token".into()),
            registration_endpoint: None,
            code_challenge_methods_supported: vec!["S256".into()],
            iss_parameter_supported: iss_supported,
        }
    }

    #[test]
    fn callback_accepts_a_matching_state_and_iss() {
        let state = Secret::new("nonce");
        let params = CallbackParams {
            code: Some("the-code".into()),
            state: Some("nonce".into()),
            iss: Some("https://as.example.com".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        let code = validate_callback(
            &params,
            &state,
            &asm_with(Some("https://as.example.com"), true),
            &mut trace,
        );
        assert_eq!(code.as_deref(), Some("the-code"));
        assert_eq!(trace.steps[0].status, Status::Pass);
    }

    #[test]
    fn callback_rejects_a_mismatched_state() {
        let params = CallbackParams {
            code: Some("the-code".into()),
            state: Some("tampered".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(None, false),
            &mut trace
        )
        .is_none());
        assert_eq!(trace.steps[0].status, Status::Fail);
        assert!(trace.steps[0].message.contains("does not match"));
    }

    #[test]
    fn callback_rejects_a_missing_state() {
        let params = CallbackParams {
            code: Some("the-code".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(None, false),
            &mut trace
        )
        .is_none());
        assert_eq!(trace.steps[0].status, Status::Fail);
    }

    #[test]
    fn callback_rejects_a_mismatched_iss() {
        let params = CallbackParams {
            code: Some("the-code".into()),
            state: Some("nonce".into()),
            iss: Some("https://evil.example.com".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(Some("https://as.example.com"), true),
            &mut trace
        )
        .is_none());
        assert_eq!(trace.steps[0].status, Status::Fail);
        assert!(trace.steps[0].message.contains("RFC 9207"));
        assert_eq!(trace.steps[0].citation, "RFC 9207 §2.4");
    }

    #[test]
    fn callback_requires_iss_when_the_as_promised_one() {
        let params = CallbackParams {
            code: Some("the-code".into()),
            state: Some("nonce".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(Some("https://as.example.com"), true),
            &mut trace
        )
        .is_none());
        assert_eq!(trace.steps[0].status, Status::Fail);
    }

    #[test]
    fn callback_allows_a_missing_iss_when_unsupported() {
        let params = CallbackParams {
            code: Some("the-code".into()),
            state: Some("nonce".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(Some("https://as.example.com"), false),
            &mut trace
        )
        .is_some());
    }

    #[test]
    fn callback_surfaces_an_error_response_verbatim() {
        let params = CallbackParams {
            error: Some("access_denied".into()),
            error_description: Some("the user declined".into()),
            ..Default::default()
        };
        let mut trace = Trace::new();
        assert!(validate_callback(
            &params,
            &Secret::new("nonce"),
            &asm_with(None, false),
            &mut trace
        )
        .is_none());
        assert!(trace.steps[0].message.contains("access_denied"));
        assert!(trace.steps[0].message.contains("the user declined"));
    }

    // ---- Scope -----------------------------------------------------------

    #[test]
    fn explicit_scope_beats_the_prm() {
        let prm = vec!["mcp:tools".to_string(), "mcp:resources".to_string()];
        assert_eq!(
            resolve_scope(Some("only:this"), &prm).as_deref(),
            Some("only:this")
        );
        assert_eq!(
            resolve_scope(None, &prm).as_deref(),
            Some("mcp:tools mcp:resources")
        );
        assert_eq!(resolve_scope(None, &[]), None);
        assert_eq!(resolve_scope(Some("  "), &[]), None);
    }

    #[test]
    fn scopes_read_leniently_from_a_prm_document() {
        let v = json!({ "scopes_supported": ["a", 7, "b"] });
        assert_eq!(scopes_from_prm(&v), vec!["a", "b"]);
        assert!(scopes_from_prm(&json!({})).is_empty());
    }

    // ---- The loopback listener --------------------------------------------

    #[tokio::test]
    async fn loopback_binds_an_ephemeral_port_and_serves_one_callback() {
        let loopback = bind_loopback().await.expect("bind");
        let uri = loopback.redirect_uri().to_string();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with(CALLBACK_PATH), "{uri}");

        let target = uri.clone();
        let driver = tokio::spawn(async move {
            let url = format!("{target}?code=CODE&state=STATE&iss=https%3A%2F%2Fas");
            reqwest::get(&url).await.map(|r| r.status().as_u16())
        });

        let params = wait_for_callback(loopback, Duration::from_secs(10))
            .await
            .expect("callback arrives");
        assert_eq!(params.code.as_deref(), Some("CODE"));
        assert_eq!(params.state.as_deref(), Some("STATE"));
        assert_eq!(params.iss.as_deref(), Some("https://as"));
        assert_eq!(driver.await.expect("join").expect("http"), 200);
    }

    #[tokio::test]
    async fn loopback_ignores_a_non_callback_request_and_keeps_waiting() {
        let loopback = bind_loopback().await.expect("bind");
        let base = loopback
            .redirect_uri()
            .trim_end_matches(CALLBACK_PATH)
            .to_string();
        let uri = loopback.redirect_uri().to_string();

        tokio::spawn(async move {
            // A browser's speculative favicon fetch must not be mistaken for the
            // authorization response.
            let _ = reqwest::get(format!("{base}/favicon.ico")).await;
            let _ = reqwest::get(format!("{uri}?code=REAL&state=S")).await;
        });

        let params = wait_for_callback(loopback, Duration::from_secs(10))
            .await
            .expect("the real callback still arrives");
        assert_eq!(params.code.as_deref(), Some("REAL"));
    }

    #[tokio::test]
    async fn loopback_times_out_with_a_clear_message() {
        let loopback = bind_loopback().await.expect("bind");
        let err = wait_for_callback(loopback, Duration::from_millis(50))
            .await
            .expect_err("must time out");
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("--timeout"), "{err}");
    }
}
