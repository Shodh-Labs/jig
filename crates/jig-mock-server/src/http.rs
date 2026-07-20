//! The server side of the MCP **Streamable HTTP** transport (`2025-06-18`),
//! implemented with axum as a test fixture for Jig's `HttpTransport`.
//!
//! Single MCP endpoint at `/mcp` supporting POST (client messages), GET
//! (returns 405 by default — no standalone server stream — or, under the
//! push flags, a server→client SSE stream), and DELETE (session termination).
//! It reuses the same JSON-RPC handlers as the stdio server, so the two
//! transports expose an identical MCP surface.
//!
//! Fixture behaviours, all driven by [`HttpConfig`] parsed in `main`:
//! * JSON response mode (default): each request answered with one
//!   `application/json` object.
//! * SSE response mode (`--sse`): each request answered with a
//!   `text/event-stream` body; the `tools/list` response is preceded by a
//!   pushed `notifications/message` so a client's notification capture can be
//!   asserted.
//! * `--resources-prompts`: advertise and serve `resources` and `prompts`.
//! * Session issuance + enforcement: `initialize` issues an `Mcp-Session-Id`;
//!   every later request must echo it or receive HTTP 404.
//! * `--expire-after-initialize`: issue the session, then 404 every
//!   post-handshake request — the client's session-expiry path.
//! * `--push-notifications <n>` / `--server-ping` / `--server-sampling`: make
//!   the standalone GET stream real — push `n` notifications and/or a
//!   server→client `ping`/`sampling/createMessage` request, so a client's
//!   GET-stream handling (capture + reply policy) can be asserted.
//! * `--giant-json` / `--giant-sse`: answer `tools/list` with a multi-megabyte
//!   body (as one JSON object, or one giant SSE event) to exercise the client's
//!   streaming size-cap enforcement.
//!
//! Any JSON-RPC *response* the client POSTs back (its reply to a server→client
//! request) is answered `202 Accepted` and echoed to stderr as
//! `observed-reply: <json>`, so an integration test can confirm the reply
//! actually arrived at the server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

/// Header carrying the session id, both directions.
const SESSION_HEADER: &str = "Mcp-Session-Id";
/// The single MCP endpoint path.
const MCP_ENDPOINT: &str = "/mcp";
/// RFC 9728 Protected Resource Metadata well-known path (root form).
const PRM_PATH: &str = "/.well-known/oauth-protected-resource";
/// RFC 9728 Protected Resource Metadata well-known path (path-appended form).
const PRM_PATH_APPENDED: &str = "/.well-known/oauth-protected-resource/mcp";
/// RFC 8414 Authorization Server Metadata well-known path.
const ASM_PATH: &str = "/.well-known/oauth-authorization-server";
/// RFC 7591 Dynamic Client Registration endpoint.
const REGISTER_PATH: &str = "/register";
/// OAuth 2.1 authorization endpoint.
const AUTHORIZE_PATH: &str = "/authorize";
/// OAuth 2.1 token endpoint.
const TOKEN_PATH: &str = "/token";
/// The scope the fixture AS grants, echoed on the token response.
const GRANTED_SCOPE: &str = "mcp:tools mcp:resources";

/// The OAuth conformance scenario the HTTP fixture plays, selected with
/// `--auth <scenario>`. Each drives a specific, deterministic auth surface so an
/// integration test can assert the exact findings `jig auth` produces.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// No auth flag: the server requires no authentication (existing behaviour).
    #[default]
    Off,
    /// `--auth open`: a `200` to the unauthenticated probe (explicitly open).
    Open,
    /// `--auth well-configured`: 401 + proper Bearer challenge + full RFC 9728
    /// and RFC 8414 metadata (S256 + DCR + iss).
    WellConfigured,
    /// `--auth no-challenge`: a bare 401 with no `WWW-Authenticate` and no
    /// metadata endpoints.
    NoChallenge,
    /// `--auth no-metadata`: a proper challenge whose `resource_metadata` URL
    /// 404s (points nowhere).
    NoMetadata,
    /// `--auth no-pkce`: full metadata, but the auth-server metadata omits the
    /// REQUIRED `S256` PKCE method.
    NoPkce,
    /// `--auth login-happy`: a working authorization server — `/register`,
    /// `/authorize`, `/token` — plus an MCP endpoint that accepts only the
    /// access token this server actually issued. The `jig auth --login` flow
    /// runs end to end against it.
    LoginHappy,
    /// `--auth login-bad-state`: as `login-happy`, but the authorization
    /// response echoes a `state` that is not the one the client sent — the CSRF
    /// case a client MUST reject (OAuth 2.1 §7.12).
    LoginBadState,
    /// `--auth login-bad-iss`: as `login-happy`, but the authorization response
    /// carries an `iss` naming a different issuer — the authorization-server
    /// mix-up case a client MUST reject (RFC 9207 §2.4).
    LoginBadIss,
    /// `--auth login-no-s256`: the authorization server advertises only the
    /// `plain` PKCE method, so a conforming client must refuse to start the
    /// flow rather than downgrade.
    LoginNoS256,
    /// `--auth login-token-error`: authorization succeeds, but the token
    /// endpoint answers `400 invalid_grant` — the client must surface the
    /// server's own error verbatim.
    LoginTokenError,
}

