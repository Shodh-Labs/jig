//! The **hostile-server chaos catalog** integration tests.
//!
//! Each test spawns the real `jig-mock-server` under one `--chaos <mode>` and
//! asserts that Jig degrades *informatively* — a specific, actionable typed
//! error or warning — and, critically, that it never panics and never hangs
//! past the configured timeout. Jig is a diagnostic tool: breaking on hostile
//! input is a product failure by definition, so this catalog is the milestone's
//! core safety net.
//!
//! The *wording* of the diagnostic errors is product surface, so the
//! distinctive messages are locked with `insta` snapshots (see the
//! `snapshots/` directory); accidental drift fails CI and intentional drift is
//! a reviewed snapshot diff.

use std::time::Duration;

use jig_core::{Client, ClientOptions, Direction, JigError, ProtocolTap, TapEntry};
use serde_json::Value;

/// Path to the freshly built mock-server binary for this test run.
fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// A `--chaos <mode>` argument vector.
fn chaos_args(mode: &str) -> Vec<String> {
    vec!["--chaos".to_string(), mode.to_string()]
}

/// Client options with an explicit request timeout and the default size cap.
fn opts(timeout: Duration) -> ClientOptions {
    ClientOptions {
        request_timeout: Some(timeout),
        ..ClientOptions::default()
    }
}

/// A timeout generous enough that the handshake always completes locally and on
/// CI, but short enough that the timeout-based chaos tests stay quick. The
/// `Timeout` error quotes this configured value verbatim, so it is snapshot
/// stable regardless of the machine's real elapsed time.
const OP_TIMEOUT: Duration = Duration::from_secs(2);

/// Connect over stdio with a chaos mode and the given options.
async fn connect_chaos(mode: &str, options: ClientOptions) -> Result<Client, JigError> {
    Client::connect_with_options(
        &mock_server(),
        &chaos_args(mode),
        ProtocolTap::new(),
        options,
    )
    .await
}

/// The id Jig assigned to the (single) outbound request for `method`.
fn outbound_request_id(entries: &[TapEntry], method: &str) -> Option<i64> {
    entries
        .iter()
        .find(|e| e.direction == Direction::Outbound && e.method() == Some(method))
        .and_then(|e| e.message.get("id").and_then(Value::as_i64))
}

/// How many inbound messages carry `id`.
fn inbound_with_id(entries: &[TapEntry], id: i64) -> usize {
    entries
        .iter()
        .filter(|e| {
            e.direction == Direction::Inbound
                && e.message.get("id").and_then(Value::as_i64) == Some(id)
        })
        .count()
}

// ---------------------------------------------------------------------------
// Handshake-phase modes: exit code + captured stderr must reach the operator.
// ---------------------------------------------------------------------------

/// `immediate-exit`: the server dies right after spawn. Connecting must fail
/// with a clear "server process exited" error that includes the exit code (3)
/// and the child's captured stderr — never a panic, never a hang.
#[tokio::test]
async fn immediate_exit_reports_exit_code_and_stderr() {
    // `Client` is not `Debug`, so match rather than `expect_err`.
    let err = match connect_chaos("immediate-exit", opts(OP_TIMEOUT)).await {
        Ok(_) => panic!("connecting to a server that exits immediately must fail"),
        Err(e) => e,
    };

    let msg = err.to_string();
    assert!(msg.contains("exited with code 3"), "got: {msg}");
    assert!(msg.contains("immediate-exit"), "stderr not surfaced: {msg}");
    insta::assert_snapshot!("immediate_exit_error", msg);
}

