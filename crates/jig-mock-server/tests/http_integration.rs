//! End-to-end integration tests for the **Streamable HTTP** transport: the
//! `jig-core` client drives the real `jig-mock-server` binary running in
//! `--http` mode, over actual TCP, in both JSON and SSE response modes, and
//! asserts on results *and* the protocol tap.
//!
//! The mock server binary path is provided by Cargo as `CARGO_BIN_EXE_<name>`
//! because this test lives in the crate that defines that binary.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::Duration;

use jig_core::{Client, Direction, JigError};
use serde_json::json;

/// Path to the freshly built mock-server binary for this test run.
fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// Grab an ephemeral free port by binding to port 0 and releasing it. There is
/// a small race between release and the child re-binding, acceptable for tests.
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

/// Spawn the mock server in `--http` mode on a free port with `extra_args`, wait
/// until it accepts TCP connections, and return the guard plus the MCP URL.
async fn spawn_http(extra_args: &[&str]) -> (ServerGuard, String) {
    let port = free_port();
    let mut cmd = Command::new(mock_server());
    cmd.arg("--http").arg(port.to_string());
    for a in extra_args {
        cmd.arg(a);
    }
    let child = cmd.spawn().expect("spawn mock http server");
    let guard = ServerGuard { child };

    // Poll until the server is listening (bounded so a failed spawn errors out
    // rather than hanging the test).
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

/// JSON response mode: the full handshake -> list -> call flow works, and the
/// tap captures exactly the same messages, in the same order, as the stdio
/// transport does (see the stdio `full_handshake_list_and_call` test).
#[tokio::test]
async fn http_json_mode_full_handshake_list_and_call() {
    let (_server, url) = spawn_http(&[]).await;

    let client = Client::connect_http(&url)
        .await
        .expect("handshake should succeed over HTTP");

    // --- Handshake results ---------------------------------------------------
    assert_eq!(client.server_info().name, "jig-mock-server");
    assert_eq!(client.protocol_version(), "2025-06-18");
    assert!(client.has_capability("tools"));
    assert!(!client.has_capability("resources"));

    // --- tools/list ----------------------------------------------------------
    let tools = client.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tools.len(), 3);
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"make_reservation"));
    assert!(names.contains(&"always_fails"));

    // Unsupported capabilities degrade to empty, not error.
    assert!(client
        .list_resources()
        .await
        .expect("resources graceful")
        .is_empty());
    assert!(client
        .list_prompts()
        .await
        .expect("prompts graceful")
        .is_empty());

    // --- tools/call: success and tool-reported error -------------------------
    let echo = client
        .call_tool("echo", json!({ "text": "hello jig" }))
        .await
        .expect("echo call");
    assert!(!echo.is_error);
    let rendered: String = echo.content.iter().map(|b| b.render()).collect();
    assert!(rendered.contains("hello jig"), "got: {rendered}");

    let fail = client
        .call_tool("always_fails", json!({}))
        .await
        .expect("always_fails is a protocol-valid result");
    assert!(fail.is_error);

    // --- Tap: identical shape to the stdio transport -------------------------
    //   0 -> initialize                 3 -> tools/list        7 -> tools/call
    //   1 <- initialize result          4 <- tools/list result 8 <- result
    //   2 -> notifications/initialized  5 -> tools/call
    //                                   6 <- echo result
    let entries = client.tap().entries();
    assert_eq!(entries.len(), 9, "unexpected tap: {entries:#?}");
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.seq, i as u64);
    }
    assert_eq!(entries[0].direction, Direction::Outbound);
    assert_eq!(entries[0].method(), Some("initialize"));
    assert_eq!(entries[1].direction, Direction::Inbound);
    assert_eq!(entries[1].method(), None);
    assert_eq!(entries[2].direction, Direction::Outbound);
    assert_eq!(entries[2].method(), Some("notifications/initialized"));
    assert_eq!(entries[3].direction, Direction::Outbound);
    assert_eq!(entries[3].method(), Some("tools/list"));
    assert_eq!(entries[4].direction, Direction::Inbound);
    assert_eq!(entries[5].direction, Direction::Outbound);
    assert_eq!(entries[5].method(), Some("tools/call"));
    assert_eq!(entries[6].direction, Direction::Inbound);
    assert_eq!(entries[7].direction, Direction::Outbound);
    assert_eq!(entries[7].method(), Some("tools/call"));
    assert_eq!(entries[8].direction, Direction::Inbound);

    // Monotonic timestamps, and the negotiated version rode the init result.
    for pair in entries.windows(2) {
        assert!(pair[1].elapsed_micros >= pair[0].elapsed_micros);
    }
    assert_eq!(
        entries[1].message["result"]["protocolVersion"],
        "2025-06-18"
    );

    client
        .shutdown()
        .await
        .expect("clean shutdown (sends DELETE)");
}

