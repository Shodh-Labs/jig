//! `jig search` and `jig info` — discover MCP servers that exist in the wider
//! ecosystem (the official MCP registry and npm) and inspect one in detail.

use std::process::ExitCode;
use std::time::Duration;

use jig_core::ecosystem::{self, NpmInfo, RegistryInfo, SearchOutcome, SourceSelector};
use jig_core::{tokens, Client, ClientOptions, ProtocolTap};
use serde_json::json;

use crate::{emit, emit_line};

/// Max characters of a description shown in the human search table.
const DESC_MAX: usize = 60;

/// Consent delay before `jig info --probe` runs third-party code (skipped by
/// `--yes`).
const PROBE_CONSENT_DELAY: Duration = Duration::from_secs(2);

/// Per-request timeout used for the probe handshake. Generous because a cold
/// `npx -y <pkg>` first downloads the package, which routinely takes far longer
/// than the default 30 s request timeout (worse on Windows).
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Run `jig search <query>`.
pub async fn run_search(
    query: &str,
    sources: SourceSelector,
    limit: usize,
    as_json: bool,
) -> Result<ExitCode, String> {
    let outcome = ecosystem::search(
        query,
        sources,
        limit,
        ecosystem::REGISTRY_BASE,
        ecosystem::NPM_BASE,
    )
    .await;

    // Per-source failures degrade gracefully: report each on stderr. The core
    // messages already name the source (e.g. "registry unreachable: …").
    for (_source, err) in &outcome.errors {
        eprintln!("jig: warning: {err}");
    }

    if as_json {
        emit_line(&render_search_json(&outcome));
    } else {
        emit(&render_search_human(&outcome, limit));
    }

    // Exit 0 if any source succeeded; 1 only if all failed.
    if outcome.any_success {
        Ok(ExitCode::SUCCESS)
    } else {
        Err("all search sources failed".to_string())
    }
}

/// Render search results as a human table, registry first.
pub fn render_search_human(outcome: &SearchOutcome, limit: usize) -> String {
    let mut s = String::new();
    let shown: Vec<_> = outcome.results.iter().take(limit).collect();

    if shown.is_empty() {
        if outcome.any_success {
            s.push_str("No matching MCP servers found.\n");
        } else {
            s.push_str("No results — every source was unreachable (see warnings above).\n");
        }
        return s;
    }

    let rows: Vec<[String; 4]> = shown
        .iter()
        .map(|r| {
            [
                r.name.clone(),
                r.source.label().to_string(),
                r.version.clone().unwrap_or_else(|| "-".to_string()),
                truncate(r.description.as_deref().unwrap_or(""), DESC_MAX),
            ]
        })
        .collect();

    let headers = ["NAME", "SOURCE", "VERSION", "DESCRIPTION"];
    let mut widths = [0usize; 4];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let fmt = |row: &[String; 4]| -> String {
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{:<width$}", c, width = widths[i]));
        }
        format!("{}\n", line.trim_end())
    };

    let header_row: [String; 4] = headers.map(String::from);
    s.push_str(&fmt(&header_row));
    let total: usize = widths.iter().sum::<usize>() + 2 * (widths.len() - 1);
    s.push_str(&"-".repeat(total));
    s.push('\n');
    for row in &rows {
        s.push_str(&fmt(row));
    }
    s
}