impl AuthMode {
    /// Parse `--auth <scenario>` from the raw arg list.
    pub fn parse(args: &[String]) -> AuthMode {
        let idx = match args.iter().position(|a| a == "--auth") {
            Some(i) => i,
            None => return AuthMode::Off,
        };
        match args.get(idx + 1).map(String::as_str) {
            Some("open") => AuthMode::Open,
            Some("well-configured") => AuthMode::WellConfigured,
            Some("no-challenge") => AuthMode::NoChallenge,
            Some("no-metadata") => AuthMode::NoMetadata,
            Some("no-pkce") => AuthMode::NoPkce,
            Some("login-happy") => AuthMode::LoginHappy,
            Some("login-bad-state") => AuthMode::LoginBadState,
            Some("login-bad-iss") => AuthMode::LoginBadIss,
            Some("login-no-s256") => AuthMode::LoginNoS256,
            Some("login-token-error") => AuthMode::LoginTokenError,
            _ => AuthMode::Off,
        }
    }

    /// Whether this scenario challenges unauthenticated requests with a 401.
    fn requires_auth(self) -> bool {
        !matches!(self, AuthMode::Off | AuthMode::Open)
    }

    /// Whether this scenario serves RFC 9728 / RFC 8414 metadata documents.
    fn serves_metadata(self) -> bool {
        matches!(self, AuthMode::WellConfigured | AuthMode::NoPkce) || self.is_login()
    }

    /// Whether this scenario runs the live authorization server (`/register`,
    /// `/authorize`, `/token`).
    fn is_login(self) -> bool {
        matches!(
            self,
            AuthMode::LoginHappy
                | AuthMode::LoginBadState
                | AuthMode::LoginBadIss
                | AuthMode::LoginNoS256
                | AuthMode::LoginTokenError
        )
    }

    /// Whether the authorization server advertises (and requires) PKCE `S256`.
    /// `login-no-s256` advertises `plain` alone so a client must refuse.
    fn advertises_s256(self) -> bool {
        !matches!(self, AuthMode::NoPkce | AuthMode::LoginNoS256)
    }
}

/// Flags controlling the HTTP fixture's behaviour, parsed once in `main`.
#[derive(Clone, Copy, Default)]
pub struct HttpConfig {
    /// Respond with SSE streams (`--sse`) rather than single JSON objects.
    pub sse: bool,
    /// Issue a session on initialize, then 404 every later request (`--expire`).
    pub expire: bool,
    /// Advertise and serve `resources` and `prompts` (`--resources-prompts`).
    pub resources_prompts: bool,
    /// Push this many notifications on the standalone GET stream.
    pub push_notifications: usize,
    /// Push a server→client `ping` request on the GET stream.
    pub server_ping: bool,
    /// Push a server→client `sampling/createMessage` request on the GET stream.
    pub server_sampling: bool,
    /// Answer `tools/list` with a multi-megabyte single JSON object.
    pub giant_json: bool,
    /// Answer `tools/list` with a single multi-megabyte SSE event.
    pub giant_sse: bool,
    /// The OAuth conformance scenario to play (`--auth <scenario>`).
    pub auth: AuthMode,
}

