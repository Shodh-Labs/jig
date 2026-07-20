//! `jig-app` — the Jig workbench, a desktop front end for `jig-core`.
//!
//! The workbench is the same instrument as the `jig` CLI, pointed at one server
//! and made interactive. It adds **no protocol logic**: every connection,
//! handshake, grade, and token count comes from `jig-core`, and the webview
//! never speaks MCP itself — it only renders what the Rust side hands it across
//! Tauri's IPC boundary.
//!
//! # Layout
//!
//! - [`wire`] — correlating the flat protocol tap into request/response spans on
//!   a folded time axis. Pure functions; the Wire pane's whole model.
//! - [`dto`] — serializable views of `jig-core`'s analysis types, whose JSON key
//!   names deliberately mirror `jig check --json` so the two surfaces cannot
//!   drift.
//! - [`session`] — connection targets and their translation into `jig-core`
//!   client calls.
//! - [`commands`] — the Tauri command layer, and the only bridge to MCP.
//!
//! Everything worth testing lives in the first three modules and is exercised
//! without a webview, a server, or (mostly) a runtime.
//!
//! # What the app does not do
//!
//! No telemetry, no update check, no analytics, and no network call of any kind
//! beyond the MCP server the user explicitly chose. The Tauri capability set
//! enables `core:default` only — no filesystem, shell, HTTP, or clipboard
//! plugin is present to be abused.

#![forbid(unsafe_code)]

pub mod commands;
pub mod dto;
pub mod session;
pub mod wire;

/// Build and run the workbench window.
///
/// Kept in the library (rather than `main.rs`) so the binary is a one-line shim
/// and the whole app can be constructed from a test or an alternate host.
pub fn run() {
    tauri::Builder::default()
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::discover_servers,
            commands::connect,
            commands::disconnect,
            commands::wire_snapshot,
            commands::run_check,
            commands::list_models,
            commands::build_context,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the jig workbench");
}
