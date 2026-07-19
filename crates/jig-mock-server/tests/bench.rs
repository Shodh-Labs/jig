//! End-to-end integration tests for `jig bench`: the real `jig-core` bench
//! engine drives the real `jig-mock-server` **twice** — once as the MCP server
//! (over stdio, to list the tool surface) and once as the **mock model
//! provider** (over TCP, `--provider`, to script each provider response).
//!
//! Every outcome in the taxonomy is exercised with zero network and zero API
//! keys: the base URL is pointed at the mock provider and a dummy key is used.
//! This is the discipline the milestone requires — a misbehaving provider is
//! Jig's to degrade informatively, just like a misbehaving server.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use jig_core::bench::{self, ArgCheck, BenchConfig, BenchModel, Outcome, Provider};
use jig_core::{Client, Tool};

fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// Extract the port from an announcement line carrying `127.0.0.1:<digits>`.
fn parse_announced_port(line: &str) -> Option<u16> {
    let rest = &line[line.find("127.0.0.1:")? + "127.0.0.1:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Spawn `cmd` with piped stderr, read the port the mock announces (bind-0, so
/// the OS assigns it — no pre-selection race), and keep draining stderr in a
/// background thread so the child never blocks on a full pipe. The announcement
/// is emitted only after the listener is bound, so the port is already
/// accepting connections by the time this returns.
fn spawn_and_read_port(mut cmd: Command) -> (Child, u16) {
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mock server");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut sent = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !sent {
                        if let Some(port) = parse_announced_port(&line) {
                            let _ = tx.send(port);
                            sent = true;
                        }
                    }
                }
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("mock server never announced its port within 10s");
    (child, port)
}

/// Kills the child mock provider when dropped.
struct Guard {
    child: Child,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the mock provider on an OS-assigned port and return the guard plus the
/// port learned from its announcement.
async fn spawn_provider() -> (Guard, u16) {
    let mut cmd = Command::new(mock_server());
    cmd.arg("--provider").arg("0");
    let (child, port) = spawn_and_read_port(cmd);
    (Guard { child }, port)
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

/// Build a bench config pointed at a scenario on the mock provider.
fn config(model: &str, port: u16, scenario: &str, runs: usize) -> BenchConfig {
    BenchConfig {
        model: BenchModel::resolve(model).expect("known model"),
        task: "Find the docs page about rate limits".into(),
        runs,
        temperature: 1.0,
        max_tokens: 1024,
        timeout: Some(Duration::from_secs(10)),
        base_url: Some(format!("http://127.0.0.1:{port}/{scenario}")),
        api_key: "dummy-test-key".into(),
    }
}

#[tokio::test]
async fn anthropic_selected_valid_args() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("claude-sonnet", port, "selected", 1))
        .await
        .expect("bench");
    assert_eq!(report.provider, Provider::Anthropic);
    assert_eq!(report.results.len(), 1);
    match &report.results[0].outcome {
        Outcome::Selected {
            tool, args_check, ..
        } => {
            assert_eq!(tool, "echo");
            assert_eq!(*args_check, ArgCheck::Valid);
        }
        other => panic!("expected selected, got {other:?}"),
    }
    // Usage + version were captured.
    assert_eq!(report.results[0].usage.input_tokens, Some(42));
    assert_eq!(
        report.results[0].model_version.as_deref(),
        Some("claude-mock-1")
    );
}

#[tokio::test]
async fn openai_selected_valid_args() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "selected", 1))
        .await
        .expect("bench");
    assert_eq!(report.provider, Provider::OpenAI);
    match &report.results[0].outcome {
        Outcome::Selected { tool, .. } => assert_eq!(tool, "echo"),
        other => panic!("expected selected, got {other:?}"),
    }
    assert_eq!(report.results[0].usage.total_tokens, Some(49));
}

#[tokio::test]
async fn no_tool_answer_is_classified() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("claude-sonnet", port, "no_tool", 1))
        .await
        .expect("bench");
    assert!(matches!(report.results[0].outcome, Outcome::NoTool { .. }));
}