/// SSE response mode: everything still works, *and* a server notification pushed
/// on the SSE stream ahead of the `tools/list` response is captured in the tap
/// (recorded inbound, ignored at the routing layer) — exactly the stdio policy.
#[tokio::test]
async fn http_sse_mode_captures_pushed_notification() {
    let (_server, url) = spawn_http(&["--sse"]).await;

    let client = Client::connect_http(&url)
        .await
        .expect("handshake should succeed over HTTP SSE");
    assert_eq!(client.server_info().name, "jig-mock-server");

    let tools = client.list_tools().await.expect("tools/list over SSE");
    assert_eq!(tools.len(), 3, "SSE list must still return every tool");

    // The pushed notification is present in the tap as an inbound message with a
    // method, and it did not disturb response routing.
    let notifications: Vec<_> = client
        .tap()
        .entries()
        .into_iter()
        .filter(|e| {
            e.direction == Direction::Inbound && e.method() == Some("notifications/message")
        })
        .collect();
    assert_eq!(
        notifications.len(),
        1,
        "expected exactly one pushed SSE notification in the tap"
    );

    // A successful tool call over SSE too.
    let echo = client
        .call_tool("echo", json!({ "text": "sse hello" }))
        .await
        .expect("echo over SSE");
    let rendered: String = echo.content.iter().map(|b| b.render()).collect();
    assert!(rendered.contains("sse hello"), "got: {rendered}");

    client.shutdown().await.expect("clean shutdown");
}

/// Session expiry: the handshake succeeds (a session id is issued), but the
/// server then returns HTTP 404 for the first post-handshake request. The client
/// must surface a clear, actionable transport error naming the expiry — never
/// silently re-initialize.
#[tokio::test]
async fn http_session_expiry_surfaces_clear_error() {
    let (_server, url) = spawn_http(&["--expire-after-initialize"]).await;

    // The handshake still completes: initialize is answered, and the
    // notifications/initialized notification is accepted (202).
    let client = Client::connect_http(&url)
        .await
        .expect("handshake completes before the session is treated as expired");

    // The first real operation hits 404 -> session-expiry error.
    let err = client
        .list_tools()
        .await
        .expect_err("an expired session must surface as an error");

    match err {
        JigError::Transport(msg) => {
            assert!(
                msg.contains("session expired") && msg.contains("404"),
                "expected an actionable session-expiry message, got: {msg}"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }

    client.shutdown().await.expect("shutdown is still clean");
}

/// A bad URL (nothing listening) must fail fast with an actionable
/// connection-refused error, not hang.
#[tokio::test]
async fn http_connection_refused_is_actionable() {
    // Port 1 is not listening; connect must fail promptly. (`Client` is not
    // `Debug`, so match rather than `expect_err`.)
    let err = match Client::connect_http("http://127.0.0.1:1/mcp").await {
        Ok(_) => panic!("connecting to a dead endpoint must fail"),
        Err(e) => e,
    };
    match err {
        JigError::Transport(msg) => {
            assert!(
                msg.contains("could not connect"),
                "expected a connection error, got: {msg}"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
}
