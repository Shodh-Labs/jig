//! End-to-end integration tests for `jig context`: spawn the real `jig` binary
//! against the real `jig-mock-server` over both transports and assert on the
//! rendered output, `--raw`/`--json` surfaces, and the no-key contract.
//!
//! The `jig` binary path comes from Cargo as `CARGO_BIN_EXE_jig`; the mock is
//! its sibling in the same target directory.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::Duration;

use serde_json::Value;

fn jig_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig"))
}

fn mock_bin() -> PathBuf {
    let mut p = jig_bin();
    let name = if cfg!(windows) {
        "jig-mock-server.exe"
    } else {
        "jig-mock-server"
    };
    p.set_file_name(name);
    assert!(
        p.exists(),
        "mock-server binary not found at {} — run with `cargo test --workspace --all-targets`",
        p.display()
    );
    p
}

/// The `--stdio` value that launches the mock: the (space-containing) path must
/// be double-quoted so Jig's command splitter keeps it a single token.
fn stdio_arg() -> String {
    format!("\"{}\"", mock_bin().display())
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Run `jig context --stdio "<mock>"` with the given trailing args.
fn run_context(args: &[&str]) -> Output {
    Command::new(jig_bin())
        .arg("context")
        .arg("--stdio")
        .arg(stdio_arg())
        .args(args)
        .output()
        .expect("spawn jig context")
}

#[test]
fn stdio_human_default_is_openai_and_key_free() {
    // No API keys in the environment: context must still work (needs no key),
    // and default to gpt-4o (OpenAI) when ANTHROPIC_API_KEY is absent.
    let out = Command::new(jig_bin())
        .arg("context")
        .arg("--stdio")
        .arg(stdio_arg())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("spawn jig context");
    assert!(out.status.success(), "context should exit 0");
    let report = stdout(&out);
    assert!(report.contains("[nothing is sent to any API]"));
    assert!(
        report.contains("openai dialect"),
        "default model is gpt-4o: {report}"
    );
    assert!(report.contains("TOTAL context before the user's first word"));
    assert!(report.contains("what `jig bench` sends"));
    assert!(report.contains("make_reservation"));
}

#[test]
fn stdio_provider_override_switches_dialect_without_a_key() {
    let out = run_context(&["--provider", "anthropic"]);
    assert!(out.status.success());
    let report = stdout(&out);
    assert!(
        report.contains("anthropic dialect"),
        "override to anthropic: {report}"
    );
}

#[test]
fn stdio_raw_is_valid_json_body_with_placeholder_task() {
    let out = run_context(&["--raw", "--model", "gpt-4o"]);
    assert!(out.status.success());
    let body: Value = serde_json::from_str(&stdout(&out)).expect("--raw is valid JSON");
    // The exact OpenAI request body, with a placeholder user task.
    assert_eq!(body["tool_choice"], "auto");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "<your task here>");
    assert_eq!(body["tools"][0]["type"], "function");
    // Never carries a key.
    assert!(!stdout(&out).contains("api_key") && !stdout(&out).contains("authorization"));
}

#[test]
fn stdio_json_has_sections_and_provenance() {
    let out = run_context(&["--json", "--model", "gpt-4o"]);
    assert!(out.status.success());
    let doc: Value = serde_json::from_str(&stdout(&out)).expect("--json is valid JSON");
    assert_eq!(doc["provenance"]["dialect"], "openai");
    assert_eq!(doc["provenance"]["tokenizer"], "o200k_base");
    assert_eq!(doc["provenance"]["exactness"]["exact"], true);
    assert!(doc["sections"]["systemPrompt"]["tokens"].as_u64().unwrap() > 0);
    assert_eq!(doc["sections"]["serverInstructions"]["sentByBench"], false);
    assert!(doc["sections"]["tools"]["count"].as_u64().unwrap() >= 1);
    assert_eq!(doc["taskPlaceholder"], "<your task here>");
    assert!(doc["requestBody"]["tools"].is_array());
}

// ---- HTTP transport parity -------------------------------------------------

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

struct Guard {
    child: Child,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_http() -> (Guard, String) {
    let port = free_port();
    let child = Command::new(mock_bin())
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
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "mock http server never started on {port}");
    (guard, format!("http://127.0.0.1:{port}/mcp"))
}

#[test]
fn http_transport_renders_context() {
    let (_guard, url) = spawn_http();
    let out = Command::new(jig_bin())
        .arg("context")
        .arg("--http")
        .arg(&url)
        .arg("--model")
        .arg("gpt-4o")
        .output()
        .expect("spawn jig context --http");
    assert!(out.status.success(), "http context should exit 0");
    let report = stdout(&out);
    assert!(report.contains("[nothing is sent to any API]"));
    assert!(report.contains("tools ("));
}
