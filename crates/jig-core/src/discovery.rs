//! **Local MCP server discovery**: find the MCP servers already configured on
//! this machine, across the config files the popular MCP clients write, merge
//! them, and label each by where it came from.
//!
//! This closes the first half of the discovery loop — "what servers do I
//! already have?" — so `jig inspect --server <name>` can connect to one by name
//! without the user retyping its command line.
//!
//! # Foreign files, parsed tolerantly
//!
//! Every file read here is written by *another* tool (Claude Desktop, Claude
//! Code, Cursor, VS Code, a project's `.mcp.json`). Jig therefore parses them
//! defensively: unknown fields are ignored, a malformed file yields a warning
//! rather than aborting the whole scan, and the two spelling conventions for the
//! servers map (`mcpServers` and `servers`) are both accepted.
//!
//! # Secrets never leave
//!
//! Config entries frequently carry secrets in their `env` block (API keys,
//! tokens). Jig keeps the values only long enough to pass them to a spawned
//! child process; every *display* path (the human table, `--json`) prints the
//! key names with the values redacted as [`REDACTED`]. See
//! [`ServerEntry::env_display`].

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The redaction placeholder shown in place of every environment-variable value.
/// Config `env` blocks routinely hold API keys and tokens; Jig prints the key
/// names but never the values.
pub const REDACTED: &str = "\u{2022}\u{2022}\u{2022}"; // •••

/// How many directory levels to walk upward from the current directory when
/// looking for a project-scoped `.mcp.json` / `.vscode/mcp.json`.
const PROJECT_SEARCH_DEPTH: usize = 6;

/// Which tool's configuration a [`ServerEntry`] was discovered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Claude Desktop (`claude_desktop_config.json`).
    ClaudeDesktop,
    /// Claude Code user config (`~/.claude.json`).
    ClaudeCode,
    /// A project-scoped `.mcp.json`.
    ProjectMcp,
    /// Cursor (`~/.cursor/mcp.json`).
    Cursor,
    /// VS Code workspace config (`.vscode/mcp.json`).
    VsCode,
}

impl Source {
    /// A short, stable, lowercase slug used both as the display label and as the
    /// `source:` prefix accepted by `--server source:name`.
    pub fn slug(self) -> &'static str {
        match self {
            Source::ClaudeDesktop => "claude-desktop",
            Source::ClaudeCode => "claude-code",
            Source::ProjectMcp => "project",
            Source::Cursor => "cursor",
            Source::VsCode => "vscode",
        }
    }

    /// Resolve a `source:` prefix slug back to a [`Source`], if it matches one.
    pub fn from_slug(slug: &str) -> Option<Source> {
        [
            Source::ClaudeDesktop,
            Source::ClaudeCode,
            Source::ProjectMcp,
            Source::Cursor,
            Source::VsCode,
        ]
        .into_iter()
        .find(|s| s.slug().eq_ignore_ascii_case(slug))
    }
}

/// The transport an entry describes: a local stdio command or a remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredTransport {
    /// A local server launched over stdio (`command` + `args`).
    Stdio {
        /// The program to run.
        command: String,
        /// Its arguments.
        args: Vec<String>,
    },
    /// A remote server reached over HTTP/SSE at `url`.
    Http {
        /// The endpoint URL.
        url: String,
    },
}

/// One MCP server discovered in a local config file.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    /// The name the config gave this server (the map key).
    pub name: String,
    /// Which tool's config it came from.
    pub source: Source,
    /// The concrete file it was read from.
    pub source_file: PathBuf,
    /// stdio command or remote URL.
    pub transport: DiscoveredTransport,
    /// Whether the entry is marked disabled in its config.
    pub disabled: bool,
    /// Environment variables declared for the server, in `(key, value)` form.
    ///
    /// Kept so they can be handed to a spawned child process. **Never** print
    /// the values — use [`ServerEntry::env_display`] for any human/JSON output.
    pub env: Vec<(String, String)>,
}

impl ServerEntry {
    /// The environment as `(key, "•••")` pairs — safe to print. The values are
    /// redacted; only the key names are revealed.
    pub fn env_display(&self) -> Vec<(String, &'static str)> {
        self.env
            .iter()
            .map(|(k, _)| (k.clone(), REDACTED))
            .collect()
    }

    /// A one-line, secret-free summary of the transport for a table cell.
    pub fn transport_summary(&self) -> String {
        match &self.transport {
            DiscoveredTransport::Stdio { command, args } => {
                if args.is_empty() {
                    format!("stdio: {command}")
                } else {
                    format!("stdio: {} {}", command, args.join(" "))
                }
            }
            DiscoveredTransport::Http { url } => format!("http: {url}"),
        }
    }

