//! **Ecosystem search & lookup**: find MCP servers that exist *out there* (not
//! just the ones already configured locally) and fetch detail on one.
//!
//! Two independent sources are queried, always concurrently, always labelled:
//!
//! * the **official MCP registry** at [`REGISTRY_BASE`], and
//! * the **npm registry** at [`NPM_BASE`].
//!
//! A failure of one source never fails the other: [`search`] returns whatever
//! succeeded alongside a per-source error list, so the CLI can degrade
//! gracefully (show npm results even when the registry is unreachable) and pick
//! its exit code from whether *any* source succeeded.
//!
//! # Version-comment: registry API assumptions
//!
//! Coded against the registry OpenAPI as published 2026-07 (`GET /v0/servers`,
//! query params `search`/`limit`/`cursor`, response
//! `{ servers: [ { server: { name, description, version, ... } } ], metadata }`).
//! The registry is young and its shape may shift; every field read here is
//! treated as optional so a schema change degrades to a thinner result rather
//! than an error. The endpoint version is pinned in [`REGISTRY_SERVERS_PATH`].

use std::time::Duration;

use serde_json::Value;

/// Base URL of the official MCP registry.
pub const REGISTRY_BASE: &str = "https://registry.modelcontextprotocol.io";
/// Base URL of the public npm registry.
pub const NPM_BASE: &str = "https://registry.npmjs.org";

/// The registry servers endpoint path. Pinned to `/v0/servers` (the version in
/// service as of this writing); bump here if the registry graduates the API.
pub const REGISTRY_SERVERS_PATH: &str = "/v0/servers";

/// Default per-request network timeout for ecosystem calls.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Which ecosystem source a result or error came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcoSource {
    /// The official MCP registry.
    Registry,
    /// The npm registry.
    Npm,
}

impl EcoSource {
    /// A short display label.
    pub fn label(self) -> &'static str {
        match self {
            EcoSource::Registry => "registry",
            EcoSource::Npm => "npm",
        }
    }
}

/// One search hit, normalized across sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// The server / package name.
    pub name: String,
    /// A one-line description, if the source provided one.
    pub description: Option<String>,
    /// The version string, if known.
    pub version: Option<String>,
    /// Which source this hit came from.
    pub source: EcoSource,
}

/// The outcome of a [`search`]: merged results plus any per-source failures.
#[derive(Debug, Default)]
pub struct SearchOutcome {
    /// Merged results, registry hits first, then npm.
    pub results: Vec<SearchResult>,
    /// Per-source failure messages (empty when a source succeeded).
    pub errors: Vec<(EcoSource, String)>,
    /// Set true when at least one source returned successfully (even if empty).
    pub any_success: bool,
}

/// Which sources to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSelector {
    /// Both the registry and npm.
    All,
    /// The MCP registry only.
    Registry,
    /// npm only.
    Npm,
}

/// Build the shared HTTP client used for ecosystem calls, with a sane timeout.
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .user_agent(concat!("jig/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Search the ecosystem for `query`, hitting the selected sources concurrently.
///
/// `registry_base` / `npm_base` are injectable so tests can point at a local
/// server; production callers pass [`REGISTRY_BASE`] / [`NPM_BASE`]. `limit`
/// bounds each source's contribution.
pub async fn search(
    query: &str,
    sources: SourceSelector,
    limit: usize,
    registry_base: &str,
    npm_base: &str,
) -> SearchOutcome {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            return SearchOutcome {
                results: vec![],
                errors: vec![(EcoSource::Registry, e.clone()), (EcoSource::Npm, e)],
                any_success: false,
            };
        }
    };

    let want_registry = matches!(sources, SourceSelector::All | SourceSelector::Registry);
    let want_npm = matches!(sources, SourceSelector::All | SourceSelector::Npm);

    // Run both concurrently; each future is a no-op when its source is off.
    let (reg, npm) = tokio::join!(
        async {
            if want_registry {
                Some(registry_search(&client, registry_base, query, limit).await)
            } else {
                None
            }
        },
        async {
            if want_npm {
                Some(npm_search(&client, npm_base, query, limit).await)
            } else {
                None
            }
        },
    );

    let mut outcome = SearchOutcome::default();
    // Registry first in the merged list.
    if let Some(result) = reg {
        match result {
            Ok(mut hits) => {
                outcome.any_success = true;
                outcome.results.append(&mut hits);
            }
            Err(e) => outcome.errors.push((EcoSource::Registry, e)),
        }
    }
    if let Some(result) = npm {
        match result {
            Ok(mut hits) => {
                outcome.any_success = true;
                outcome.results.append(&mut hits);
            }
            Err(e) => outcome.errors.push((EcoSource::Npm, e)),
        }
    }
    outcome
}

