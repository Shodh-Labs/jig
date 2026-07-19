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

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
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
    match kind {
        Kind::Stdio => {
            let client = Client::connect(&mock_server(), &[])
                .await
                .expect("stdio handshake");
            let sig = drive(&client).await;
            client.shutdown().await.expect("stdio shutdown");
            sig
        }
        Kind::Http => {
            let (_guard, url) = spawn_http(&[]).await;
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
        (Direction::Outbound, Some("notifications/initialized".to_string())),
        (Direction::Outbound, Some("tools/list".to_string())),
        (Direction::Inbound, None),
        (Direction::Outbound, Some("tools/call".to_string())),
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

/// Grab an ephemeral free port by binding to 0 and releasing it.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
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

/// Spawn the mock server in `--http` mode on a free port, wait until it accepts
/// connections, and return the guard plus the MCP URL.
async fn spawn_http(extra_args: &[&str]) -> (ServerGuard, String) {
    let port = free_port();
    let mut cmd = Command::new(mock_server());
    cmd.arg("--http").arg(port.to_string());
    for a in extra_args {
        cmd.arg(a);
    }
    let child = cmd.spawn().expect("spawn mock http server");
    let guard = ServerGuard { child };

    let mut ready = false;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "mock http server never started listening on {port}");

    (guard, format!("http://127.0.0.1:{port}/mcp"))
}
