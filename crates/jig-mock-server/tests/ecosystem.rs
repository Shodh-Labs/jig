//! `jig search` / `jig info` against a **local** HTTP double.
//!
//! A tiny axum server serves canned registry + npm responses, so the merge
//! order, per-source failure degradation, and npm MCP-filter behaviour are all
//! asserted with zero live network. (A manual live smoke against the real
//! registry/npm is documented in the README, but never committed as a test.)

use std::net::SocketAddr;

use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use jig_core::ecosystem::{self, EcoSource, SourceSelector};
use serde_json::{json, Value};

/// Canned MCP-registry `/v0/servers` response: two servers.
async fn registry_servers(
    Query(_q): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    Json(json!({
        "servers": [
            {
                "server": {
                    "name": "io.github.acme/db",
                    "description": "A database MCP server.",
                    "version": "1.2.0"
                },
                "_meta": { "status": "active" }
            },
            {
                "server": {
                    "name": "io.github.acme/files",
                    "description": "A filesystem MCP server.",
                    "version": "0.9.0"
                }
            }
        ],
        "metadata": { "count": 2 }
    }))
}

/// Canned npm `/-/v1/search` response: one plausible MCP package plus one
/// unrelated package that the filter must drop.
async fn npm_search(Query(_q): Query<std::collections::HashMap<String, String>>) -> Json<Value> {
    Json(json!({
        "objects": [
            {
                "package": {
                    "name": "acme-mcp-server",
                    "description": "An MCP server on npm.",
                    "version": "0.4.1",
                    "keywords": ["mcp", "ai"]
                }
            },
            {
                "package": {
                    "name": "totally-unrelated-lib",
                    "description": "Not an MCP server at all.",
                    "version": "3.0.0",
                    "keywords": ["math"]
                }
            }
        ]
    }))
}

/// Canned npm per-package endpoint. `known-mcp` exists; anything else 404s.
async fn npm_package(Path(pkg): Path<String>) -> Result<Json<Value>, axum::http::StatusCode> {
    if pkg == "known-mcp" {
        Ok(Json(json!({
            "name": "known-mcp",
            "description": "A known MCP package.",
            "dist-tags": { "latest": "2.1.0" },
            "time": { "2.1.0": "2026-03-04T10:00:00Z" }
        })))
    } else {
        Err(axum::http::StatusCode::NOT_FOUND)
    }
}

/// Spin up the double on an ephemeral port; return its base URL.
async fn spawn_server() -> String {
    let app = Router::new()
        .route("/v0/servers", get(registry_servers))
        .route("/-/v1/search", get(npm_search))
        .route("/{pkg}", get(npm_package));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn search_merges_registry_first_and_filters_npm() {
    let base = spawn_server().await;
    let outcome = ecosystem::search("db", SourceSelector::All, 20, &base, &base).await;

    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    assert!(outcome.any_success);

    // Registry hits come first, then the single npm hit that passed the filter.
    let sources: Vec<EcoSource> = outcome.results.iter().map(|r| r.source).collect();
    assert_eq!(
        sources,
        vec![EcoSource::Registry, EcoSource::Registry, EcoSource::Npm]
    );
    // The unrelated npm package was filtered out.
    assert!(!outcome
        .results
        .iter()
        .any(|r| r.name == "totally-unrelated-lib"));
    // Registry entry carried its version through.
    let db = outcome
        .results
        .iter()
        .find(|r| r.name == "io.github.acme/db")
        .unwrap();
    assert_eq!(db.version.as_deref(), Some("1.2.0"));
}

#[tokio::test]
async fn search_registry_only_and_npm_only_selectors() {
    let base = spawn_server().await;

    let reg = ecosystem::search("x", SourceSelector::Registry, 20, &base, &base).await;
    assert!(reg.results.iter().all(|r| r.source == EcoSource::Registry));
    assert_eq!(reg.results.len(), 2);

    let npm = ecosystem::search("x", SourceSelector::Npm, 20, &base, &base).await;
    assert!(npm.results.iter().all(|r| r.source == EcoSource::Npm));
    assert_eq!(npm.results.len(), 1);
}

#[tokio::test]
async fn one_source_down_degrades_gracefully_and_still_succeeds() {
    let base = spawn_server().await;
    // Point the registry at a dead address (connection refused); npm stays live.
    let dead = "http://127.0.0.1:1";

    let outcome = ecosystem::search("db", SourceSelector::All, 20, dead, &base).await;

    // npm results are still present...
    assert!(outcome.any_success, "npm should have succeeded");
    assert!(outcome.results.iter().any(|r| r.source == EcoSource::Npm));
    // ...and the registry failure is reported, not swallowed.
    assert!(outcome
        .errors
        .iter()
        .any(|(s, _)| *s == EcoSource::Registry));
}

#[tokio::test]
async fn all_sources_down_reports_no_success() {
    let dead = "http://127.0.0.1:1";
    let outcome = ecosystem::search("db", SourceSelector::All, 20, dead, dead).await;
    assert!(!outcome.any_success);
    assert_eq!(outcome.errors.len(), 2);
    assert!(outcome.results.is_empty());
}

#[tokio::test]
async fn info_npm_found_and_not_found() {
    let base = spawn_server().await;

    let found = ecosystem::npm_info(&base, "known-mcp").await.unwrap();
    let info = found.expect("known-mcp should be found");
    assert_eq!(info.name, "known-mcp");
    assert_eq!(info.version.as_deref(), Some("2.1.0"));
    assert_eq!(info.published.as_deref(), Some("2026-03-04T10:00:00Z"));
    assert_eq!(info.install, "npx -y known-mcp");

    // A 404 is Ok(None) — "not found in npm", not an error.
    let missing = ecosystem::npm_info(&base, "does-not-exist").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn info_registry_exact_name_match() {
    let base = spawn_server().await;
    let found = ecosystem::registry_info(&base, "io.github.acme/files")
        .await
        .unwrap();
    let info = found.expect("exact registry name should match");
    assert_eq!(info.name, "io.github.acme/files");
    assert_eq!(info.version.as_deref(), Some("0.9.0"));

    // A name the registry does not carry is Ok(None).
    let missing = ecosystem::registry_info(&base, "io.github.nobody/nope")
        .await
        .unwrap();
    assert!(missing.is_none());
}