#[tokio::test]
async fn hallucinated_tool_is_classified() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "hallucinated", 1))
        .await
        .expect("bench");
    match &report.results[0].outcome {
        Outcome::HallucinatedTool { name, .. } => assert_eq!(name, "no_such_tool"),
        other => panic!("expected hallucinated, got {other:?}"),
    }
}

#[tokio::test]
async fn nested_enum_args_validate_against_real_schema() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // `reservation` scenario supplies valid nested + enum args.
    let ok = bench::run_bench(&tools, &config("claude-sonnet", port, "reservation", 1))
        .await
        .expect("bench");
    match &ok.results[0].outcome {
        Outcome::Selected {
            tool, args_check, ..
        } => {
            assert_eq!(tool, "make_reservation");
            assert_eq!(*args_check, ArgCheck::Valid, "valid nested args must pass");
        }
        other => panic!("expected selected, got {other:?}"),
    }

    // `bad_args` supplies a wrong type, a bad enum, and a missing required field.
    let bad = bench::run_bench(&tools, &config("claude-sonnet", port, "bad_args", 1))
        .await
        .expect("bench");
    match &bad.results[0].outcome {
        Outcome::Selected { args_check, .. } => match args_check {
            ArgCheck::Invalid { errors } => {
                let joined = errors.join(" | ");
                assert!(joined.contains("missing required field 'date'"), "{joined}");
                assert!(joined.contains("expected integer"), "{joined}");
                assert!(joined.contains("not one of"), "{joined}");
            }
            other => panic!("expected invalid args, got {other:?}"),
        },
        other => panic!("expected selected, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_openai_args_are_unparseable_not_a_panic() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "malformed_args", 1))
        .await
        .expect("bench");
    match &report.results[0].outcome {
        Outcome::Selected { args_check, .. } => {
            assert!(matches!(args_check, ArgCheck::Unparseable { .. }));
        }
        other => panic!("expected selected+unparseable, got {other:?}"),
    }
}

#[tokio::test]
async fn retry_after_429_then_succeeds() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // First hit is 429 with Retry-After: 0; the retry succeeds.
    let report = bench::run_bench(
        &tools,
        &config("claude-sonnet", port, "retry_then_success", 1),
    )
    .await
    .expect("bench");
    assert!(
        matches!(report.results[0].outcome, Outcome::Selected { .. }),
        "a 429 with Retry-After must be retried into success, got {:?}",
        report.results[0].outcome
    );
}

#[tokio::test]
async fn persistent_500_becomes_provider_error_not_a_crash() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "error_500", 1))
        .await
        .expect("bench must not fail — a bad provider degrades to an outcome");
    match &report.results[0].outcome {
        Outcome::ProviderError { detail } => assert!(detail.contains("500"), "{detail}"),
        other => panic!("expected provider_error, got {other:?}"),
    }
}

#[tokio::test]
async fn persistent_429_exhausts_retries_to_provider_error() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("claude-sonnet", port, "error_429", 1))
        .await
        .expect("bench");
    assert!(matches!(
        report.results[0].outcome,
        Outcome::ProviderError { .. }
    ));
}

#[tokio::test]
async fn distribution_aggregates_repeated_runs() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "selected", 5))
        .await
        .expect("bench");
    let dist = report.distribution();
    assert_eq!(dist.total, 5);
    assert_eq!(dist.selected, vec![("echo".to_string(), 5)]);
    assert!(dist.is_consistent());
    assert!(dist.takeaway().starts_with("consistent"));
}

#[tokio::test]
async fn rendered_request_carries_no_auth_and_matches_server_tools() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let report = bench::run_bench(&tools, &config("gpt-4o", port, "selected", 1))
        .await
        .expect("bench");

    // The rendered request never contains the key (auth rides in a header).
    let serialized = serde_json::to_string(&report.rendered_request).unwrap();
    assert!(
        !serialized.contains("dummy-test-key"),
        "the rendered request must be auth-free"
    );

    // Every server tool appears in the request's tool list.
    let sent_names: HashSet<String> = report.rendered_request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    let server_names: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(sent_names, server_names);
    // The minimal system prompt is present and documented.
    assert_eq!(report.system_prompt, bench::BENCH_SYSTEM_PROMPT);
}