/// An authorization code the fixture AS has issued but not yet redeemed, with
/// everything RFC 7636 §4.5 and OAuth 2.1 §4.1.3 require it to be checked
/// against at the token endpoint.
struct PendingCode {
    /// The `code_challenge` from the authorization request.
    challenge: String,
    /// The `redirect_uri` from the authorization request; the token request
    /// MUST present the identical value.
    redirect_uri: String,
    /// The RFC 8707 `resource` the token will be audience-bound to.
    resource: String,
    /// The granted scope, echoed on the token response.
    scope: String,
}

/// Shared server state.
struct AppState {
    cfg: HttpConfig,
    /// The currently-issued session id (set on initialize, cleared on DELETE).
    session: Mutex<Option<String>>,
    /// Authorization codes issued by `/authorize`, awaiting redemption.
    codes: Mutex<std::collections::HashMap<String, PendingCode>>,
    /// Access tokens `/token` has issued. The MCP endpoint accepts these and
    /// nothing else under the `login-*` scenarios, so a passing test proves the
    /// flow actually minted the token it is presenting.
    issued_tokens: Mutex<std::collections::HashSet<String>>,
}

/// Run the HTTP server on `127.0.0.1:<port>` until the process is killed. Builds
/// its own Tokio runtime so `main` can stay synchronous for the stdio path.
///
/// Pass port `0` to bind an OS-assigned ephemeral port; the actual port is read
/// back from the bound listener and reported in the announcement line, so a test
/// can parse it rather than racily pre-selecting a port.
pub fn serve(port: u16, cfg: HttpConfig) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");
    rt.block_on(async move {
        let state = Arc::new(AppState {
            cfg,
            session: Mutex::new(None),
            codes: Mutex::new(std::collections::HashMap::new()),
            issued_tokens: Mutex::new(std::collections::HashSet::new()),
        });
        let app = Router::new()
            .route(
                MCP_ENDPOINT,
                post(handle_post).get(handle_get).delete(handle_delete),
            )
            // RFC 9728 / RFC 8414 discovery documents (served under --auth).
            .route(PRM_PATH, get(handle_protected_resource_metadata))
            .route(PRM_PATH_APPENDED, get(handle_protected_resource_metadata))
            .route(ASM_PATH, get(handle_auth_server_metadata))
            // The authorization server proper (served under --auth login-*).
            .route(REGISTER_PATH, post(handle_register))
            .route(AUTHORIZE_PATH, get(handle_authorize))
            .route(TOKEN_PATH, post(handle_token))
            .with_state(state);
        // A diagnostic fixture must not panic-dump on a busy port: report the
        // failure cleanly and exit non-zero instead.
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("jig-mock-server: failed to bind HTTP port {port}: {e}");
                std::process::exit(1);
            }
        };
        // Announce the *actual* bound port (which differs from `port` when 0 was
        // requested). The format is stable — tests parse the `127.0.0.1:<port>`.
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        eprintln!(
            "jig-mock-server: HTTP MCP endpoint on http://127.0.0.1:{port}{MCP_ENDPOINT} \
             (sse={}, expire={})",
            cfg.sse, cfg.expire
        );
        axum::serve(listener, app).await.expect("HTTP server error");
    });
}

