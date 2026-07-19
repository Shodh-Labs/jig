//! A **scripted mock model provider** — the test double for `jig bench`.
//!
//! `jig bench` talks to a real model API. To exercise the full bench flow with
//! zero network and zero keys, this axum server impersonates *both* provider
//! dialects (Anthropic Messages API and OpenAI Chat Completions) and returns a
//! pre-scripted response chosen by a path segment in the base URL.
//!
//! A test points `jig bench --base-url http://127.0.0.1:<port>/<scenario>` at
//! this server; the bench engine then appends `/v1/messages` or
//! `/v1/chat/completions`, so the routes are `/{scenario}/v1/messages` and
//! `/{scenario}/v1/chat/completions`. The `{scenario}` selects which canned
//! outcome to produce, letting one server cover every branch of the outcome
//! taxonomy deterministically.
//!
//! Scenarios (dialect-aware — each returns the correct shape for the endpoint):
//!
//! * `selected` — a tool call picking `echo` with valid args.
//! * `reservation` — a tool call picking `make_reservation` with valid nested +
//!   enum args (exercises the arg validator against a real nested schema).
//! * `bad_args` — `make_reservation` with a wrong-typed field, a bad enum value,
//!   and a missing required field (arg validation must flag all three).
//! * `no_tool` — a text-only answer (no tool call).
//! * `hallucinated` — a tool call naming a tool the server does not expose.
//! * `malformed_args` — (OpenAI) `arguments` that are not valid JSON; the client
//!   must record args-unparseable, never panic.
//! * `retry_then_success` — first request `429` with `Retry-After: 0`, then a
//!   `selected` success (exercises bounded retry).
//! * `error_500` — always `500` (exhausts retries → `provider_error`).
//! * `error_429` — always `429` (exhausts retries → `provider_error`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

/// Per-process attempt counter, so the `retry_then_success` scenario can fail
/// the first hit and succeed thereafter without any shared client state.
struct ProviderState {
    hits: AtomicU64,
}

/// Run the mock provider on `127.0.0.1:<port>` until the process is killed.
pub fn serve(port: u16) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");
    rt.block_on(async move {
        let state = Arc::new(ProviderState {
            hits: AtomicU64::new(0),
        });
        let app = Router::new()
            .route("/{scenario}/v1/messages", post(anthropic))
            .route("/{scenario}/v1/chat/completions", post(openai))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("failed to bind provider port");
        eprintln!("jig-mock-server: mock provider on http://127.0.0.1:{port}/<scenario>/v1/...");
        axum::serve(listener, app)
            .await
            .expect("provider serve error");
    });
}

/// A JSON `200` response.
fn ok_json(body: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A plain error response with an optional `Retry-After` header.
fn err(status: StatusCode, retry_after: Option<u64>, msg: &str) -> Response {
    let mut b = Response::builder().status(status);
    if let Some(secs) = retry_after {
        b = b.header("retry-after", secs.to_string());
    }
    b.body(Body::from(format!("{{\"error\":\"{msg}\"}}")))
        .unwrap()
}

/// Anthropic Messages API endpoint.
async fn anthropic(
    State(state): State<Arc<ProviderState>>,
    Path(scenario): Path<String>,
) -> Response {
    if let Some(resp) = error_scenario(&state, &scenario) {
        return resp;
    }
    ok_json(anthropic_body(&scenario))
}

/// OpenAI Chat Completions endpoint.
async fn openai(State(state): State<Arc<ProviderState>>, Path(scenario): Path<String>) -> Response {
    if let Some(resp) = error_scenario(&state, &scenario) {
        return resp;
    }
    ok_json(openai_body(&scenario))
}

/// Handle the error/retry scenarios shared by both dialects. Returns `Some` when
/// the scenario is an error/retry one (and should short-circuit), `None` for a
/// normal success scenario.
fn error_scenario(state: &ProviderState, scenario: &str) -> Option<Response> {
    match scenario {
        "error_500" => Some(err(StatusCode::INTERNAL_SERVER_ERROR, None, "boom")),
        "error_429" => Some(err(StatusCode::TOO_MANY_REQUESTS, Some(0), "slow down")),
        "retry_then_success" => {
            let n = state.hits.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Some(err(StatusCode::TOO_MANY_REQUESTS, Some(0), "retry please"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The Anthropic response body for a success scenario. Unknown scenarios default
/// to `selected` so a typo fails loudly in the assertion, not the transport.
fn anthropic_body(scenario: &str) -> Value {
    let usage = json!({ "input_tokens": 42, "output_tokens": 7 });
    let base = |content: Value| {
        json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "model": "claude-mock-1",
            "content": content,
            "usage": usage,
        })
    };
    match scenario {
        "no_tool" => {
            let mut v = base(json!([
                { "type": "text", "text": "No suitable tool for this task, answering directly." }
            ]));
            v["stop_reason"] = json!("end_turn");
            v
        }
        "hallucinated" => tool_use_anthropic(base, "no_such_tool", json!({ "q": "x" })),
        "reservation" => tool_use_anthropic(
            base,
            "make_reservation",
            json!({ "party": { "size": 2, "seating": "outdoor" }, "date": "2026-01-01" }),
        ),
        "bad_args" => tool_use_anthropic(
            base,
            "make_reservation",
            // size wrong type, seating bad enum, `date` missing.
            json!({ "party": { "size": "two", "seating": "rooftop" } }),
        ),
        // "selected", "retry_then_success", and any default.
        _ => tool_use_anthropic(base, "echo", json!({ "text": "hello" })),
    }
}

fn tool_use_anthropic(base: impl Fn(Value) -> Value, name: &str, input: Value) -> Value {
    let mut v = base(json!([
        { "type": "tool_use", "id": "toolu_mock", "name": name, "input": input }
    ]));
    v["stop_reason"] = json!("tool_use");
    v
}

/// The OpenAI response body for a success scenario.
fn openai_body(scenario: &str) -> Value {
    let usage = json!({ "prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49 });
    let base = |message: Value, finish: &str| {
        json!({
            "id": "chatcmpl_mock",
            "object": "chat.completion",
            "model": "gpt-mock-1",
            "choices": [ { "index": 0, "message": message, "finish_reason": finish } ],
            "usage": usage,
        })
    };
    match scenario {
        "no_tool" => base(
            json!({ "role": "assistant", "content": "No suitable tool; answering directly." }),
            "stop",
        ),
        "hallucinated" => base(
            tool_calls_openai("no_such_tool", "{\"q\":\"x\"}"),
            "tool_calls",
        ),
        "reservation" => base(
            tool_calls_openai(
                "make_reservation",
                "{\"party\":{\"size\":2,\"seating\":\"outdoor\"},\"date\":\"2026-01-01\"}",
            ),
            "tool_calls",
        ),
        "bad_args" => base(
            tool_calls_openai(
                "make_reservation",
                "{\"party\":{\"size\":\"two\",\"seating\":\"rooftop\"}}",
            ),
            "tool_calls",
        ),
        "malformed_args" => base(
            // Deliberately-broken JSON in `arguments` — models really do this.
            tool_calls_openai("echo", "{not valid json"),
            "tool_calls",
        ),
        _ => base(
            tool_calls_openai("echo", "{\"text\":\"hello\"}"),
            "tool_calls",
        ),
    }
}

fn tool_calls_openai(name: &str, arguments: &str) -> Value {
    json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": [
            { "id": "call_mock", "type": "function", "function": { "name": name, "arguments": arguments } }
        ]
    })
}
