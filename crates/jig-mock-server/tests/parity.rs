//! Cross-transport **parity** suite.
//!
//! One logical conversation — initialize (in `connect`) → `tools/list` →
//! `tools/call` → `shutdown` — is driven over every transport Jig speaks, and
//! the resulting protocol taps are asserted *equivalent*: the same methods, in
//! the same order, in the same directions. Timing (`elapsed_micros`) and the
//! concrete server endpoint legitimately differ and are excluded from the
//! comparison; everything else must match, because a transport is supposed to
//! be invisible above the wire.
//!
//! Adding a future transport is a one-line change: add a variant to [`Kind`]
//! and its arm in [`connect_and_drive`]; it then joins the parity assertion
//! automatically.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use jig_core::{Client, Direction, TapEntry};
use serde_json::json;

/// Path to the freshly built mock-server binary for this test run.
fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// Every transport under parity test. **Add a transport here (one line) and it
/// joins the suite.**
const TRANSPORTS: &[Kind] = &[Kind::Stdio, Kind::Http];

/// A transport Jig can speak to the mock server.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Stdio,
    Http,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Stdio => "stdio",
            Kind::Http => "http",
        }
    }
}

/// The normalized, timing-independent signature of one tap entry: its direction
/// and its JSON-RPC method (absent for responses). This is exactly the part of
/// the tap that must be identical across transports.
type Signature = Vec<(Direction, Option<String>)>;

fn signature(entries: &[TapEntry]) -> Signature {
    entries
        .iter()
        .map(|e| (e.direction, e.method().map(str::to_string)))
        .collect()
}

/// Connect over `kind`, drive the identical conversation, and return the tap's
/// signature. The connection is cleanly shut down before returning.
async fn connect_and_drive(kind: Kind) -> Signature {
    // The mock is asked to serve resources + prompts on both transports so the
    // conversation can exercise resources/read and prompts/get at parity.
    match kind {
        Kind::Stdio => {
            let client = Client::connect(&mock_server(), &["--resources-prompts".to_string()])
                .await
                .expect("stdio handshake");
            let sig = drive(&client).await;
            client.shutdown().await.expect("stdio shutdown");
            sig
        }
        Kind::Http => {
            let (_guard, url) = spawn_http(&["--resources-prompts"]).await;
            let client = Client::connect_http(&url).await.expect("http handshake");
            let sig = drive(&client).await;
            client.shutdown().await.expect("http shutdown");
            // `_guard` kills the child on drop, here, after shutdown.
            sig
        }
    }
}

/// The one logical conversation, identical for every transport. Returns the tap
/// signature captured *before* shutdown (shutdown records no protocol traffic).
async fn drive(client: &Client) -> Signature {
    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 3, "the mock exposes three tools");

    let echo = client
        .call_tool("echo", json!({ "text": "parity" }))
        .await
        .expect("tools/call echo");
    assert!(!echo.is_error);

    // resources/read: a text resource, rendered as text.
    let read = client
        .read_resource("mock://text/hello")
        .await
        .expect("resources/read");
    assert_eq!(read.contents.len(), 1);

    // prompts/get: expand the greet prompt with an argument.
    let prompt = client
        .get_prompt("greet", json!({ "name": "parity" }))
        .await
        .expect("prompts/get");
    assert_eq!(prompt.messages.len(), 1);

    signature(&client.tap().entries())
}

/// The single parity assertion: every transport produces the same tap signature.
#[tokio::test]
async fn taps_are_equivalent_across_transports() {
    let mut signatures: Vec<(&str, Signature)> = Vec::new();
    for &kind in TRANSPORTS {
        signatures.push((kind.name(), connect_and_drive(kind).await));
    }

    // The expected shape, stated once so a regression is legible rather than
    // just "A != B".
    let expected: Signature = vec![
        (Direction::Outbound, Some("initialize".to_string())),
        (Direction::Inbound, None),
        (
            Direction::Outbound,
            Some("notifications/initialized".to_string()),
        ),
        (Direction::Outbound, Some("tools/list".to_string())),
        (Direction::Inbound, None),
        (Direction::Outbound, Some("tools/call".to_string())),
        (Direction::Inbound, None),
        (Direction::Outbound, Some("resources/read".to_string())),
        (Direction::Inbound, None),
        (Direction::Outbound, Some("prompts/get".to_string())),
        (Direction::Inbound, None),
    ];

    for (name, sig) in &signatures {
        assert_eq!(
            sig, &expected,
            "transport '{name}' produced an unexpected tap signature"
        );
    }

    // And, redundantly but explicitly, every transport agrees with every other.
    let (first_name, first) = &signatures[0];
    for (name, sig) in &signatures[1..] {
        assert_eq!(
            sig, first,
            "transport '{name}' diverged from '{first_name}'"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP fixture plumbing (mirrors http_integration.rs).
// ---------------------------------------------------------------------------

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

/// Kills the child mock server when dropped, so no fixture process leaks.
struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the mock server in `--http 0` mode, learn the OS-assigned port from its
/// announcement, and return the guard plus the MCP URL.
async fn spawn_http(extra_args: &[&str]) -> (ServerGuard, String) {
    let mut cmd = Command::new(mock_server());
    cmd.arg("--http").arg("0");
    for a in extra_args {
        cmd.arg(a);
    }
    let (child, port) = spawn_and_read_port(cmd);
    (
        ServerGuard { child },
        format!("http://127.0.0.1:{port}/mcp"),
    )
}