/// Lock helper tolerant of poisoning (a test fixture must not cascade-panic).
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// POST `/mcp`: the client sending a JSON-RPC message.
async fn handle_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // OAuth conformance scenarios (--auth): challenge any unauthenticated
    // request with the scenario's 401 before doing any MCP work. A request that
    // carries a non-empty `Authorization: Bearer` header is treated as
    // authenticated and falls through to normal handling — this is what the
    // `jig auth` header-passthrough probe exercises.
    if state.cfg.auth.requires_auth() && !is_authorized(&state, &headers) {
        return auth_challenge(&state, &headers);
    }

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return text_status(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}")),
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // A JSON-RPC *response* the client POSTs back (its reply to a server→client
    // request): it carries `result`/`error` and no `method`. Per spec: answer
    // 202 Accepted with no body. Echo it to stderr so a test can confirm the
    // reply arrived.
    if method.is_empty() && (req.get("result").is_some() || req.get("error").is_some()) {
        eprintln!(
            "jig-mock-server: observed-reply: {}",
            serde_json::to_string(&req).unwrap_or_default()
        );
        return accepted();
    }

    // A notification (no id): per spec, 202 Accepted with an empty body.
    if id.is_none() {
        return accepted();
    }
    let id = id.unwrap_or(Value::Null);

    // initialize: issue a fresh session and return the InitializeResult with the
    // Mcp-Session-Id header.
    if method == "initialize" {
        let session = new_session_id();
        *locked(&state.session) = Some(session.clone());
        let msg = crate::handle_initialize(id, state.cfg.resources_prompts);
        return respond(&state, vec![msg], Some(&session));
    }

    // Every post-handshake request must carry the issued session id.
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = locked(&state.session).clone();
    let session_ok = known.is_some() && known == provided;
    if state.cfg.expire || !session_ok {
        // 404: expired (--expire), unknown, or missing session id.
        return text_status(StatusCode::NOT_FOUND, "session not found");
    }

    let messages = match method {
        "tools/list" => return tools_list_response(&state, id),
        "tools/call" => vec![crate::handle_tools_call(id, req.get("params"))],
        "resources/list" if state.cfg.resources_prompts => {
            vec![crate::handle_resources_list(id)]
        }
        "resources/read" if state.cfg.resources_prompts => {
            vec![crate::handle_resources_read(id, req.get("params"))]
        }
        "prompts/list" if state.cfg.resources_prompts => vec![crate::handle_prompts_list(id)],
        "prompts/get" if state.cfg.resources_prompts => {
            vec![crate::handle_prompts_get(id, req.get("params"))]
        }
        other => vec![crate::error_response(
            id,
            -32601,
            &format!("Method not found: {other}"),
        )],
    };
    respond(&state, messages, None)
}

/// Build the `tools/list` response, honouring the giant-body fixtures.
fn tools_list_response(state: &AppState, id: Value) -> Response {
    // A giant single JSON object (streaming size-cap fixture), regardless of
    // the SSE flag.
    if state.cfg.giant_json {
        let msg = crate::handle_tools_list_giant(id);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&msg).unwrap_or_else(|_| "null".to_string()),
            ))
            .unwrap();
    }
    // A single giant SSE event (streaming per-event size-cap fixture).
    if state.cfg.giant_sse {
        let msg = crate::handle_tools_list_giant(id);
        let body = sse_event(&msg);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }

    let response = crate::handle_tools_list(id);
    let messages = if state.cfg.sse {
        // Push a server notification ahead of the response so the client
        // records-and-ignores it exactly as it would over stdio.
        vec![pushed_notification(0), response]
    } else {
        vec![response]
    };
    respond(state, messages, None)
}

