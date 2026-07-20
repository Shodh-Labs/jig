//! End-to-end integration tests for the **Streamable HTTP** transport: the
//! `jig-core` client drives the real `jig-mock-server` binary running in
//! `--http` mode, over actual TCP, in both JSON and SSE response modes, and
//! asserts on results *and* the protocol tap.
//!
//! The mock server binary path is provided by Cargo as `CARGO_BIN_EXE_<name>`
//! because this test lives in the crate that defines that binary.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use jig_core::{Client, ClientOptions, Direction, JigError, ProtocolTap, TapEntry};
use serde_json::{json, Value};

/// Path to the freshly built mock-server binary for this test run.
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
                    // Keep draining so `observed-reply:` lines never block the
                    // child on a full stderr pipe.
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

/// Spawn the mock server in `--http 0` mode with `extra_args`, learn the
/// OS-assigned port from its announcement, and return the guard plus the MCP URL.
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

// ---------------------------------------------------------------------------
// Standalone GET SSE stream (server→client messages).
// ---------------------------------------------------------------------------

/// Options with the standalone GET stream enabled.
fn listen_opts() -> ClientOptions {
    ClientOptions {
        listen: true,
        ..ClientOptions::default()
    }
}

/// Find the inbound message carrying `method` (a server→client request/notif).
fn inbound_method<'a>(entries: &'a [TapEntry], method: &str) -> Option<&'a TapEntry> {
    entries
        .iter()
        .find(|e| e.direction == Direction::Inbound && e.method() == Some(method))
}

/// The GET stream carries server-pushed notifications *and* server→client
/// requests; every one lands in the tap, `ping` is answered with an empty
/// result, and an unimplemented request (`sampling/createMessage`) is answered
/// with `-32601`. Every exchange is tapped in both directions.
#[tokio::test]
async fn http_get_stream_captures_pushes_and_answers_requests() {
    let (_server, url) = spawn_http(&[
        "--push-notifications",
        "2",
        "--server-ping",
        "--server-sampling",
    ])
    .await;

    let client = Client::connect_http_with_options(&url, vec![], ProtocolTap::new(), listen_opts())
        .await
        .expect("handshake");

    let summary = client
        .listen(Duration::from_secs(3))
        .await
        .expect("listen on the GET stream");

    assert!(summary.opened, "the server opened the SSE stream");
    assert_eq!(summary.status, 200);
    assert_eq!(summary.notifications, 2, "two pushed notifications");
    assert_eq!(summary.pings, 1, "one server ping, answered");
    assert_eq!(summary.other_requests, 1, "one sampling request, -32601'd");

    let entries = client.tap().entries();

    // The ping request was captured, and Jig's empty-result reply is in the tap.
    let ping = inbound_method(&entries, "ping").expect("ping in tap");
    let ping_id = ping.message.get("id").cloned().expect("ping has an id");
    let ping_reply = entries.iter().find(|e| {
        e.direction == Direction::Outbound
            && e.message.get("id") == Some(&ping_id)
            && e.message.get("result").is_some()
    });
    assert!(
        ping_reply.is_some(),
        "ping answered with a result in the tap"
    );

    // The sampling request was captured, and Jig's -32601 reply is in the tap.
    let sampling = inbound_method(&entries, "sampling/createMessage").expect("sampling in tap");
    let sampling_id = sampling.message.get("id").cloned().expect("sampling id");
    let sampling_reply = entries.iter().find(|e| {
        e.direction == Direction::Outbound
            && e.message.get("id") == Some(&sampling_id)
            && e.message.get("error").and_then(|err| err.get("code")) == Some(&json!(-32601))
    });
    assert!(
        sampling_reply.is_some(),
        "sampling answered with -32601 in the tap"
    );

    client.shutdown().await.expect("clean shutdown");
}

