//! End-to-end tests for `jig context`'s **core** rendering against the real
//! `jig-mock-server`.
//!
//! The correctness anchor is the *bench-parity* test: the request body
//! `jig context` renders must be byte-identical to the body `jig bench`
//! actually assembles and sends to a provider — minus the placeholder task.
//! We prove it by running the real bench engine against the scripted mock
//! provider (capturing `BenchReport::rendered_request`, the exact body sent),
//! then swapping its user task for the placeholder and asserting equality with
//! the context body, for **both** provider dialects.
//!
//! Two transport tests exercise `context::build` over a live MCP session — one
//! stdio, one Streamable HTTP — so the "list tools + capture instructions" path
//! is covered on both transports. `jig context` never contacts a provider and
//! never needs a key; nothing here uses one.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::Duration;

use jig_core::bench::{self, BenchConfig, BenchModel, Provider};
use jig_core::context::{self, CONTEXT_TASK_PLACEHOLDER};
use jig_core::{Client, Tool};
use serde_json::Value;

fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Kills the child process when dropped.
struct Guard {
    child: Child,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the scripted mock model provider and wait until it accepts TCP.
async fn spawn_provider() -> (Guard, u16) {
    let port = free_port();
    let child = Command::new(mock_server())
        .arg("--provider")
        .arg(port.to_string())
        .spawn()
        .expect("spawn mock provider");
    let guard = Guard { child };
    let mut ready = false;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "mock provider never started on {port}");
    (guard, port)
}

/// Spawn the mock server in `--http` MCP mode and wait until it is listening.
async fn spawn_http() -> (Guard, String) {
    let port = free_port();
    let child = Command::new(mock_server())
        .arg("--http")
        .arg(port.to_string())
        .spawn()
        .expect("spawn mock http server");
    let guard = Guard { child };
    let mut ready = false;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "mock http server never started on {port}");
    (guard, format!("http://127.0.0.1:{port}/mcp"))
}

/// List the mock MCP server's tools over stdio.
async fn server_tools() -> Vec<Tool> {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake");
    let tools = client.list_tools().await.expect("tools/list");
    client.shutdown().await.expect("shutdown");
    tools
}

/// A bench config pointed at the `selected` scenario on the mock provider.
fn config(model: &str, port: u16) -> BenchConfig {
    BenchConfig {
        model: BenchModel::resolve(model).expect("known model"),
        task: "Find the docs page about rate limits".into(),
        runs: 1,
        temperature: context::CONTEXT_TEMPERATURE,
        max_tokens: bench::DEFAULT_MAX_TOKENS,
        timeout: Some(Duration::from_secs(10)),
        base_url: Some(format!("http://127.0.0.1:{port}/selected")),
        api_key: "dummy-test-key".into(),
    }
}

/// Replace the last message's `content` (the user task) with the placeholder,
/// so a bench body can be compared to a context body regardless of task.
fn with_placeholder_task(mut body: Value) -> Value {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        if let Some(last) = messages.last_mut() {
            last["content"] = Value::String(CONTEXT_TASK_PLACEHOLDER.to_string());
        }
    }
    body
}

/// The bench-parity anchor for a given model/dialect: `jig context`'s body must
/// equal the body `jig bench` actually sent, once the task is neutralized.
async fn assert_parity(model: &str, provider: Provider) {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;

    // The body `jig bench` really assembled and sent to the mock provider.
    let report = bench::run_bench(&tools, &config(model, port))
        .await
        .expect("bench ran against the mock provider");
    assert_eq!(report.provider, provider);
    let bench_body = with_placeholder_task(report.rendered_request.clone());

    // The body `jig context` renders (no key, nothing sent).
    let view = context::build(
        provider,
        model,
        &report.api_model,
        &tools,
        Some("A toy MCP server for exercising Jig."),
    )
    .expect("context built");

    assert_eq!(
        view.body, bench_body,
        "context body must be byte-identical to bench's captured request minus the task"
    );
    // The context body carries the placeholder task, never a real one.
    let last = view.body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(
        last["content"],
        Value::String(CONTEXT_TASK_PLACEHOLDER.into())
    );
}

#[tokio::test]
async fn context_body_matches_bench_captured_request_openai() {
    assert_parity("gpt-4o", Provider::OpenAI).await;
}

#[tokio::test]
async fn context_body_matches_bench_captured_request_anthropic() {
    assert_parity("claude-sonnet", Provider::Anthropic).await;
}

#[tokio::test]
async fn context_builds_over_stdio_capturing_tools_and_instructions() {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake");
    let tools = client.list_tools().await.expect("tools/list");
    let instructions = client.instructions().map(|s| s.to_string());
    client.shutdown().await.expect("shutdown");

    let view = context::build(
        Provider::OpenAI,
        "gpt-4o",
        "gpt-4o",
        &tools,
        instructions.as_deref(),
    )
    .expect("context built");

    // The tool surface is rendered, largest-first, and the instructions section
    // is captured but flagged as not sent by bench.
    assert_eq!(view.tools.len(), tools.len());
    assert_eq!(view.tools[0].name, "make_reservation"); // the heaviest tool
    let instr = view.instructions.expect("mock server offers instructions");
    assert!(!instr.sent_by_bench);
    assert!(instr.tokens > 0);
    assert!(view.total_tokens == view.system_tokens + view.tools_tokens);
    // Every server tool appears in the rendered request body.
    let sent = view.body["tools"].as_array().unwrap().len();
    assert_eq!(sent, tools.len());
}

#[tokio::test]
async fn context_builds_over_http_transport_parity() {
    let (_server, url) = spawn_http().await;
    let client = Client::connect_http(&url).await.expect("http handshake");
    let tools = client.list_tools().await.expect("tools/list");
    let instructions = client.instructions().map(|s| s.to_string());
    client.shutdown().await.expect("shutdown");

    let view = context::build(
        Provider::Anthropic,
        "claude-sonnet",
        "claude-sonnet-5",
        &tools,
        instructions.as_deref(),
    )
    .expect("context built over http");

    assert_eq!(view.provider, Provider::Anthropic);
    assert_eq!(view.tools.len(), tools.len());
    // Anthropic dialect: tools are {name, description, input_schema} objects.
    assert!(view.body["tools"][0].get("input_schema").is_some());
}