/// `mid-session-crash`: the handshake succeeds, then the server exits before
/// answering the first real request. That request must fail with the exit code
/// (7) and the server's stderr.
#[tokio::test]
async fn mid_session_crash_reports_exit_code_and_stderr() {
    let client = connect_chaos("mid-session-crash", opts(OP_TIMEOUT))
        .await
        .expect("handshake completes before the crash");

    let err = client
        .list_tools()
        .await
        .expect_err("a mid-session crash must surface as an error");

    let msg = err.to_string();
    assert!(msg.contains("exited with code 7"), "got: {msg}");
    assert!(
        msg.contains("mid-session-crash"),
        "stderr not surfaced: {msg}"
    );
    insta::assert_snapshot!("mid_session_crash_error", msg);
}

// ---------------------------------------------------------------------------
// Malformed / mis-framed output: warn + time out, never hang.
// ---------------------------------------------------------------------------

/// `malformed-json`: a truncated JSON line for `tools/list`. Jig records the
/// stdout pollution and the request times out with the method named.
#[tokio::test]
async fn malformed_json_times_out_and_flags_pollution() {
    let client = connect_chaos("malformed-json", opts(OP_TIMEOUT))
        .await
        .expect("handshake is normal under malformed-json");

    let err = client.list_tools().await.expect_err("must time out");
    assert!(
        matches!(&err, JigError::Timeout { method, .. } if method == "tools/list"),
        "got: {err:?}"
    );

    let bad = client.tap().non_protocol_inbound();
    assert!(
        !bad.is_empty(),
        "the garbled line must be flagged as pollution"
    );
    assert!(
        bad.iter().any(|(_, raw)| raw.contains("\"result\"")),
        "expected the truncated fragment in the tap: {bad:?}"
    );
    insta::assert_snapshot!("malformed_json_timeout", err.to_string());
}

/// `binary-garbage`: raw non-UTF-8 bytes on stdout. Jig must decode lossily,
/// flag the pollution, and still complete the handshake and list tools — never
/// abort the whole stream.
#[tokio::test]
async fn binary_garbage_is_survived_and_flagged() {
    let client = connect_chaos("binary-garbage", opts(OP_TIMEOUT))
        .await
        .expect("handshake must survive non-UTF-8 stdout pollution");

    assert_eq!(client.server_info().name, "jig-mock-server");
    let tools = client.list_tools().await.expect("tools/list still works");
    assert_eq!(tools.len(), 3);

    let bad = client.tap().non_protocol_inbound();
    assert!(
        !bad.is_empty(),
        "binary garbage must be flagged as pollution"
    );
}

// ---------------------------------------------------------------------------
// Size: handle a giant message under the cap, reject it over the cap.
// ---------------------------------------------------------------------------

/// `giant-message` under the default 64 MiB cap: Jig must simply handle the
/// ~20 MiB response.
#[tokio::test]
async fn giant_message_handled_under_default_cap() {
    let client = connect_chaos("giant-message", ClientOptions::default())
        .await
        .expect("handshake is normal");

    let tools = client
        .list_tools()
        .await
        .expect("a 20 MiB response is under the 64 MiB default cap");
    assert_eq!(tools.len(), 1);
    let desc = tools[0].description.as_deref().unwrap_or("");
    assert!(
        desc.len() >= 20 * 1024 * 1024,
        "expected a ~20 MiB description"
    );
}

/// `giant-message` under a low `--max-message-bytes`: Jig must fail with a
/// clear, size-specific error rather than buffer without limit.
#[tokio::test]
async fn giant_message_rejected_over_low_cap() {
    let options = ClientOptions {
        request_timeout: Some(OP_TIMEOUT),
        // 1 MiB: the small handshake fits, the 20 MiB list does not.
        max_message_bytes: Some(1024 * 1024),
        ..ClientOptions::default()
    };
    let client = connect_chaos("giant-message", options)
        .await
        .expect("the small handshake fits under a 1 MiB cap");

    let err = client
        .list_tools()
        .await
        .expect_err("a 20 MiB response must be rejected under a 1 MiB cap");
    assert!(
        matches!(err, JigError::MessageTooLarge { limit } if limit == 1024 * 1024),
        "got: {err:?}"
    );
    insta::assert_snapshot!("giant_message_too_large", err.to_string());
}