/// GET `/mcp`: the standalone server→client stream.
///
/// By default we offer no such stream, so per spec we return 405. Under the
/// push flags we open a real `text/event-stream`, push the configured
/// notifications and/or server→client requests, and then close it (the server
/// MAY close the stream at any time). The client captures every message and
/// replies to the requests.
async fn handle_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let cfg = state.cfg;
    let pushes_anything = cfg.push_notifications > 0 || cfg.server_ping || cfg.server_sampling;
    if !pushes_anything {
        return text_status(
            StatusCode::METHOD_NOT_ALLOWED,
            "this server does not offer a standalone SSE stream",
        );
    }

    // The stream requires a valid session, just like POST requests.
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = locked(&state.session).clone();
    if known.is_none() || known != provided {
        return text_status(StatusCode::NOT_FOUND, "session not found");
    }

    let mut body = String::new();
    for i in 0..cfg.push_notifications {
        body.push_str(&sse_event(&pushed_notification(i)));
    }
    if cfg.server_ping {
        body.push_str(&sse_event(&json!({
            "jsonrpc": "2.0",
            "id": server_request_id(),
            "method": "ping"
        })));
    }
    if cfg.server_sampling {
        body.push_str(&sse_event(&json!({
            "jsonrpc": "2.0",
            "id": server_request_id(),
            "method": "sampling/createMessage",
            "params": {
                "messages": [
                    { "role": "user", "content": { "type": "text", "text": "hello?" } }
                ],
                "maxTokens": 16
            }
        })));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

/// DELETE `/mcp`: explicit session termination. Clears the session if the id
/// matches; 404 otherwise.
async fn handle_delete(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let provided = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut guard = locked(&state.session);
    if guard.is_some() && *guard == provided {
        *guard = None;
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    } else {
        text_status(StatusCode::NOT_FOUND, "session not found")
    }
}

// ---------------------------------------------------------------------------
// OAuth conformance fixtures (--auth <scenario>)
// ---------------------------------------------------------------------------

/// The `Authorization: Bearer <token>` credential, if the header carries one.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("authorization")?.to_str().ok()?.trim();
    let (scheme, token) = raw.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then(|| token.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Whether a request may proceed past the auth gate.
///
/// Under the conformance scenarios any non-empty Bearer token is accepted —
/// they exist to exercise the *challenge*, and `jig auth` never mints a token
/// for them. Under the `login-*` scenarios the token must be one this server's
/// own `/token` endpoint issued, so a green integration test is proof the flow
/// really ran rather than proof a placeholder string was accepted (MCP
/// 2025-06-18: a server MUST only accept tokens issued for it).
fn is_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    match bearer_token(headers) {
        None => false,
        Some(token) => !state.cfg.auth.is_login() || locked(&state.issued_tokens).contains(&token),
    }
}

/// The `scheme://host` origin of this request, from the `Host` header.
fn request_origin(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    // The fixture always serves plain HTTP.
    format!("http://{host}")
}

/// The 401 challenge for the active scenario. `well-configured`, `no-metadata`,
/// and `no-pkce` send a proper RFC 6750 / RFC 9728 §5.1 Bearer challenge; the
/// `no-challenge` scenario sends a bare 401 with no `WWW-Authenticate`.
fn auth_challenge(state: &AppState, headers: &HeaderMap) -> Response {
    if state.cfg.auth == AuthMode::NoChallenge {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from("Unauthorized"))
            .unwrap();
    }
    let origin = request_origin(headers);
    let resource_metadata = format!("{origin}{PRM_PATH}");
    let challenge = format!(
        "Bearer resource_metadata=\"{resource_metadata}\", error=\"invalid_token\", \
         error_description=\"authentication required\""
    );
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", challenge)
        .body(Body::from("Unauthorized"))
        .unwrap()
}

/// `GET /.well-known/oauth-protected-resource[/mcp]` (RFC 9728). Served only by
/// the scenarios that advertise metadata; otherwise 404 (points nowhere).
async fn handle_protected_resource_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !state.cfg.auth.serves_metadata() {
        return text_status(StatusCode::NOT_FOUND, "no protected-resource metadata");
    }
    let origin = request_origin(&headers);
    let doc = json!({
        "resource": format!("{origin}{MCP_ENDPOINT}"),
        "authorization_servers": [origin],
        "scopes_supported": ["mcp:tools", "mcp:resources"],
        "bearer_methods_supported": ["header"]
    });
    json_response(&doc)
}

/// `GET /.well-known/oauth-authorization-server` (RFC 8414). The `no-pkce`
/// scenario omits the REQUIRED `S256` method; `well-configured` includes S256,
/// DCR, and the RFC 9207 `iss` parameter.
async fn handle_auth_server_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !state.cfg.auth.serves_metadata() {
        return text_status(StatusCode::NOT_FOUND, "no authorization-server metadata");
    }
    let origin = request_origin(&headers);
    let pkce_methods: Vec<&str> = if state.cfg.auth.advertises_s256() {
        vec!["S256"]
    } else {
        vec!["plain"]
    };
    let doc = json!({
        "issuer": origin,
        "authorization_endpoint": format!("{origin}{AUTHORIZE_PATH}"),
        "token_endpoint": format!("{origin}{TOKEN_PATH}"),
        "registration_endpoint": format!("{origin}{REGISTER_PATH}"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": pkce_methods,
        "authorization_response_iss_parameter_supported": true
    });
    json_response(&doc)
}

// ---------------------------------------------------------------------------
// The fixture authorization server (--auth login-*)
// ---------------------------------------------------------------------------

