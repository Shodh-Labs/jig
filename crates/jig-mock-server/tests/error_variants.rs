//! Error-variant completeness.
//!
//! The milestone bar: **every `JigError` variant is constructible by a test —
//! via a real code path that genuinely produces it, not a hand-built
//! `JigError::X { .. }` literal.** Each test below drives an actual failure and
//! asserts the resulting variant.
//!
//! [`variant_name`] is an *exhaustive* match over `JigError`: if a new variant
//! is ever added, this file stops compiling until it is named here — a
//! compile-time nudge to add a real-path test for it, so the guarantee cannot
//! silently rot.

use std::time::Duration;

use jig_core::transport::parse_response;
use jig_core::{Client, ClientOptions, JigError, ProtocolTap, StdioTransport};
use serde_json::json;

fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// Exhaustive over every `JigError` variant. Adding a variant to the enum makes
/// this fail to compile until it is added here — the completeness ratchet.
fn variant_name(e: &JigError) -> &'static str {
    match e {
        JigError::Transport(_) => "Transport",
        JigError::Protocol(_) => "Protocol",
        JigError::Server { .. } => "Server",
        JigError::Timeout { .. } => "Timeout",
        JigError::MessageTooLarge { .. } => "MessageTooLarge",
    }
}

/// `Transport` — a real spawn failure: launching a program that does not exist.
#[tokio::test]
async fn transport_variant_from_spawn_failure() {
    // `StdioTransport` is not `Debug`, so match rather than `expect_err`.
    let err = match StdioTransport::spawn(
        "jig-nonexistent-program-zzz-please-do-not-exist",
        &[],
        ProtocolTap::new(),
    ) {
        Ok(_) => panic!("spawning a non-existent program must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, JigError::Transport(_)));
    assert_eq!(variant_name(&err), "Transport");
}

/// `Protocol` — a real response envelope carrying neither `result` nor `error`,
/// run through the shared response parser the transports actually use.
#[tokio::test]
async fn protocol_variant_from_parse_response() {
    let err = parse_response(json!({ "jsonrpc": "2.0", "id": 1 }))
        .expect_err("a response with no result/error is a protocol fault");
    assert!(matches!(err, JigError::Protocol(_)));
    assert_eq!(variant_name(&err), "Protocol");
}

/// `Server` — the mock returns a JSON-RPC error object for an unknown tool,
/// which `call_tool` maps to a server error.
#[tokio::test]
async fn server_variant_from_error_response() {
    let client = Client::connect(&mock_server(), &[])
        .await
        .expect("handshake");
    let err = client
        .call_tool("no-such-tool", json!({}))
        .await
        .expect_err("an unknown tool yields a server error object");
    assert!(matches!(err, JigError::Server { .. }));
    assert_eq!(variant_name(&err), "Server");
    client.shutdown().await.expect("shutdown");
}

/// `Timeout` — the mock's `test/hang` accepts a request and never answers.
#[tokio::test]
async fn timeout_variant_from_silent_server() {
    let transport = StdioTransport::spawn_with_timeout(
        &mock_server(),
        &[],
        ProtocolTap::new(),
        Some(Duration::from_millis(300)),
    )
    .expect("spawn");
    let err = transport
        .request("test/hang", json!({}))
        .await
        .expect_err("a silent server must time out");
    assert!(matches!(err, JigError::Timeout { .. }));
    assert_eq!(variant_name(&err), "Timeout");
    transport.shutdown().await.expect("shutdown");
}

/// `MessageTooLarge` — a ~20 MiB chaos response against a 1 MiB inbound cap.
#[tokio::test]
async fn message_too_large_variant_from_giant_response() {
    let options = ClientOptions {
        request_timeout: Some(Duration::from_secs(5)),
        max_message_bytes: Some(1024 * 1024),
    };
    let client = Client::connect_with_options(
        &mock_server(),
        &["--chaos".to_string(), "giant-message".to_string()],
        ProtocolTap::new(),
        options,
    )
    .await
    .expect("the small handshake fits under the cap");

    let err = client
        .list_tools()
        .await
        .expect_err("a 20 MiB response overruns a 1 MiB cap");
    assert!(matches!(err, JigError::MessageTooLarge { .. }));
    assert_eq!(variant_name(&err), "MessageTooLarge");
}
