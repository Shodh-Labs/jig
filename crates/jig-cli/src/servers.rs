//! `jig servers` — discover the MCP servers already configured on this machine.
//!
//! Reads the config files the popular MCP clients write (Claude Desktop, Claude
//! Code, Cursor, VS Code, project `.mcp.json`), merges them, labels each entry
//! by its source, and prints a table or `--json`. Environment-variable *values*
//! are always redacted; only key names are shown.

use std::process::ExitCode;

use jig_core::discovery::{self, Discovery, ServerEntry};
use serde_json::json;

use crate::{emit, emit_line};

/// Run `jig servers`.
pub fn run(as_json: bool) -> Result<ExitCode, String> {
    let discovered = discovery::discover();
    // Warnings (malformed/unreadable config files) go to stderr so they never
    // pollute the table or the JSON on stdout.
    for w in &discovered.warnings {
        eprintln!("jig: warning: {w}");
    }
    if as_json {
        emit_line(&render_json(&discovered));
    } else {
        emit(&render_table(&discovered));
    }
    Ok(ExitCode::SUCCESS)
}

/// Render the machine-readable JSON document (env values redacted).
pub fn render_json(d: &Discovery) -> String {
    let servers: Vec<_> = d.entries.iter().map(ServerEntry::to_json).collect();
    let doc = json!({
        "servers": servers,
        "warnings": d.warnings,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// Render the human table. The env column shows redacted key names only.
pub fn render_table(d: &Discovery) -> String {
    if d.entries.is_empty() {
        return "No configured MCP servers found on this machine.\n\
                Looked in Claude Desktop, Claude Code (~/.claude.json), Cursor, VS Code, \
                and project .mcp.json files.\n"
            .to_string();
    }

    let headers = ["NAME", "SOURCE", "TRANSPORT", "ENV", "DISABLED"];
    let mut rows: Vec<[String; 5]> = Vec::new();
    for e in &d.entries {
        let env = if e.env.is_empty() {
            "-".to_string()
        } else {
            // Redacted: key names only, each shown as KEY=•••.
            e.env_display()
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        rows.push([
            e.name.clone(),
            e.source.slug().to_string(),
            e.transport_summary(),
            env,
            if e.disabled { "yes" } else { "-" }.to_string(),
        ]);
    }

    // Column widths.
    let mut widths = [0usize; 5];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }

    let fmt_row = |row: &[String; 5]| -> String {
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{:<width$}", c, width = widths[i]));
        }
        // Trim trailing padding on the last column for tidy snapshots.
        format!("{}\n", line.trim_end())
    };

    let mut s = String::new();
    let header_row: [String; 5] = headers.map(String::from);
    s.push_str(&fmt_row(&header_row));
    let total: usize = widths.iter().sum::<usize>() + 2 * (widths.len() - 1);
    s.push_str(&"-".repeat(total));
    s.push('\n');
    for row in &rows {
        s.push_str(&fmt_row(row));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::discovery::{DiscoveredTransport, Source};
    use std::path::PathBuf;

    fn sample() -> Discovery {
        Discovery {
            entries: vec![
                ServerEntry {
                    name: "github".to_string(),
                    source: Source::ClaudeDesktop,
                    source_file: PathBuf::from("claude_desktop_config.json"),
                    transport: DiscoveredTransport::Stdio {
                        command: "npx".to_string(),
                        args: vec![
                            "-y".to_string(),
                            "@modelcontextprotocol/server-github".to_string(),
                        ],
                    },
                    disabled: false,
                    env: vec![("GITHUB_TOKEN".to_string(), "ghp_secret_value".to_string())],
                },
                ServerEntry {
                    name: "remote-api".to_string(),
                    source: Source::VsCode,
                    source_file: PathBuf::from(".vscode/mcp.json"),
                    transport: DiscoveredTransport::Http {
                        url: "https://example.com/mcp".to_string(),
                    },
                    disabled: true,
                    env: vec![],
                },
            ],
            warnings: vec![],
        }
    }

    #[test]
    fn table_redacts_env_values() {
        let out = render_table(&sample());
        // The key name is shown, the secret value never is.
        assert!(out.contains("GITHUB_TOKEN"), "got:\n{out}");
        assert!(!out.contains("ghp_secret_value"), "secret leaked:\n{out}");
        assert!(
            out.contains("\u{2022}\u{2022}\u{2022}"),
            "redaction missing:\n{out}"
        );
    }

    #[test]
    fn json_redacts_env_values() {
        let out = render_json(&sample());
        assert!(!out.contains("ghp_secret_value"), "secret leaked:\n{out}");
        assert!(out.contains("GITHUB_TOKEN"));
    }

    #[test]
    fn empty_discovery_has_friendly_message() {
        let out = render_table(&Discovery::default());
        assert!(out.contains("No configured MCP servers"));
    }

    #[test]
    fn servers_table_snapshot() {
        insta::assert_snapshot!("servers_table", render_table(&sample()));
    }

    #[test]
    fn servers_json_snapshot() {
        insta::assert_snapshot!("servers_json", render_json(&sample()));
    }
}