    /// Machine-readable JSON for this entry, with `env` values redacted.
    pub fn to_json(&self) -> Value {
        let transport = match &self.transport {
            DiscoveredTransport::Stdio { command, args } => serde_json::json!({
                "type": "stdio",
                "command": command,
                "args": args,
            }),
            DiscoveredTransport::Http { url } => serde_json::json!({
                "type": "http",
                "url": url,
            }),
        };
        let env: serde_json::Map<String, Value> = self
            .env
            .iter()
            .map(|(k, _)| (k.clone(), Value::String(REDACTED.to_string())))
            .collect();
        serde_json::json!({
            "name": self.name,
            "source": self.source.slug(),
            "sourceFile": self.source_file.to_string_lossy(),
            "transport": transport,
            "disabled": self.disabled,
            "env": env,
        })
    }
}

/// The outcome of a discovery scan: the merged entries plus any non-fatal
/// warnings (a malformed file, an unreadable path). Warnings are surfaced to the
/// user but never abort the scan.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Every server found, in a stable order (by source, then by name).
    pub entries: Vec<ServerEntry>,
    /// Human-readable warnings collected while scanning (bad JSON, etc.).
    pub warnings: Vec<String>,
}

/// The error returned when `--server <name>` cannot be resolved to exactly one
/// discovered entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No entry matched the requested name (optionally within a source).
    NotFound {
        /// The name that was requested.
        requested: String,
        /// The names actually available, `source:name` formatted, for a hint.
        available: Vec<String>,
    },
    /// The bare name matched entries in more than one source; the user must
    /// disambiguate with `source:name`.
    Ambiguous {
        /// The ambiguous name.
        requested: String,
        /// The `source:name` candidates it matched.
        candidates: Vec<String>,
    },
    /// A `source:name` was given but the `source:` prefix is not a known source.
    UnknownSource {
        /// The unrecognized prefix.
        source: String,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound {
                requested,
                available,
            } => {
                if available.is_empty() {
                    write!(
                        f,
                        "no configured MCP server named '{requested}' was found \
                         (no server configs discovered on this machine)"
                    )
                } else {
                    write!(
                        f,
                        "no configured MCP server named '{requested}'. Available: {}",
                        available.join(", ")
                    )
                }
            }
            ResolveError::Ambiguous {
                requested,
                candidates,
            } => write!(
                f,
                "'{requested}' is ambiguous across sources: {}. \
                 Disambiguate with --server <source>:{requested}",
                candidates.join(", ")
            ),
            ResolveError::UnknownSource { source } => write!(
                f,
                "unknown config source '{source}' (known: claude-desktop, claude-code, \
                 project, cursor, vscode)"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl Discovery {
    /// Resolve a `--server` selector to exactly one entry.
    ///
    /// The selector is either a bare `name` or a `source:name` (e.g.
    /// `project:my-server`). A bare name that matches entries in more than one
    /// source is [`ResolveError::Ambiguous`]; a `source:name` selects within
    /// that one source only.
    pub fn resolve(&self, selector: &str) -> Result<&ServerEntry, ResolveError> {
        let (source_filter, name) = match selector.split_once(':') {
            // A Windows drive letter (`C:\...`) or a URL is not a source prefix;
            // only treat the prefix as a source when it actually names one.
            Some((prefix, rest)) if Source::from_slug(prefix).is_some() => {
                (Some(Source::from_slug(prefix).unwrap()), rest)
            }
            _ => (None, selector),
        };

        // If a prefix was given that looks like `x:y` but `x` is not a known
        // source, surface that specifically rather than a confusing not-found.
        if source_filter.is_none() {
            if let Some((prefix, _)) = selector.split_once(':') {
                // Only complain when it plausibly meant a source (no slashes —
                // those are paths/URLs, which a bare name would never contain).
                if !prefix.contains('/') && !prefix.contains('\\') && !prefix.is_empty() {
                    return Err(ResolveError::UnknownSource {
                        source: prefix.to_string(),
                    });
                }
            }
        }

        let matches: Vec<&ServerEntry> = self
            .entries
            .iter()
            .filter(|e| {
                e.name.eq_ignore_ascii_case(name)
                    && source_filter.map(|s| s == e.source).unwrap_or(true)
            })
            .collect();

        match matches.as_slice() {
            [] => Err(ResolveError::NotFound {
                requested: selector.to_string(),
                available: self
                    .entries
                    .iter()
                    .map(|e| format!("{}:{}", e.source.slug(), e.name))
                    .collect(),
            }),
            [one] => Ok(one),
            many => Err(ResolveError::Ambiguous {
                requested: name.to_string(),
                candidates: many
                    .iter()
                    .map(|e| format!("{}:{}", e.source.slug(), e.name))
                    .collect(),
            }),
        }
    }
}

/// Parse a single MCP-client config file's *contents* into server entries.
///
/// This is the pure, testable core of discovery: it takes the raw file text (so
/// tests need no filesystem) plus the [`Source`] label and the path the text
/// came from. It accepts both the `mcpServers` and `servers` top-level keys, and
/// — for the Claude Code user config — also harvests the per-project
/// `projects.<path>.mcpServers` blocks.
///
/// # Errors
///
/// Returns `Err(message)` only when the file is not parseable JSON at all. A
/// well-formed file that simply declares no servers yields an empty vector.
pub fn parse_config_contents(
    source: Source,
    path: &Path,
    contents: &str,
) -> Result<Vec<ServerEntry>, String> {
    let root: Value = serde_json::from_str(contents)
        .map_err(|e| format!("{}: malformed JSON ({e})", path.display()))?;

    let mut out = Vec::new();

    // Top-level `mcpServers` or `servers` map.
    for key in ["mcpServers", "servers"] {
        if let Some(map) = root.get(key).and_then(Value::as_object) {
            for (name, spec) in map {
                if let Some(entry) = parse_one(source, path, name, spec) {
                    out.push(entry);
                }
            }
        }
    }

    // Claude Code stores per-project servers under `projects.<dir>.mcpServers`.
    if let Some(projects) = root.get("projects").and_then(Value::as_object) {
        for project in projects.values() {
            if let Some(map) = project.get("mcpServers").and_then(Value::as_object) {
                for (name, spec) in map {
                    if let Some(entry) = parse_one(source, path, name, spec) {
                        out.push(entry);
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Parse one `{name: spec}` entry. Returns `None` when the spec has neither a
/// `command` nor a `url` (nothing we can connect to).
fn parse_one(source: Source, path: &Path, name: &str, spec: &Value) -> Option<ServerEntry> {
    let disabled = spec
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        // Some configs use `"enabled": false` instead.
        || spec.get("enabled").and_then(Value::as_bool) == Some(false);

    let env = spec
        .get("env")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_env_string(v)))
                .collect()
        })
        .unwrap_or_default();

    let transport = if let Some(command) = spec.get("command").and_then(Value::as_str) {
        let args = spec
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(value_to_arg_string).collect())
            .unwrap_or_default();
        DiscoveredTransport::Stdio {
            command: command.to_string(),
            args,
        }
    } else {
        // No command: it must carry a url to be connectable — otherwise skip
        // (the `?` returns None).
        DiscoveredTransport::Http {
            url: spec.get("url").and_then(Value::as_str)?.to_string(),
        }
    };

    Some(ServerEntry {
        name: name.to_string(),
        source,
        source_file: path.to_path_buf(),
        transport,
        disabled,
        env,
    })
}

/// Coerce a JSON env value to a string. Strings pass through; other scalars are
/// stringified so a numeric/boolean env value is not silently dropped.
fn value_to_env_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Coerce a JSON arg element to a string, skipping structural values.
fn value_to_arg_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The standard config file locations to scan, each tagged with its [`Source`].
///
/// Locations are resolved from environment variables (`APPDATA`, `HOME`,
/// `USERPROFILE`) and the current directory, so no external crate is needed for
/// home-directory discovery. A location that does not exist is simply skipped by
/// [`discover_from`].
///
/// The exact set is OS-aware for Claude Desktop (whose config path differs on
/// Windows / macOS / Linux) and otherwise home-relative.
pub fn standard_locations() -> Vec<(Source, PathBuf)> {
    let mut locs = Vec::new();

    // Claude Desktop — OS-specific.
    if let Some(p) = claude_desktop_config_path() {
        locs.push((Source::ClaudeDesktop, p));
    }

    let home = home_dir();
    if let Some(home) = &home {
        // Claude Code user config.
        locs.push((Source::ClaudeCode, home.join(".claude.json")));
        // Cursor.
        locs.push((Source::Cursor, home.join(".cursor").join("mcp.json")));
    }

    // Project-scoped files, walking up from the current directory.
    if let Ok(cwd) = std::env::current_dir() {
        for dir in ancestors(&cwd, PROJECT_SEARCH_DEPTH) {
            locs.push((Source::ProjectMcp, dir.join(".mcp.json")));
            locs.push((Source::VsCode, dir.join(".vscode").join("mcp.json")));
        }
    }

    locs
}

/// The first `depth` ancestor directories of `start`, `start` included.
fn ancestors(start: &Path, depth: usize) -> Vec<PathBuf> {
    start
        .ancestors()
        .take(depth)
        .map(Path::to_path_buf)
        .collect()
}

/// The Claude Desktop config path for the current OS, if the needed environment
/// variable is set.
fn claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata)
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        home_dir().map(|h| {
            h.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
}

/// The user's home directory, from `USERPROFILE` (Windows) or `HOME` (Unix),
/// preferring whichever is set. No external crate required.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

/// Scan the given `(source, path)` locations, reading and parsing each that
/// exists, and merge the results into a single [`Discovery`].
///
/// Deduplication: an identical `(source, name, transport)` seen twice (e.g. the
/// same project file reached via two `.` ancestors) is collapsed to one entry.
/// A malformed file becomes a warning, never an abort.
///
/// This is the seam the tests drive: point it at a temp directory's files and
/// assert on the merged result, no real home directory involved.
pub fn discover_from(locations: &[(Source, PathBuf)]) -> Discovery {
    let mut discovery = Discovery::default();

    for (source, path) in locations {
        if !path.exists() {
            continue;
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                discovery
                    .warnings
                    .push(format!("could not read {}: {e}", path.display()));
                continue;
            }
        };
        match parse_config_contents(*source, path, &contents) {
            Ok(entries) => {
                for entry in entries {
                    if !discovery.entries.iter().any(|e| {
                        e.source == entry.source
                            && e.name == entry.name
                            && e.transport == entry.transport
                    }) {
                        discovery.entries.push(entry);
                    }
                }
            }
            Err(warn) => discovery.warnings.push(warn),
        }
    }

    // Stable order: by source slug, then by name (case-insensitive).
    discovery.entries.sort_by(|a, b| {
        a.source.slug().cmp(b.source.slug()).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });

    discovery
}

/// Discover every configured MCP server on this machine by scanning the
/// [`standard_locations`].
pub fn discover() -> Discovery {
    discover_from(&standard_locations())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parses_stdio_entry_with_args_and_redacts_env() {
        let json = r#"{
            "mcpServers": {
                "gh": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "ghp_supersecret", "DEBUG": "1" }
                }
            }
        }"#;
        let entries = parse_config_contents(
            Source::ClaudeDesktop,
            &p("claude_desktop_config.json"),
            json,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "gh");
        assert!(matches!(
            &e.transport,
            DiscoveredTransport::Stdio { command, args }
                if command == "npx" && args.len() == 2
        ));
        // The value is kept internally for spawning...
        assert!(e
            .env
            .iter()
            .any(|(k, v)| k == "GITHUB_TOKEN" && v == "ghp_supersecret"));
        // ...but the display form redacts it and reveals only the key name.
        let disp = e.env_display();
        assert!(disp
            .iter()
            .any(|(k, v)| k == "GITHUB_TOKEN" && *v == REDACTED));
        // The secret must never appear in the JSON projection.
        let json_out = e.to_json().to_string();
        assert!(
            !json_out.contains("ghp_supersecret"),
            "secret leaked: {json_out}"
        );
        assert!(json_out.contains("GITHUB_TOKEN"));
        assert!(json_out.contains(REDACTED));
    }

    #[test]
    fn accepts_servers_key_and_http_url() {
        // VS Code uses `servers` and may declare a remote url.
        let json = r#"{ "servers": { "remote": { "url": "https://example.com/mcp" } } }"#;
        let entries = parse_config_contents(Source::VsCode, &p(".vscode/mcp.json"), json).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0].transport,
            DiscoveredTransport::Http { url } if url == "https://example.com/mcp"
        ));
    }

    #[test]
    fn disabled_flag_variants_are_honored() {
        let json = r#"{
            "mcpServers": {
                "off1": { "command": "x", "disabled": true },
                "off2": { "command": "y", "enabled": false },
                "on":   { "command": "z" }
            }
        }"#;
        let entries = parse_config_contents(Source::ProjectMcp, &p(".mcp.json"), json).unwrap();
        let by = |n: &str| entries.iter().find(|e| e.name == n).unwrap();
        assert!(by("off1").disabled);
        assert!(by("off2").disabled);
        assert!(!by("on").disabled);
    }

    #[test]
    fn harvests_claude_code_top_level_and_per_project() {
        let json = r#"{
            "mcpServers": { "top": { "command": "a" } },
            "projects": {
                "/home/x/proj": { "mcpServers": { "proj-server": { "command": "b" } } }
            },
            "someOtherHugeField": { "ignored": true }
        }"#;
        let entries = parse_config_contents(Source::ClaudeCode, &p(".claude.json"), json).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"top"));
        assert!(names.contains(&"proj-server"));
    }

    #[test]
    fn malformed_json_is_a_warning_not_a_panic() {
        let err =
            parse_config_contents(Source::Cursor, &p("mcp.json"), "{ not json ]").unwrap_err();
        assert!(err.contains("malformed JSON"), "got: {err}");
    }

    #[test]
    fn entry_without_command_or_url_is_skipped() {
        let json = r#"{ "mcpServers": { "weird": { "note": "no transport" } } }"#;
        let entries = parse_config_contents(Source::ProjectMcp, &p(".mcp.json"), json).unwrap();
        assert!(entries.is_empty());
    }

    fn discovery_with(entries: Vec<ServerEntry>) -> Discovery {
        Discovery {
            entries,
            warnings: vec![],
        }
    }

    fn stdio_entry(name: &str, source: Source) -> ServerEntry {
        ServerEntry {
            name: name.to_string(),
            source,
            source_file: p("cfg.json"),
            transport: DiscoveredTransport::Stdio {
                command: "x".to_string(),
                args: vec![],
            },
            disabled: false,
            env: vec![],
        }
    }

    #[test]
    fn resolve_bare_name_unique() {
        let d = discovery_with(vec![stdio_entry("solo", Source::ProjectMcp)]);
        assert_eq!(d.resolve("solo").unwrap().name, "solo");
        // Case-insensitive.
        assert_eq!(d.resolve("SOLO").unwrap().name, "solo");
    }

    #[test]
    fn resolve_ambiguous_across_sources_errors() {
        let d = discovery_with(vec![
            stdio_entry("dup", Source::ProjectMcp),
            stdio_entry("dup", Source::Cursor),
        ]);
        match d.resolve("dup") {
            Err(ResolveError::Ambiguous { candidates, .. }) => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // A source: prefix disambiguates.
        assert_eq!(d.resolve("cursor:dup").unwrap().source, Source::Cursor);
        assert_eq!(d.resolve("project:dup").unwrap().source, Source::ProjectMcp);
    }

    #[test]
    fn resolve_not_found_lists_candidates() {
        let d = discovery_with(vec![stdio_entry("a", Source::ProjectMcp)]);
        match d.resolve("missing") {
            Err(ResolveError::NotFound { available, .. }) => {
                assert_eq!(available, vec!["project:a".to_string()]);
            }
            other => panic!("expected not found, got {other:?}"),
        }
    }

    #[test]
    fn resolve_unknown_source_prefix_errors() {
        let d = discovery_with(vec![stdio_entry("a", Source::ProjectMcp)]);
        match d.resolve("bogus:a") {
            Err(ResolveError::UnknownSource { source }) => assert_eq!(source, "bogus"),
            other => panic!("expected unknown source, got {other:?}"),
        }
    }

    #[test]
    fn discover_from_reads_parses_and_sorts_real_files() {
        // A unique temp dir for this test run.
        let dir = std::env::temp_dir().join(format!("jig-disco-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let proj = dir.join(".mcp.json");
        std::fs::write(
            &proj,
            r#"{ "mcpServers": { "zeta": { "command": "z" }, "alpha": { "command": "a" } } }"#,
        )
        .unwrap();
        let cursor = dir.join("cursor-mcp.json");
        std::fs::write(
            &cursor,
            r#"{ "mcpServers": { "mid": { "command": "m" } } }"#,
        )
        .unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let absent = dir.join("does-not-exist.json");

        let locs = vec![
            (Source::ProjectMcp, proj.clone()),
            // The same project file twice must dedup, not double.
            (Source::ProjectMcp, proj),
            (Source::Cursor, cursor),
            (Source::ClaudeDesktop, bad),
            (Source::VsCode, absent),
        ];
        let d = discover_from(&locs);

        // Three unique servers (alpha, zeta from project; mid from cursor).
        assert_eq!(d.entries.len(), 3, "entries: {:?}", d.entries);
        // Sorted by source slug then name: claude-desktop(none) < cursor:mid <
        // project:alpha < project:zeta.
        let ordered: Vec<(&str, &str)> = d
            .entries
            .iter()
            .map(|e| (e.source.slug(), e.name.as_str()))
            .collect();
        assert_eq!(
            ordered,
            vec![("cursor", "mid"), ("project", "alpha"), ("project", "zeta")]
        );
        // The malformed file produced a warning; the absent file did not.
        assert_eq!(d.warnings.len(), 1, "warnings: {:?}", d.warnings);
        assert!(d.warnings[0].contains("malformed JSON"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
