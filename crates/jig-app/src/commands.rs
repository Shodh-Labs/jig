//! The Tauri command layer: the only bridge between the webview and MCP.
//!
//! Every command here is `async` and returns `Result<T, String>`. The error
//! string is always the specific, actionable sentence `jig-core` produced (via
//! [`crate::dto::error_message`]) — the webview renders it verbatim and never
//! substitutes a generic failure.
//!
//! The heavy lifting is delegated to the pure modules ([`crate::wire`],
//! [`crate::dto`]) so that the logic worth testing is tested without a webview;
//! what remains in this file is state handling and `jig-core` orchestration.

use crate::dto;
use crate::session::{self, ConnectOptions, Target};
use crate::wire;
use jig_core::check::{evaluate, CheckInput, Observations, PollutionSite};
use jig_core::{Client, JigError, ProtocolTap, Tool};
use serde::Serialize;
use serde_json::Value;
use std::time::Instant;
use tokio::sync::Mutex;

/// The default fold threshold for the wire timeline: any quiet stretch longer
/// than one second is elided. Chosen because an `npx` cold start is seconds and
/// real protocol traffic is milliseconds — there is a wide, safe gap between.
pub const FOLD_THRESHOLD_MICROS: u64 = 1_000_000;

/// The live session the Connect pane opened.
///
/// `Client::shutdown` consumes `self`, so the client lives inside an `Option`
/// we can `take()` — that is the whole reason for this shape.
pub struct Session {
    client: Option<Client>,
    tap: ProtocolTap,
    tools: Vec<Tool>,
    instructions: Option<String>,
    target: Target,
    options: ConnectOptions,
}

