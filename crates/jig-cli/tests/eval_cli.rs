//! End-to-end CLI tests for the **save-case loop** and `jig eval` exit codes.
//!
//! These drive the real `jig` binary against the real `jig-mock-server` (as the
//! MCP server over stdio) and the mock model provider (over TCP), with a dummy
//! key and `JIG_BENCH_BASE_URL` pointed at a scripted scenario.
//!
//! The headline test is the full loop: `jig bench --save-case <file>` drafts a
//! case from an exploration run, then `jig eval --suite <file>` replays it
//! against the same mocks and passes — exploration turned into a regression
//! test, automated.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

fn jig_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig"))
}

/// Locate the sibling `jig-mock-server` binary (built alongside `jig` under
/// `cargo test --workspace`). Falls back to building it if run in isolation.
fn mock_server_bin() -> PathBuf {
    let dir = jig_bin().parent().expect("bin dir").to_path_buf();
    let name = if cfg!(windows) {
        "jig-mock-server.exe"
    } else {
        "jig-mock-server"
    };
    let path = dir.join(name);
    if !path.exists() {
        // Isolated `-p jig-cli` run: build the fixture binary on demand.
        let _ = Command::new(env!("CARGO"))
            .args(["build", "-p", "jig-mock-server"])
            .status();
    }
    assert!(
        path.exists(),
        "mock-server binary not found at {} (run `cargo test --workspace`)",
        path.display()
    );
    path
}

/// A single `--stdio` command string for the mock MCP server, quoted so a path
/// containing spaces survives jig's command splitter.
fn stdio_arg() -> String {
    format!("\"{}\"", mock_server_bin().display())
}

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

/// Spawn the mock provider and block until it accepts TCP.
fn spawn_provider() -> (Guard, u16) {
    let port = free_port();
    let child = Command::new(mock_server_bin())
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
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "mock provider never started on {port}");
    (guard, port)
}

fn base_url(port: u16, scenario: &str) -> String {
    format!("http://127.0.0.1:{port}/{scenario}")
}

/// A unique path in this crate's integration-test temp dir.
fn temp_suite(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("{name}-{}.yaml", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn save_case_then_eval_round_trips() {
    let (_g, port) = spawn_provider();
    let suite_path = temp_suite("savecase-loop");

    // 1) Explore with `jig bench --save-case`. The `selected` scenario always
    //    picks `echo` with `{ "text": "hello" }`, so the draft asserts `echo`.
    let bench = Command::new(jig_bin())
        .args([
            "bench",
            "--stdio",
            &stdio_arg(),
            "--task",
            "Echo the greeting hello",
            "--model",
            "gpt-4o",
            "--runs",
            "3",
            "--save-case",
        ])
        .arg(&suite_path)
        .env("JIG_BENCH_BASE_URL", base_url(port, "selected"))
        .env("JIG_BENCH_API_KEY", "dummy-test-key")
        .output()
        .expect("run jig bench");
    assert!(
        bench.status.success(),
        "bench failed: {}",
        String::from_utf8_lossy(&bench.stderr)
    );

    let drafted = std::fs::read_to_string(&suite_path).expect("suite file written");
    assert!(drafted.contains("# TODO: review drafted case"), "{drafted}");
    assert!(drafted.contains("tool: echo"), "{drafted}");
    assert!(drafted.contains("runs: 3"), "{drafted}");
    // The drafted suite must parse (round-trips through the loader).
    jig_core::eval::load_suite_str(&drafted, "drafted").expect("drafted suite parses");

    // 2) Replay the drafted case with `jig eval` against the same mocks → pass.
    let eval = Command::new(jig_bin())
        .args([
            "eval",
            "--stdio",
            &stdio_arg(),
            "--model",
            "gpt-4o",
            "--suite",
        ])
        .arg(&suite_path)
        .env("JIG_BENCH_BASE_URL", base_url(port, "selected"))
        .env("JIG_BENCH_API_KEY", "dummy-test-key")
        .output()
        .expect("run jig eval");
    assert_eq!(
        eval.status.code(),
        Some(0),
        "eval of the drafted case should pass (0). stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&eval.stderr),
        String::from_utf8_lossy(&eval.stdout)
    );
    let out = String::from_utf8_lossy(&eval.stdout);
    assert!(out.contains("verdict:   PASS"), "{out}");
}

#[test]
fn eval_gate_not_met_exits_3() {
    let (_g, port) = spawn_provider();
    let suite_path = temp_suite("gate-fail");
    // A case expecting `echo`, but the provider answers with no tool → 0% → the
    // gate of 0.5 is not met → the run fails with the dedicated exit code 3.
    std::fs::write(
        &suite_path,
        r#"cases:
  - id: echo-it
    task: "Echo hello"
    expect:
      tool: echo
    runs: 3
"#,
    )
    .expect("write suite");

    let eval = Command::new(jig_bin())
        .args([
            "eval",
            "--stdio",
            &stdio_arg(),
            "--model",
            "gpt-4o",
            "--gate",
            "0.5",
            "--suite",
        ])
        .arg(&suite_path)
        .env("JIG_BENCH_BASE_URL", base_url(port, "no_tool"))
        .env("JIG_BENCH_API_KEY", "dummy-test-key")
        .output()
        .expect("run jig eval");
    assert_eq!(
        eval.status.code(),
        Some(3),
        "an unmet gate must exit 3. stderr: {}",
        String::from_utf8_lossy(&eval.stderr)
    );
}

#[test]
fn save_case_refuses_when_no_tool_selected() {
    let (_g, port) = spawn_provider();
    let suite_path = temp_suite("savecase-refuse");
    // The `no_tool` scenario never selects a tool, so there is nothing to draft.
    let bench = Command::new(jig_bin())
        .args([
            "bench",
            "--stdio",
            &stdio_arg(),
            "--task",
            "Answer in words",
            "--model",
            "gpt-4o",
            "--runs",
            "2",
            "--save-case",
        ])
        .arg(&suite_path)
        .env("JIG_BENCH_BASE_URL", base_url(port, "no_tool"))
        .env("JIG_BENCH_API_KEY", "dummy-test-key")
        .output()
        .expect("run jig bench");
    assert!(bench.status.success(), "bench itself still succeeds");
    let stderr = String::from_utf8_lossy(&bench.stderr);
    assert!(stderr.contains("not drafting"), "{stderr}");
    assert!(!suite_path.exists(), "no file should be written on refusal");
}