/// `POST /register` — RFC 7591 Dynamic Client Registration.
///
/// Accepts any well-formed metadata document and issues a public client id.
/// The one thing it insists on is `redirect_uris`: an AS that registered a
/// client without one could not later validate the redirect, which is the whole
/// point of registration (MCP 2025-06-18, Open Redirection).
async fn handle_register(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    if !state.cfg.auth.is_login() {
        return text_status(StatusCode::NOT_FOUND, "no registration endpoint");
    }
    let doc: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "the registration request body is not JSON",
            )
        }
    };
    let redirect_uris = doc.get("redirect_uris").and_then(Value::as_array);
    if redirect_uris.map(|a| a.is_empty()).unwrap_or(true) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris is required and must be non-empty",
        );
    }
    let client_id = format!("jig-dcr-client-{}", next_counter());
    let mut out = json!({
        "client_id": client_id,
        "client_id_issued_at": 1_700_000_000,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(name) = doc.get("client_name") {
        out["client_name"] = name.clone();
    }
    Response::builder()
        .status(StatusCode::CREATED)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&out).unwrap_or_else(|_| "null".to_string()),
        ))
        .unwrap()
}

/// `GET /authorize` — the OAuth 2.1 authorization endpoint.
///
/// There is no user interaction to simulate, so consent is implicit; what the
/// fixture *does* do is validate that the client sent a spec-shaped request
/// (`response_type=code`, a `redirect_uri`, a CSRF `state`, and an S256 PKCE
/// challenge) before redirecting. A client that forgot any of them gets a 400
/// rather than a working flow.
async fn handle_authorize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    if !state.cfg.auth.is_login() {
        return text_status(StatusCode::NOT_FOUND, "no authorization endpoint");
    }
    let q = parse_form(&query.unwrap_or_default());
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let Some(redirect_uri) = get("redirect_uri").filter(|s| !s.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing redirect_uri",
        );
    };
    // Everything after this point can be reported *through* the redirect, per
    // OAuth 2.1 §4.1.2.1, but the fixture keeps it simple: a malformed request
    // is a 400 the test can read directly.
    if get("response_type").as_deref() != Some("code") {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            "response_type must be `code`",
        );
    }
    if get("client_id").filter(|s| !s.is_empty()).is_none() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing client_id",
        );
    }
    let Some(client_state) = get("state").filter(|s| !s.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing state — this authorization server requires the CSRF nonce",
        );
    };
    let Some(challenge) = get("code_challenge").filter(|s| !s.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing code_challenge — PKCE is required",
        );
    };
    let method = get("code_challenge_method").unwrap_or_default();
    if state.cfg.auth.advertises_s256() && method != "S256" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_challenge_method must be S256",
        );
    }

    let code = format!("jig-code-{}", next_counter());
    let scope = get("scope").unwrap_or_else(|| GRANTED_SCOPE.to_string());
    locked(&state.codes).insert(
        code.clone(),
        PendingCode {
            challenge,
            redirect_uri: redirect_uri.clone(),
            resource: get("resource").unwrap_or_default(),
            scope,
        },
    );

    // The failure scenarios differ only here, in what the authorization
    // *response* says — which is exactly where a client's validation lives.
    let echoed_state = if state.cfg.auth == AuthMode::LoginBadState {
        "not-the-state-you-sent".to_string()
    } else {
        client_state
    };
    let issuer = if state.cfg.auth == AuthMode::LoginBadIss {
        "https://mixup.example.net".to_string()
    } else {
        request_origin(&headers)
    };

    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let location = format!(
        "{redirect_uri}{sep}code={}&state={}&iss={}",
        percent_encode(&code),
        percent_encode(&echoed_state),
        percent_encode(&issuer)
    );
    Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", location)
        .header("Cache-Control", "no-store")
        .body(Body::empty())
        .unwrap()
}

