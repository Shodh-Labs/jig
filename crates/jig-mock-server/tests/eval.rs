//! End-to-end integration tests for the `jig eval` **engine** (`jig_core::eval`):
//! the real engine drives the real `jig-mock-server` twice — once as the MCP
//! server (stdio, to list the tool surface) and once as the scripted mock model
//! provider (TCP, `--provider`) — with zero network and zero API keys.
//!
//! Each test loads a `.jig` suite from an inline YAML string, points the eval
//! runner's provider base URL at a mock-provider scenario, and asserts the
//! scored verdict. Together they cover the runner's semantics: a passing case, a
//! wrong-tool fail, a rate-based flaky pass (the `alternate` scenario), a
//! `not_tools` hard fail, provider-error exclusion/erroring, and the run-level
//! gate / `must_pass` verdict.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::Duration;

use jig_core::bench::BenchModel;
use jig_core::eval::{self, CaseVerdict, EvalConfig};
use jig_core::{Client, Tool};

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

/// Spawn the mock provider and wait until it accepts TCP.
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

/// List the mock MCP server's tools over stdio.
async fn server_tools() -> Vec<Tool> {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake");
    let tools = client.list_tools().await.expect("tools/list");
    client.shutdown().await.expect("shutdown");
    tools
}

/// An [`EvalConfig`] pointed at `scenario` on the mock provider.
fn config(model: &str, port: u16, scenario: &str, gate: Option<f64>) -> EvalConfig {
    EvalConfig {
        model: BenchModel::resolve(model).expect("known model"),
        api_key: "dummy-test-key".into(),
        runs_override: None,
        temp_override: None,
        gate,
        timeout: Some(Duration::from_secs(10)),
        max_tokens: 1024,
        base_url: Some(format!("http://127.0.0.1:{port}/{scenario}")),
    }
}

fn suite(yaml: &str) -> Vec<eval::Suite> {
    vec![eval::load_suite_str(yaml, "test.yaml").expect("valid suite")]
}

#[tokio::test]
async fn passing_case_against_selected_scenario() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // `selected` always picks `echo` with valid args.
    let suites = suite(
        r#"
cases:
  - id: echo-it
    task: "Echo hello"
    expect:
      tool: echo
      args:
        text: { contains: "hello" }
    runs: 3
    min_rate: 0.8
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "selected", None), &suites)
        .await
        .expect("eval");
    let case = &report.suites[0].cases[0];
    assert_eq!(case.verdict, CaseVerdict::Pass);
    assert_eq!(case.passes, 3);
    assert!(!case.flaky);
    assert!(report.passed());
}

#[tokio::test]
async fn wrong_tool_case_fails() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // The server picks `echo`, but the case expects `make_reservation`.
    let suites = suite(
        r#"
cases:
  - id: reserve
    task: "Book a table"
    expect:
      tool: make_reservation
    runs: 3
    min_rate: 0.8
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "selected", None), &suites)
        .await
        .expect("eval");
    let case = &report.suites[0].cases[0];
    assert_eq!(case.verdict, CaseVerdict::Fail);
    assert_eq!(case.passes, 0);
}

#[tokio::test]
async fn flaky_case_is_rate_scored_and_flagged() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // `alternate` picks echo, reservation, echo, reservation → 2/4 echo.
    let suites = suite(
        r#"
cases:
  - id: echo-flaky
    task: "Echo hello"
    expect:
      tool: echo
    runs: 4
    min_rate: 0.5
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "alternate", None), &suites)
        .await
        .expect("eval");
    let case = &report.suites[0].cases[0];
    assert_eq!(case.passes, 2, "alternate yields 2/4 echo");
    assert_eq!(case.counted, 4);
    assert!(case.flaky, "a mixed selection must be flagged flaky");
    // rate 0.5 >= min_rate 0.5 → passes, but flaky is still a finding.
    assert_eq!(case.verdict, CaseVerdict::Pass);
}

#[tokio::test]
async fn flaky_case_below_min_rate_fails() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let suites = suite(
        r#"
cases:
  - id: echo-strict
    task: "Echo hello"
    expect:
      tool: echo
    runs: 4
    min_rate: 0.9
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "alternate", None), &suites)
        .await
        .expect("eval");
    let case = &report.suites[0].cases[0];
    assert_eq!(case.verdict, CaseVerdict::Fail, "2/4 < 0.9 must fail");
    assert!(case.flaky);
}

#[tokio::test]
async fn not_tools_selection_is_a_hard_fail() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // The server picks `echo`; the case declares `echo` a known-wrong selection.
    let suites = suite(
        r#"
cases:
  - id: no-echo
    task: "Book a table"
    expect:
      tool: make_reservation
      not_tools: [echo]
    runs: 3
    min_rate: 0.1
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "selected", None), &suites)
        .await
        .expect("eval");
    let case = &report.suites[0].cases[0];
    assert_eq!(
        case.verdict,
        CaseVerdict::NotTools,
        "a not_tools hit is a hard fail regardless of the low min_rate"
    );
    assert_eq!(case.not_tools_hits, 3);
}

#[tokio::test]
async fn provider_errors_error_the_case() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let suites = suite(
        r#"
cases:
  - id: broken
    task: "Echo hello"
    expect:
      tool: echo
    runs: 3
    min_rate: 0.8
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "error_500", None), &suites)
        .await
        .expect("eval must not fail — a bad provider degrades to run outcomes");
    let case = &report.suites[0].cases[0];
    assert_eq!(case.verdict, CaseVerdict::Errored);
    assert_eq!(case.provider_errors, 3);
    assert_eq!(case.counted, 0);
    assert_eq!(case.rate, None);
}

#[tokio::test]
async fn gate_not_met_fails_the_run() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    // Wrong expectation → accuracy 0 → below any positive gate.
    let suites = suite(
        r#"
cases:
  - id: reserve
    task: "Book a table"
    expect:
      tool: make_reservation
    runs: 3
"#,
    );
    let report = eval::run_eval(
        &tools,
        &config("gpt-4o", port, "selected", Some(0.8)),
        &suites,
    )
    .await
    .expect("eval");
    assert_eq!(report.overall_accuracy(), Some(0.0));
    assert!(!report.gate_met());
    assert!(!report.passed(), "a gate below the threshold fails the run");
}

#[tokio::test]
async fn must_pass_case_failing_fails_run_without_gate() {
    let (_g, port) = spawn_provider().await;
    let tools = server_tools().await;
    let suites = suite(
        r#"
cases:
  - id: reserve
    task: "Book a table"
    expect:
      tool: make_reservation
    runs: 3
    must_pass: true
"#,
    );
    let report = eval::run_eval(&tools, &config("gpt-4o", port, "selected", None), &suites)
        .await
        .expect("eval");
    assert!(report.gate_met(), "no gate is trivially met");
    assert_eq!(report.must_pass_failures().len(), 1);
    assert!(!report.passed());
}