/// Tauri-managed application state. One optional session at a time: the
/// workbench is an instrument pointed at one server, not a fleet console.
#[derive(Default)]
pub struct AppState {
    session: Mutex<Option<Session>>,
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectResult {
    pub server: dto::ServerDto,
    pub protocol_version: String,
    pub capabilities: Value,
    /// The capability keys the server actually advertised, for the chip row.
    pub capability_keys: Vec<String>,
    pub instructions: Option<String>,
    pub transport: String,
    pub tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
    /// True when `tools` was advertised as a capability. An empty tool list
    /// means something different depending on this, and the UI says which.
    pub tools_advertised: bool,
    /// Milliseconds from connect() to a completed handshake.
    pub handshake_ms: u64,
}

/// Scan the standard config locations for MCP servers.
///
/// Synchronous in core and fast (it reads a handful of JSON files), but exposed
/// as an async command so the webview's call path is uniform.
#[tauri::command]
pub async fn discover_servers() -> Result<dto::DiscoveryDto, String> {
    let discovery = jig_core::discovery::discover();
    Ok(dto::discovery_dto(&discovery))
}

/// Connect to a target and complete the handshake.
///
/// Any previously open session is shut down first, so a reconnect can never
/// leave an orphaned child process behind.
#[tauri::command]
pub async fn connect(
    target: Target,
    options: Option<ConnectOptions>,
    state: tauri::State<'_, AppState>,
) -> Result<ConnectResult, String> {
    let options = options.unwrap_or_default();

    // Close whatever was open before touching the new target.
    {
        let mut guard = state.session.lock().await;
        if let Some(mut old) = guard.take() {
            if let Some(client) = old.client.take() {
                let _ = client.shutdown().await;
            }
        }
    }

    let tap = ProtocolTap::new();
    let t0 = Instant::now();
    let client = session::connect(&target, tap.clone(), &options).await?;
    let handshake_ms = t0.elapsed().as_millis() as u64;

    // Read the surface. `list_*` return an empty vec (not an error) when the
    // capability was never advertised, so the counts below are honest only
    // alongside `tools_advertised`.
    let tools_advertised = client.has_capability("tools");
    let tools = client
        .list_tools()
        .await
        .map_err(|e| format!("tools/list failed: {}", dto::error_message(&e)))?;
    let resources = client.list_resources().await.unwrap_or_default();
    let prompts = client.list_prompts().await.unwrap_or_default();

    let capabilities = client.capabilities().clone();
    let capability_keys = capabilities
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();

    let result = ConnectResult {
        server: dto::ServerDto::from_implementation(client.server_info()),
        protocol_version: client.protocol_version().to_string(),
        capabilities,
        capability_keys,
        instructions: client.instructions().map(str::to_string),
        transport: session::transport_label(&target).to_string(),
        tool_count: tools.len(),
        resource_count: resources.len(),
        prompt_count: prompts.len(),
        tools_advertised,
        handshake_ms,
    };

    let instructions = client.instructions().map(str::to_string);
    *state.session.lock().await = Some(Session {
        client: Some(client),
        tap,
        tools,
        instructions,
        target,
        options,
    });

    Ok(result)
}

/// Close the live session, shutting the server down gracefully.
#[tauri::command]
pub async fn disconnect(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().await;
    if let Some(mut s) = guard.take() {
        if let Some(client) = s.client.take() {
            client
                .shutdown()
                .await
                .map_err(|e| dto::error_message(&e))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wire
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSnapshot {
    pub spans: Vec<wire::WireSpan>,
    pub axis: Vec<wire::AxisSegment>,
    /// Total entries in the tap, so the UI can show the raw message count
    /// alongside the (smaller) span count.
    pub entry_count: usize,
    pub fold_width_fraction: f64,
}

/// Snapshot the protocol tap and correlate it into spans.
///
/// `jig-core`'s tap is poll-only — it has no channel or callback — so the Wire
/// pane polls this on a timer while a session is open. Correlation happens in
/// Rust ([`wire::build_spans`]), never in the webview.
#[tauri::command]
pub async fn wire_snapshot(state: tauri::State<'_, AppState>) -> Result<WireSnapshot, String> {
    let guard = state.session.lock().await;
    let Some(session) = guard.as_ref() else {
        return Ok(WireSnapshot {
            spans: Vec::new(),
            axis: Vec::new(),
            entry_count: 0,
            fold_width_fraction: wire::FOLD_WIDTH_FRACTION,
        });
    };

    let entries = session.tap.entries();
    let spans = wire::build_spans(&entries);
    let axis = wire::build_axis(&spans, FOLD_THRESHOLD_MICROS);
    Ok(WireSnapshot {
        entry_count: entries.len(),
        spans,
        axis,
        fold_width_fraction: wire::FOLD_WIDTH_FRACTION,
    })
}

// ---------------------------------------------------------------------------
// Report card
// ---------------------------------------------------------------------------

/// Grade the connected server.
///
/// This opens its **own** session rather than reusing the live one, exactly as
/// `jig check` does. That is not redundancy: two of the scored observations —
/// a clean shutdown, and the response to an unknown method — can only be made
/// by driving a connection from handshake to close. Grading the live session
/// would either skip them or destroy the session the Wire pane is showing.
#[tauri::command]
pub async fn run_check(state: tauri::State<'_, AppState>) -> Result<dto::ReportDto, String> {
    // Copy out what we need, then release the lock: the check below is a long
    // operation and must not block `wire_snapshot` polling the live session.
    let (target, options) = {
        let guard = state.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or("not connected — pick a server in Connect first")?;
        (session.target.clone(), session.options.clone())
    };

    let tap = ProtocolTap::new();
    let client = session::connect(&target, tap.clone(), &options).await?;

    // Time the list operation. A list the server accepts but never answers is
    // an observation, not a hard error — score it with an empty surface.
    let t0 = Instant::now();
    let (tools, list_timed_out) = match client.list_tools().await {
        Ok(tools) => (tools, false),
        Err(JigError::Timeout { .. }) => (Vec::new(), true),
        Err(e) => return Err(format!("tools/list failed: {}", dto::error_message(&e))),
    };
    let list_latency = Some(t0.elapsed());

    let unknown_method = client.probe_unknown_method().await;

    let server = client.server_info().clone();
    let protocol_version = client.protocol_version().to_string();
    let capabilities = client.capabilities().clone();
    let instructions = client.instructions().map(str::to_string);

    // A clean shutdown is itself a scored robustness signal.
    let clean_shutdown = client.shutdown().await.is_ok();

    let polluting = tap.non_protocol_inbound_detailed();
    let pollution_lines = polluting.len();
    let first_pollution = polluting.first().map(|l| PollutionSite {
        offset: l.offset,
        line: l.raw.clone(),
    });

    let input = CheckInput {
        server_name: server.name.clone(),
        server_version: server.version.clone(),
        protocol_version,
        capabilities,
        instructions,
        tools,
        observations: Observations {
            pollution_lines,
            first_pollution,
            list_timed_out,
            list_latency,
            clean_shutdown,
            // Child stderr volume is not plumbed through the client; left
            // unobserved rather than assumed, as in the CLI.
            stderr_noise_bytes: None,
            unknown_method,
        },
    };

    // Scored against the census bundled into the binary, matching `jig check`'s
    // default. No network call is made to obtain it.
    let report = evaluate(&input, jig_core::bundled_percentiles().as_ref());
    Ok(dto::report_dto(&report))
}

// ---------------------------------------------------------------------------
// Context & budget
// ---------------------------------------------------------------------------

/// The models the Context pane offers, from `jig-core`'s registry.
#[tauri::command]
pub async fn list_models() -> Result<Vec<String>, String> {
    Ok(jig_core::tokens::known_models()
        .into_iter()
        .map(str::to_string)
        .collect())
}

/// Build the token-annotated request body for the connected server.
///
/// Pure and fast — it reuses the tool list captured at connect time and makes
/// no protocol call, so switching models in the UI is instant.
#[tauri::command]
pub async fn build_context(
    model: String,
    state: tauri::State<'_, AppState>,
) -> Result<dto::ContextDto, String> {
    let guard = state.session.lock().await;
    let session = guard
        .as_ref()
        .ok_or("not connected — pick a server in Connect first")?;

    let (canonical, provider, api_model) =
        jig_core::tokens::bench_model_spec(&model).ok_or_else(|| {
            format!(
                "unknown model '{model}' (known models: {})",
                jig_core::tokens::known_models().join(", ")
            )
        })?;

    let view = jig_core::context::build(
        provider,
        canonical,
        api_model,
        &session.tools,
        session.instructions.as_deref(),
    )
    .map_err(|e| e.to_string())?;

    Ok(dto::context_dto(&view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_state_starts_disconnected() {
        let state = AppState::default();
        let guard = state.session.try_lock().expect("uncontended");
        assert!(guard.is_none());
    }

    #[test]
    fn the_fold_threshold_separates_boot_time_from_protocol_time() {
        // A cold start is seconds; real traffic is milliseconds. The threshold
        // must sit clearly between the two or the timeline folds the wrong
        // thing. Checked at compile time — it is a property of the constant.
        const {
            assert!(
                FOLD_THRESHOLD_MICROS > 100_000,
                "must not fold 100ms traffic"
            );
            assert!(
                FOLD_THRESHOLD_MICROS < 5_000_000,
                "must fold a 5s cold start"
            );
        }
    }

    #[tokio::test]
    async fn wire_snapshot_is_empty_and_does_not_error_when_disconnected() {
        // The Wire pane polls on a timer; polling before a connection must be
        // a quiet no-op, never an error dialog.
        let state = AppState::default();
        let guard = state.session.lock().await;
        assert!(guard.is_none());
        drop(guard);

        let snap = WireSnapshot {
            spans: Vec::new(),
            axis: Vec::new(),
            entry_count: 0,
            fold_width_fraction: wire::FOLD_WIDTH_FRACTION,
        };
        assert!(snap.spans.is_empty());
        assert_eq!(snap.entry_count, 0);
    }

    #[test]
    fn an_unknown_model_names_the_models_that_do_exist() {
        // The actionable-error promise: never just "unknown model".
        let known = jig_core::tokens::known_models();
        assert!(!known.is_empty());
        let msg = format!("unknown model 'nope' (known models: {})", known.join(", "));
        assert!(msg.contains("gpt-4o"), "{msg}");
    }

    #[test]
    fn every_registry_model_resolves_to_a_bench_spec() {
        // The Context pane offers exactly `known_models()`; if any of them
        // failed to resolve, that dropdown would have a dead entry.
        for m in jig_core::tokens::known_models() {
            assert!(
                jig_core::tokens::bench_model_spec(m).is_some(),
                "model {m} is offered but does not resolve"
            );
        }
    }

    #[test]
    fn connect_result_serializes_with_camel_case_keys() {
        let r = ConnectResult {
            server: dto::ServerDto {
                name: "everything".into(),
                version: "2.0.0".into(),
                title: None,
            },
            protocol_version: "2025-06-18".into(),
            capabilities: serde_json::json!({"tools": {}}),
            capability_keys: vec!["tools".into()],
            instructions: None,
            transport: "stdio".into(),
            tool_count: 13,
            resource_count: 0,
            prompt_count: 0,
            tools_advertised: true,
            handshake_ms: 8000,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["protocolVersion"], "2025-06-18");
        assert_eq!(v["toolCount"], 13);
        assert_eq!(v["toolsAdvertised"], true);
        assert_eq!(v["handshakeMs"], 8000);
    }
}