/// A server that offers no standalone stream answers the GET with HTTP 405.
/// That is spec-permitted, not an error: the summary records it and `listen`
/// returns `Ok` with `opened == false`.
#[tokio::test]
async fn http_get_stream_405_is_tolerated() {
    // No push flags -> the mock's GET handler returns 405.
    let (_server, url) = spawn_http(&[]).await;

    let client = Client::connect_http_with_options(&url, vec![], ProtocolTap::new(), listen_opts())
        .await
        .expect("handshake");

    let summary = client
        .listen(Duration::from_secs(1))
        .await
        .expect("405 must not be an error");
    assert!(!summary.opened, "the server declined the stream");
    assert_eq!(summary.status, 405);
    assert_eq!(summary.notifications, 0);

    client.shutdown().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------
// Streaming size-cap enforcement on HTTP response bodies.
// ---------------------------------------------------------------------------

/// Options with a low inbound size cap and the default timeout.
fn low_cap_opts(cap: usize) -> ClientOptions {
    ClientOptions {
        max_message_bytes: Some(cap),
        ..ClientOptions::default()
    }
}

/// A multi-megabyte single JSON response body must abort with MessageTooLarge —
/// enforced *while streaming* (the cap is far below the body size, so it fires
/// long before the whole body could be buffered).
#[tokio::test]
async fn http_streaming_cap_aborts_giant_json_body() {
    let (_server, url) = spawn_http(&["--giant-json"]).await;

    let client = Client::connect_http_with_options(
        &url,
        vec![],
        ProtocolTap::new(),
        low_cap_opts(64 * 1024),
    )
    .await
    .expect("handshake (initialize is small)");

    let err = client
        .list_tools()
        .await
        .expect_err("a giant JSON body must exceed the cap");
    assert!(
        matches!(err, JigError::MessageTooLarge { limit } if limit == 64 * 1024),
        "expected MessageTooLarge, got: {err:?}"
    );

    client.shutdown().await.expect("clean shutdown");
}

/// A single multi-megabyte SSE event must likewise abort with MessageTooLarge:
/// the per-event cap fires as the event accumulates, not after the whole body.
#[tokio::test]
async fn http_streaming_cap_aborts_giant_sse_event() {
    let (_server, url) = spawn_http(&["--sse", "--giant-sse"]).await;

    let client = Client::connect_http_with_options(
        &url,
        vec![],
        ProtocolTap::new(),
        low_cap_opts(64 * 1024),
    )
    .await
    .expect("handshake");

    let err = client
        .list_tools()
        .await
        .expect_err("a giant SSE event must exceed the cap");
    assert!(
        matches!(err, JigError::MessageTooLarge { limit } if limit == 64 * 1024),
        "expected MessageTooLarge, got: {err:?}"
    );

    client.shutdown().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------
// resources/read + prompts/get over HTTP.
// ---------------------------------------------------------------------------

/// The invocation verbs work over HTTP: a text resource renders as text, a blob
/// resource preserves its base64, and a prompt expands with its argument.
#[tokio::test]
async fn http_resources_read_and_prompts_get() {
    let (_server, url) = spawn_http(&["--resources-prompts"]).await;

    let client = Client::connect_http(&url).await.expect("handshake");
    assert!(client.has_capability("resources"));
    assert!(client.has_capability("prompts"));

    let text = client
        .read_resource("mock://text/hello")
        .await
        .expect("read text resource");
    assert_eq!(text.contents.len(), 1);
    assert!(text.contents[0].render().contains("Hello from a jig mock"));

    let blob = client
        .read_resource("mock://blob/logo")
        .await
        .expect("read blob resource");
    // A blob is summarized, never dumped; the base64 survives round-trip.
    assert_eq!(blob.contents[0].mime_type(), Some("image/png"));
    assert!(blob.contents[0].render().starts_with("[blob image/png"));

    let prompt = client
        .get_prompt("greet", json!({ "name": "Ada" }))
        .await
        .expect("get prompt");
    assert_eq!(prompt.messages.len(), 1);
    assert_eq!(prompt.messages[0].role, "user");
    assert!(prompt.messages[0].content.render().contains("Ada"));

    // An unknown resource URI surfaces the server's error, not an empty result.
    let missing: Value = match client.read_resource("mock://nope").await {
        Ok(_) => panic!("unknown URI must error"),
        Err(JigError::Server { code, .. }) => json!(code),
        Err(other) => panic!("expected a server error, got {other:?}"),
    };
    assert_eq!(missing, json!(-32002));

    client.shutdown().await.expect("clean shutdown");
}

/// **An unobservable stderr volume is `None`, never `0`.**
///
/// A remote server's stderr belongs to a process Jig never spawned, so there is
/// nothing to count. Reporting `0` would assert the server logged nothing —
/// a claim Jig has no basis for. The absence is modelled explicitly so callers
/// (notably `jig check`'s informational finding) can omit the figure rather
/// than publish a fabricated zero.
#[tokio::test]
async fn http_stderr_volume_is_unknown_not_zero() {
    let (_guard, url) = spawn_http(&[]).await;
    let client = Client::connect_http(&url).await.expect("handshake");

    assert!(
        client.stderr_volume().is_none(),
        "an HTTP target has no child stderr to observe"
    );

    client.shutdown().await.expect("shutdown");
}