/// Query the MCP registry's `/v0/servers?search=…` endpoint.
async fn registry_search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, String> {
    let url = format!("{}{}", base.trim_end_matches('/'), REGISTRY_SERVERS_PATH);
    let resp = client
        .get(&url)
        .query(&[
            ("search", query.to_string()),
            ("limit", limit.clamp(1, 100).to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("registry unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("registry returned HTTP {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("registry sent invalid JSON: {e}"))?;

    let servers = body
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for item in servers.iter().take(limit) {
        // The server object may be nested under `server` (current shape) or be
        // the item itself (defensive against a flattened variant).
        let s = item.get("server").unwrap_or(item);
        let Some(name) = s.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(SearchResult {
            name: name.to_string(),
            description: s
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            version: s.get("version").and_then(Value::as_str).map(str::to_string),
            source: EcoSource::Registry,
        });
    }
    Ok(out)
}

/// Query npm's search endpoint and filter to plausible MCP servers.
///
/// The filter (documented for honesty): a hit is kept when its `keywords`
/// contain `mcp` or `model context protocol` (case-insensitive), **or** its
/// package name contains `mcp`. npm has no "is an MCP server" flag, so this is a
/// heuristic; it errs toward inclusion of anything self-identifying as MCP.
async fn npm_search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, String> {
    let url = format!("{}/-/v1/search", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        // `text` combines the user query with `mcp` to bias npm's ranker.
        .query(&[("text", format!("{query} mcp")), ("size", "20".to_string())])
        .send()
        .await
        .map_err(|e| format!("npm unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("npm returned HTTP {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("npm sent invalid JSON: {e}"))?;

    let objects = body
        .get("objects")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for obj in &objects {
        let Some(pkg) = obj.get("package") else {
            continue;
        };
        let Some(name) = pkg.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !looks_like_mcp(pkg, name) {
            continue;
        }
        out.push(SearchResult {
            name: name.to_string(),
            description: pkg
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            version: pkg
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string),
            source: EcoSource::Npm,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// The MCP-plausibility filter for an npm package object. See [`npm_search`].
fn looks_like_mcp(pkg: &Value, name: &str) -> bool {
    if name.to_ascii_lowercase().contains("mcp") {
        return true;
    }
    if let Some(keywords) = pkg.get("keywords").and_then(Value::as_array) {
        for kw in keywords {
            if let Some(k) = kw.as_str() {
                let k = k.to_ascii_lowercase();
                if k.contains("mcp") || k.contains("model context protocol") {
                    return true;
                }
            }
        }
    }
    false
}

/// Detailed info about a package from npm's per-package endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmInfo {
    /// Package name.
    pub name: String,
    /// Description, if any.
    pub description: Option<String>,
    /// The `latest` dist-tag version, if any.
    pub version: Option<String>,
    /// ISO-8601 publish time of `version`, if the registry reported it.
    pub published: Option<String>,
    /// The suggested one-line install/run command.
    pub install: String,
}

/// Detailed info about a server from the MCP registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryInfo {
    /// Reverse-DNS server name.
    pub name: String,
    /// Version, if any.
    pub version: Option<String>,
    /// Description, if any.
    pub description: Option<String>,
}

/// Look up one package on npm (`GET {npm_base}/{pkg}`).
///
/// A `404` is reported as `Ok(None)` ("not found in npm"), distinct from an
/// `Err` (the registry was unreachable or misbehaved).
pub async fn npm_info(npm_base: &str, pkg: &str) -> Result<Option<NpmInfo>, String> {
    let client = http_client()?;
    let url = format!("{}/{}", npm_base.trim_end_matches('/'), pkg);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("npm unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("npm returned HTTP {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("npm sent invalid JSON: {e}"))?;

    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(pkg)
        .to_string();
    let version = body
        .get("dist-tags")
        .and_then(|t| t.get("latest"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let published = version
        .as_ref()
        .and_then(|v| body.get("time").and_then(|t| t.get(v)))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(NpmInfo {
        description: body
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        install: format!("npx -y {name}"),
        name,
        version,
        published,
    }))
}

/// Look up one server by name in the MCP registry.
///
/// Registry names are reverse-DNS; this searches and then matches the exact
/// name (case-insensitive). `Ok(None)` means "not found in registry".
pub async fn registry_info(
    registry_base: &str,
    name: &str,
) -> Result<Option<RegistryInfo>, String> {
    let client = http_client()?;
    let hits = registry_search(&client, registry_base, name, 50).await?;
    Ok(hits
        .into_iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| RegistryInfo {
            name: h.name,
            version: h.version,
            description: h.description,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn npm_filter_keeps_mcp_named_and_keyworded() {
        assert!(looks_like_mcp(&json!({}), "some-mcp-server"));
        assert!(looks_like_mcp(
            &json!({ "keywords": ["MCP", "tool"] }),
            "plain-name"
        ));
        assert!(looks_like_mcp(
            &json!({ "keywords": ["Model Context Protocol"] }),
            "plain-name"
        ));
        assert!(!looks_like_mcp(
            &json!({ "keywords": ["cli", "tool"] }),
            "plain-name"
        ));
        assert!(!looks_like_mcp(&json!({}), "unrelated"));
    }

    #[test]
    fn source_labels() {
        assert_eq!(EcoSource::Registry.label(), "registry");
        assert_eq!(EcoSource::Npm.label(), "npm");
    }
}