/// `POST /token` — the OAuth 2.1 token endpoint.
///
/// Verifies the PKCE proof for real: SHA-256 the presented `code_verifier`,
/// base64url it, and compare against the challenge stored at `/authorize`
/// (RFC 7636 §4.6). A client that sent a `plain` verifier, reused a challenge,
/// or skipped the verifier entirely is rejected with `invalid_grant`.
async fn handle_token(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    if !state.cfg.auth.is_login() {
        return text_status(StatusCode::NOT_FOUND, "no token endpoint");
    }
    let form = parse_form(&String::from_utf8_lossy(&body));
    let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    if get("grant_type").as_deref() != Some("authorization_code") {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code is supported",
        );
    }
    let code = get("code").unwrap_or_default();
    // Codes are single-use (OAuth 2.1 §4.1.3): remove on redemption.
    let Some(pending) = locked(&state.codes).remove(&code) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "unknown or already-redeemed authorization code",
        );
    };
    if get("redirect_uri").as_deref() != Some(pending.redirect_uri.as_str()) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri does not match the authorization request",
        );
    }
    let verifier = get("code_verifier").unwrap_or_default();
    if verifier.len() < 43 || verifier.len() > 128 {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code_verifier must be 43-128 characters (RFC 7636 §4.1)",
        );
    }
    if s256(&verifier) != pending.challenge {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the code_verifier does not hash to the code_challenge",
        );
    }

    // The scenario whose whole point is a token-endpoint failure. Everything
    // above still ran, so the client is proven to have reached a valid state
    // before being told no.
    if state.cfg.auth == AuthMode::LoginTokenError {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "this authorization code was issued to a different client",
        );
    }

    let token = format!("jig-access-token-{}", next_counter());
    locked(&state.issued_tokens).insert(token.clone());
    let doc = json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": pending.scope,
        "refresh_token": format!("jig-refresh-token-{}", next_counter()),
        // Echoed back so a test can confirm the RFC 8707 audience made the
        // round trip.
        "resource": pending.resource,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-store")
        .body(Body::from(
            serde_json::to_string(&doc).unwrap_or_else(|_| "null".to_string()),
        ))
        .unwrap()
}

/// An RFC 6749 §5.2 / RFC 7591 §3.2.2 error object.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let doc = json!({ "error": error, "error_description": description });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&doc).unwrap_or_else(|_| "null".to_string()),
        ))
        .unwrap()
}

/// `BASE64URL-ENCODE(SHA256(ASCII(verifier)))` — RFC 7636 §4.2, server side.
fn s256(verifier: &str) -> String {
    use base64::Engine as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// Parse an `application/x-www-form-urlencoded` string (a query string or a
/// form body) into ordered `(name, value)` pairs. Total over arbitrary input.
fn parse_form(input: &str) -> Vec<(String, String)> {
    input
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(pair), String::new()),
        })
        .collect()
}

/// Decode one form-encoded component (`+` → space, `%XX` → byte). A malformed
/// escape is kept literally rather than dropped.
fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Percent-encode a query-parameter value, escaping everything outside the
/// unreserved set so the redirect `Location` is unambiguous.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A monotonically increasing counter for code/token/client ids.
fn next_counter() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A `200 application/json` response carrying `doc`.
fn json_response(doc: &Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(doc).unwrap_or_else(|_| "null".to_string()),
        ))
        .unwrap()
}

/// Render `messages` as either a single JSON object or an SSE stream, attaching
/// the session id header when provided.
fn respond(state: &AppState, messages: Vec<Value>, session: Option<&str>) -> Response {
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(s) = session {
        builder = builder.header(SESSION_HEADER, s);
    }

    if state.cfg.sse {
        let mut body = String::new();
        for m in &messages {
            body.push_str(&sse_event(m));
        }
        builder
            .header("Content-Type", "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    } else {
        // A single-object JSON reply carries only the response (the last
        // message); pushed notifications require SSE and are omitted here.
        let last = messages.last().cloned().unwrap_or(Value::Null);
        builder
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&last).unwrap_or_else(|_| "null".to_string()),
            ))
            .unwrap()
    }
}

/// Serialize one JSON-RPC message as an SSE `event: message` frame.
fn sse_event(message: &Value) -> String {
    format!(
        "event: message\ndata: {}\n\n",
        serde_json::to_string(message).unwrap_or_else(|_| "{}".to_string())
    )
}

/// A 202 Accepted with an empty body (the spec reply to a client notification
/// or response).
fn accepted() -> Response {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(Body::empty())
        .unwrap()
}

/// A server notification, numbered so multiple pushes are distinguishable.
fn pushed_notification(n: usize) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": "info", "data": format!("push #{n} from the SSE stream") }
    })
}

/// A unique id for a server→client request pushed on the GET stream.
fn server_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("srv-req-{n}")
}

/// Generate a unique, visible-ASCII session id (spec: 0x21-0x7E).
fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("jig-sess-{nanos:x}-{n:x}")
}

/// A plain-text response with a given status.
fn text_status(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .unwrap()
}
