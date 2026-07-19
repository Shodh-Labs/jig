//! End-to-end `--server` resolution: write a temp `.mcp.json` that points at the
//! real `jig-mock-server` binary, discover + resolve it by name, then connect
//! using the resolved transport **and** its declared environment — proving the
//! whole "discover → resolve → connect with env" path against a live server.
//!
//! The mock binary path comes from `CARGO_BIN_EXE_jig-mock-server` (available
//! because this test lives in the crate that defines that binary).

use jig_core::discovery::{self, DiscoveredTransport, Source};
use jig_core::{Client, ProtocolTap};

fn mock_server() -> String {
    env!("CARGO_BIN_EXE_jig-mock-server").to_string()
}

/// A unique temp directory for this test.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("jig-server-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn resolve_from_project_mcp_json_and_connect_with_env() {
    let dir = temp_dir("resolve");
    let config = dir.join(".mcp.json");

    // A project `.mcp.json` naming the mock server, with an env block whose value
    // the mock echoes back as its instructions when spawned.
    let json = serde_json::json!({
        "mcpServers": {
            "mocky": {
                "command": mock_server(),
                "args": [],
                "env": { "JIG_MOCK_INSTRUCTIONS": "instructions-from-config-env" }
            }
        }
    });
    std::fs::write(&config, serde_json::to_string_pretty(&json).unwrap()).unwrap();

    // Discover from exactly this file, then resolve by name (as `--server` does).
    let discovered = discovery::discover_from(&[(Source::ProjectMcp, config.clone())]);
    assert!(
        discovered.warnings.is_empty(),
        "warnings: {:?}",
        discovered.warnings
    );
    let entry = discovered.resolve("mocky").expect("resolve by name");

    // The resolved transport is the mock command, and the env carries our value.
    let (program, args) = match &entry.transport {
        DiscoveredTransport::Stdio { command, args } => (command.clone(), args.clone()),
        other => panic!("expected stdio transport, got {other:?}"),
    };
    assert_eq!(program, mock_server());
    assert!(entry
        .env
        .iter()
        .any(|(k, v)| k == "JIG_MOCK_INSTRUCTIONS" && v == "instructions-from-config-env"));

    // Connect exactly as `Target::connect` does for a discovered stdio server:
    // handshake succeeds, and the injected env reached the child (proven by the
    // echoed instructions).
    let client = Client::connect_with_env(
        &program,
        &args,
        &entry.env,
        ProtocolTap::new(),
        Default::default(),
    )
    .await
    .expect("handshake against the resolved server");
    assert_eq!(client.server_info().name, "jig-mock-server");
    assert_eq!(
        client.instructions(),
        Some("instructions-from-config-env"),
        "the config's env var must reach the spawned child"
    );
    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 3);
    client.shutdown().await.expect("shutdown");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ambiguous_name_across_sources_is_rejected() {
    // The same name declared in two different sources must not silently pick one.
    let dir = temp_dir("ambig");
    let a = dir.join("a.mcp.json");
    let b = dir.join("b.mcp.json");
    let spec = serde_json::json!({ "mcpServers": { "dup": { "command": mock_server() } } });
    std::fs::write(&a, spec.to_string()).unwrap();
    std::fs::write(&b, spec.to_string()).unwrap();

    let discovered = discovery::discover_from(&[(Source::ProjectMcp, a), (Source::Cursor, b)]);
    match discovered.resolve("dup") {
        Err(discovery::ResolveError::Ambiguous { candidates, .. }) => {
            assert_eq!(candidates.len(), 2, "candidates: {candidates:?}");
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
    // A `source:name` selector disambiguates.
    assert_eq!(
        discovered
            .resolve("cursor:dup")
            .expect("disambiguated")
            .source,
        Source::Cursor
    );

    let _ = std::fs::remove_dir_all(&dir);
}