// ---------------------------------------------------------------------------
// Timing / correlation misbehaviour.
// ---------------------------------------------------------------------------

/// `slow-drip`: the response arrives one byte at a time. Jig must reassemble it
/// and complete under a generous timeout.
#[tokio::test]
async fn slow_drip_completes() {
    // Generous timeout: the drip is bounded but not instant.
    let client = connect_chaos("slow-drip", opts(Duration::from_secs(20)))
        .await
        .expect("handshake is normal");

    let tools = client
        .list_tools()
        .await
        .expect("a byte-at-a-time response must still be reassembled");
    assert_eq!(tools.len(), 3);
}

/// `wrong-id`: the server answers with an id that was never requested. Jig
/// cannot correlate it, so the real request times out; the stray response is
/// still recorded in the tap.
#[tokio::test]
async fn wrong_id_times_out_and_records_the_stray_response() {
    let client = connect_chaos("wrong-id", opts(OP_TIMEOUT))
        .await
        .expect("handshake is normal");

    let err = client.list_tools().await.expect_err("must time out");
    assert!(
        matches!(&err, JigError::Timeout { method, .. } if method == "tools/list"),
        "got: {err:?}"
    );

    // The stray response (id 987654321) is present in the tap, but was routed
    // nowhere (the real tools/list id received nothing).
    let entries = client.tap().entries();
    assert_eq!(
        inbound_with_id(&entries, 987654321),
        1,
        "stray response missing"
    );
    let real_id = outbound_request_id(&entries, "tools/list").expect("a tools/list request");
    assert_eq!(
        inbound_with_id(&entries, real_id),
        0,
        "the real id got no answer"
    );
}

/// `duplicate-id`: the server answers the same request twice. The first answer
/// wins and the call succeeds; the surplus second answer lands in the tap.
#[tokio::test]
async fn duplicate_id_first_wins_second_recorded() {
    let client = connect_chaos("duplicate-id", opts(OP_TIMEOUT))
        .await
        .expect("handshake is normal");

    let tools = client
        .list_tools()
        .await
        .expect("the first of two answers completes the call");
    assert_eq!(tools.len(), 3);

    // A follow-up round-trip guarantees the reader has consumed the surplus
    // second answer (pipe reads are sequential) before we inspect the tap —
    // otherwise we would race the background reader.
    let echo = client
        .call_tool("echo", serde_json::json!({ "text": "after dup" }))
        .await
        .expect("a normal call still works after the duplicate");
    assert!(!echo.is_error);

    // Both answers to the tools/list id are recorded — the second is surplus.
    let entries = client.tap().entries();
    let real_id = outbound_request_id(&entries, "tools/list").expect("a tools/list request");
    assert_eq!(
        inbound_with_id(&entries, real_id),
        2,
        "expected the duplicate answer to be recorded in the tap"
    );
}

/// `no-newline`: a valid JSON response with no trailing newline, and the server
/// stays alive. Nothing is ever framed, so the request must time out with the
/// method named, and the tap must show that no response arrived.
#[tokio::test]
async fn no_newline_times_out_with_nothing_delivered() {
    let client = connect_chaos("no-newline", opts(OP_TIMEOUT))
        .await
        .expect("handshake is normal");

    let err = client.list_tools().await.expect_err("must time out");
    assert!(
        matches!(&err, JigError::Timeout { method, .. } if method == "tools/list"),
        "got: {err:?}"
    );

    // The un-framed bytes never became a message: nothing inbound carries the
    // tools/list id, and nothing was flagged as (framed) pollution either.
    let entries = client.tap().entries();
    let real_id = outbound_request_id(&entries, "tools/list").expect("a tools/list request");
    assert_eq!(
        inbound_with_id(&entries, real_id),
        0,
        "an un-terminated line must not be delivered as a response"
    );
}
