//! End-to-end integration test: the `jig-core` client spawns the real
//! `jig-mock-server` binary, performs the full handshake, lists tools, calls
//! two of them, and asserts on both the results *and* the protocol tap.
//!
//! The mock server binary path is provided by Cargo as `CARGO_BIN_EXE_<name>`
//! because this test lives in the crate that defines that binary.

use std::time::Duration;

use jig_core::{Client, Direction, JigError, ProtocolTap, StdioTransport};
use serde_json::json;

/// Path to the freshly built mock-server binary for this test run.
fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

#[tokio::test]
async fn full_handshake_list_and_call() {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake should succeed");

    // --- Handshake results ---------------------------------------------------
    assert_eq!(client.server_info().name, "jig-mock-server");
    assert_eq!(client.protocol_version(), "2025-06-18");
    assert!(client.has_capability("tools"));
    assert!(!client.has_capability("resources"));

    // --- tools/list ----------------------------------------------------------
    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 3);
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
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

    // --- tools/call: success -------------------------------------------------
    let echo = client
        .call_tool("echo", json!({ "text": "hello jig" }))
        .await
        .expect("echo call");
    assert!(!echo.is_error);
    let rendered: String = echo.content.iter().map(|b| b.render()).collect();
    assert!(rendered.contains("hello jig"), "got: {rendered}");

    // --- tools/call: tool-reported error is Ok(is_error), not Err ------------
    let fail = client
        .call_tool("always_fails", json!({}))
        .await
        .expect("always_fails returns a protocol-valid result");
    assert!(fail.is_error);

    // --- Protocol tap assertions ---------------------------------------------
    // The tap must have captured, in order:
    //   0 -> initialize                 (outbound)
    //   1 <- initialize result          (inbound)
    //   2 -> notifications/initialized  (outbound)
    //   3 -> tools/list                 (outbound)
    //   4 <- tools/list result          (inbound)
    //   (resources/prompts are skipped client-side: no traffic)
    //   5 -> tools/call echo            (outbound)
    //   6 <- echo result                (inbound)
    //   7 -> tools/call always_fails    (outbound)
    //   8 <- always_fails result        (inbound)
    let entries = client.tap().entries();
    assert_eq!(entries.len(), 9, "unexpected tap: {entries:#?}");

    // Sequence numbers are dense and monotonic.
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.seq, i as u64);
    }

    // Directions and methods in order.
    assert_eq!(entries[0].direction, Direction::Outbound);
    assert_eq!(entries[0].method(), Some("initialize"));
    assert_eq!(entries[1].direction, Direction::Inbound);
    assert_eq!(entries[1].method(), None); // response carries no method
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

    // Monotonic timestamps across the whole session.
    for pair in entries.windows(2) {
        assert!(pair[1].elapsed_micros >= pair[0].elapsed_micros);
    }

    // The inbound initialize result actually carried the negotiated version.
    assert_eq!(
        entries[1].message["result"]["protocolVersion"],
        "2025-06-18"
    );

    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn tap_serializes_to_jsonl() {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake");
    client.list_tools().await.expect("tools/list");

    let jsonl = client.tap().to_jsonl();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), client.tap().len());
    // Every line is a standalone JSON object with the expected shape.
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid json line");
        assert!(v.get("seq").is_some());
        assert!(v.get("direction").is_some());
        assert!(v.get("elapsed_micros").is_some());
        assert!(v.get("message").is_some());
    }

    client.shutdown().await.expect("shutdown");
}

/// A server that accepts a request but never answers it must surface as a
/// named [`JigError::Timeout`] within the configured window — never an
/// indefinite hang. `test/hang` is the mock's deliberately-silent method.
#[tokio::test]
async fn request_that_gets_no_response_times_out() {
    let transport = StdioTransport::spawn_with_timeout(
        &mock_server(),
        &[],
        ProtocolTap::new(),
        Some(Duration::from_millis(300)),
    )
    .expect("spawn");

    let started = std::time::Instant::now();
    let err = transport
        .request("test/hang", json!({}))
        .await
        .expect_err("a silent server must time out, not hang");

    match err {
        JigError::Timeout { method, elapsed } => {
            assert_eq!(method, "test/hang");
            assert_eq!(elapsed, Duration::from_millis(300));
        }
        other => panic!("expected JigError::Timeout, got {other:?}"),
    }
    // Sanity: it actually gave up near the deadline, not after some long hang.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout took too long: {:?}",
        started.elapsed()
    );

    transport.shutdown().await.expect("shutdown");
}

/// With the timeout disabled (`None`), a normal request still succeeds — the
/// no-timeout path must not break ordinary operation.
#[tokio::test]
async fn no_timeout_still_completes_normal_requests() {
    let transport =
        StdioTransport::spawn_with_timeout(&mock_server(), &[], ProtocolTap::new(), None)
            .expect("spawn");

    let result = transport
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        )
        .await
        .expect("initialize should succeed with no timeout");
    assert_eq!(result["serverInfo"]["name"], "jig-mock-server");

    transport.shutdown().await.expect("shutdown");
}