/// Render search results as JSON, including any per-source errors.
pub fn render_search_json(outcome: &SearchOutcome) -> String {
    let results: Vec<_> = outcome
        .results
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "description": r.description,
                "version": r.version,
                "source": r.source.label(),
            })
        })
        .collect();
    let errors: Vec<_> = outcome
        .errors
        .iter()
        .map(|(source, msg)| json!({ "source": source.label(), "error": msg }))
        .collect();
    let doc = json!({
        "results": results,
        "errors": errors,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// Run `jig info <name-or-package>`.
pub async fn run_info(
    name: &str,
    probe: bool,
    yes: bool,
    as_json: bool,
) -> Result<ExitCode, String> {
    // Static lookups run concurrently: registry (by name) and npm (by package).
    let (registry, npm) = tokio::join!(
        ecosystem::registry_info(ecosystem::REGISTRY_BASE, name),
        ecosystem::npm_info(ecosystem::NPM_BASE, name),
    );

    // A source *error* (unreachable) is reported but does not abort — the other
    // source may still have an answer.
    let registry = match registry {
        Ok(v) => v,
        Err(e) => {
            eprintln!("jig: warning: {e}");
            None
        }
    };
    let npm = match npm {
        Ok(v) => v,
        Err(e) => {
            eprintln!("jig: warning: {e}");
            None
        }
    };

    let probe_result = if probe {
        Some(probe_package(name, yes).await)
    } else {
        None
    };

    if as_json {
        emit_line(&render_info_json(
            name,
            &registry,
            &npm,
            probe_result.as_ref(),
        ));
    } else {
        emit(&render_info_human(name, &registry, &npm));
        if let Some(pr) = &probe_result {
            emit(&render_probe_human(pr));
        }
    }

    // A probe failure is the only thing that makes `info` exit non-zero.
    match probe_result {
        Some(Err(e)) => Err(e),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// Render the static (registry + npm) info block for a person.
pub fn render_info_human(
    name: &str,
    registry: &Option<RegistryInfo>,
    npm: &Option<NpmInfo>,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("Info for: {name}\n\n"));

    s.push_str("registry (registry.modelcontextprotocol.io):\n");
    match registry {
        Some(r) => {
            s.push_str(&format!("  name:        {}\n", r.name));
            if let Some(v) = &r.version {
                s.push_str(&format!("  version:     {v}\n"));
            }
            if let Some(d) = &r.description {
                s.push_str(&format!("  description: {}\n", truncate(d, 200)));
            }
        }
        None => s.push_str("  not found in registry\n"),
    }

    s.push('\n');
    s.push_str("npm (registry.npmjs.org):\n");
    match npm {
        Some(n) => {
            s.push_str(&format!("  package:     {}\n", n.name));
            if let Some(v) = &n.version {
                s.push_str(&format!("  version:     {v}\n"));
            }
            if let Some(p) = &n.published {
                s.push_str(&format!("  published:   {p}\n"));
            }
            if let Some(d) = &n.description {
                s.push_str(&format!("  description: {}\n", truncate(d, 200)));
            }
            s.push_str(&format!("  install:     {}\n", n.install));
        }
        None => s.push_str("  not found in npm\n"),
    }
    s
}

/// Render the full `jig info` JSON document.
pub fn render_info_json(
    name: &str,
    registry: &Option<RegistryInfo>,
    npm: &Option<NpmInfo>,
    probe: Option<&Result<ProbeReport, String>>,
) -> String {
    let registry_json = registry
        .as_ref()
        .map(|r| json!({ "name": r.name, "version": r.version, "description": r.description }));
    let npm_json = npm.as_ref().map(|n| {
        json!({
            "name": n.name,
            "version": n.version,
            "published": n.published,
            "description": n.description,
            "install": n.install,
        })
    });
    let probe_json = probe.map(|p| match p {
        Ok(r) => json!({
            "ok": true,
            "serverInfo": { "name": r.server_name, "version": r.server_version },
            "protocolVersion": r.protocol_version,
            "toolCount": r.tool_names.len(),
            "tools": r.tool_names,
            "totalContextTokens": r.total_tokens,
        }),
        Err(e) => json!({ "ok": false, "error": e }),
    });
    let doc = json!({
        "name": name,
        "registry": registry_json,
        "npm": npm_json,
        "probe": probe_json,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// The live-handshake facts gathered by `--probe`.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// The server's advertised name.
    pub server_name: String,
    /// The server's advertised version.
    pub server_version: String,
    /// The negotiated protocol version.
    pub protocol_version: String,
    /// The capability keys the server advertised.
    pub capabilities: Vec<String>,
    /// The names of the tools it exposes.
    pub tool_names: Vec<String>,
    /// Total context-token cost of the tool surface (gpt-4o, exact).
    pub total_tokens: usize,
}

/// Actually run `npx -y <pkg>` over stdio and report the live handshake.
///
/// The security model is a printed notice plus a short delay — no interactive
/// prompt (this is a CLI). `--yes` skips the delay.
async fn probe_package(pkg: &str, yes: bool) -> Result<ProbeReport, String> {
    eprintln!(
        "jig: probe runs third-party code from npm on your machine (npx -y {pkg}) — \
         Ctrl-C to abort"
    );
    if !yes {
        tokio::time::sleep(PROBE_CONSENT_DELAY).await;
    }

    let tap = ProtocolTap::new();
    let args: Vec<String> = vec!["-y".to_string(), pkg.to_string()];
    let opts = ClientOptions {
        request_timeout: Some(PROBE_REQUEST_TIMEOUT),
        ..ClientOptions::default()
    };
    let client = Client::connect_with_env("npx", &args, &[], tap, opts)
        .await
        .map_err(|e| format!("failed to launch '{pkg}' via npx: {e}"))?;

    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let instructions = client.instructions().map(str::to_string);
    let server = client.server_info().clone();
    let protocol_version = client.protocol_version().to_string();
    let capabilities: Vec<String> = client
        .capabilities()
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // Reuse the budget engine: total context tokens on gpt-4o (exact).
    let budget = tokens::budget_local("gpt-4o", &tools, instructions.as_deref())
        .map_err(|e| e.to_string())?;

    let report = ProbeReport {
        server_name: server.name,
        server_version: server.version,
        protocol_version,
        capabilities,
        tool_names: tools.iter().map(|t| t.name.clone()).collect(),
        total_tokens: budget.total,
    };

    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok(report)
}

/// Render a probe result for a person.
pub fn render_probe_human(probe: &Result<ProbeReport, String>) -> String {
    let mut s = String::new();
    s.push('\n');
    match probe {
        Ok(r) => {
            s.push_str("probe (live handshake via npx):\n");
            s.push_str(&format!(
                "  server:      {} v{}\n",
                r.server_name, r.server_version
            ));
            s.push_str(&format!("  protocol:    {}\n", r.protocol_version));
            let caps = if r.capabilities.is_empty() {
                "(none)".to_string()
            } else {
                r.capabilities.join(", ")
            };
            s.push_str(&format!("  capabilities: {caps}\n"));
            s.push_str(&format!(
                "  tools ({}):  {}\n",
                r.tool_names.len(),
                r.tool_names.join(", ")
            ));
            s.push_str(&format!(
                "  context:     {} tokens (gpt-4o, exact)\n",
                r.total_tokens
            ));
        }
        Err(e) => s.push_str(&format!("probe failed: {e}\n")),
    }
    s
}

/// Truncate to `max` chars (char boundary), collapsing internal whitespace to a
/// single line and appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::ecosystem::{EcoSource, SearchResult};

    fn outcome() -> SearchOutcome {
        SearchOutcome {
            results: vec![
                SearchResult {
                    name: "io.github.acme/db".to_string(),
                    description: Some("A database MCP server for querying things.".to_string()),
                    version: Some("1.2.0".to_string()),
                    source: EcoSource::Registry,
                },
                SearchResult {
                    name: "acme-mcp-server".to_string(),
                    description: Some("npm-published MCP server.".to_string()),
                    version: Some("0.4.1".to_string()),
                    source: EcoSource::Npm,
                },
            ],
            errors: vec![],
            any_success: true,
        }
    }

    #[test]
    fn search_human_lists_registry_first() {
        let out = render_search_human(&outcome(), 20);
        let reg = out.find("io.github.acme/db").unwrap();
        let npm = out.find("acme-mcp-server").unwrap();
        assert!(reg < npm, "registry should sort first:\n{out}");
    }

    #[test]
    fn search_degraded_reports_no_results_when_all_failed() {
        let degraded = SearchOutcome {
            results: vec![],
            errors: vec![(EcoSource::Registry, "boom".to_string())],
            any_success: false,
        };
        let out = render_search_human(&degraded, 20);
        assert!(out.contains("unreachable"), "got:\n{out}");
    }

    #[test]
    fn info_human_states_not_found_plainly() {
        let out = render_info_human("nope", &None, &None);
        assert!(out.contains("not found in registry"));
        assert!(out.contains("not found in npm"));
    }

    #[test]
    fn info_human_shows_install_command() {
        let npm = Some(NpmInfo {
            name: "some-mcp".to_string(),
            description: Some("desc".to_string()),
            version: Some("1.0.0".to_string()),
            published: Some("2026-01-01T00:00:00Z".to_string()),
            install: "npx -y some-mcp".to_string(),
        });
        let out = render_info_human("some-mcp", &None, &npm);
        assert!(out.contains("npx -y some-mcp"));
        assert!(out.contains("2026-01-01"));
    }

    #[test]
    fn search_results_snapshot() {
        insta::assert_snapshot!("search_results", render_search_human(&outcome(), 20));
    }

    #[test]
    fn info_static_snapshot() {
        let registry = Some(RegistryInfo {
            name: "io.github.acme/db".to_string(),
            version: Some("1.2.0".to_string()),
            description: Some("A database MCP server.".to_string()),
        });
        let npm = Some(NpmInfo {
            name: "acme-db-mcp".to_string(),
            description: Some("npm build of the acme db server.".to_string()),
            version: Some("0.4.1".to_string()),
            published: Some("2026-05-05T12:00:00Z".to_string()),
            install: "npx -y acme-db-mcp".to_string(),
        });
        insta::assert_snapshot!("info_static", render_info_human("acme-db", &registry, &npm));
    }
}
