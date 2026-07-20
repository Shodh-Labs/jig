//! Connection targets and the live session the workbench holds open.
//!
//! The app speaks MCP only through `jig-core`'s [`Client`] — the webview never
//! touches the protocol, a socket, or a subprocess. Everything here is about
//! turning what the user picked in the Connect pane into the exact `jig-core`
//! call the CLI would have made.

use jig_core::discovery::{DiscoveredTransport, Discovery};
use jig_core::{Client, ClientOptions, ProtocolTap};
use serde::Deserialize;
use std::time::Duration;

/// The default request timeout, in seconds — the same 30s `jig-core` uses.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The default inbound message cap, in bytes: 64 MiB, as `jig-core` ships.
pub const DEFAULT_MAX_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// What the user asked to connect to.
///
/// `Discovered` is resolved against a fresh [`Discovery`] scan at connect time
/// rather than carrying a command through the webview — which also means the
/// environment variables in a discovered config (real API keys) are read in
/// Rust and never cross the IPC boundary.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    /// A server discovered in a Claude Desktop / Cursor / VS Code config.
    #[serde(rename_all = "camelCase")]
    Discovered { name: String },
    /// A manually entered stdio command.
    #[serde(rename_all = "camelCase")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A manually entered Streamable HTTP endpoint.
    #[serde(rename_all = "camelCase")]
    Http { url: String },
}

/// Tunables the Connect pane exposes under "Advanced".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOptions {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_bytes")]
    pub max_message_bytes: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}
fn default_max_bytes() -> u64 {
    DEFAULT_MAX_MESSAGE_BYTES
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

impl ConnectOptions {
    /// Translate to `jig-core`'s options. A zero in either field means "no
    /// limit", matching the CLI's flag semantics.
    pub fn to_client_options(&self) -> ClientOptions {
        ClientOptions {
            request_timeout: (self.timeout_secs > 0)
                .then(|| Duration::from_secs(self.timeout_secs)),
            max_message_bytes: (self.max_message_bytes > 0)
                .then_some(self.max_message_bytes as usize),
            listen: false,
        }
    }
}

/// A human-readable label for the transport, matching the CLI's report meta.
pub fn transport_label(target: &Target) -> &'static str {
    match target {
        Target::Http { .. } => "http",
        Target::Discovered { .. } | Target::Stdio { .. } => "stdio",
    }
}

/// Split a manually typed command line into a program and its arguments.
///
/// Honours double quotes so a Windows path containing spaces
/// (`"C:\Program Files\node\node.exe" server.js`) survives, which is the case
/// that actually bites users on this platform. Deliberately simple: this is a
/// convenience for the manual-entry box, not a shell — nothing is expanded,
/// globbed, or executed through a shell.
pub fn split_command(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    parts.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        parts.push(cur);
    }
    parts
}

/// Connect to `target`, completing the MCP handshake.
///
/// The tap is passed in by the caller and kept by the caller, so a session that
/// fails *during* the handshake still leaves its wire log inspectable — which is
/// exactly when you most want to see it.
pub async fn connect(
    target: &Target,
    tap: ProtocolTap,
    options: &ConnectOptions,
) -> Result<Client, String> {
    let opts = options.to_client_options();
    match target {
        Target::Stdio { command, args } => Client::connect_with_env(command, args, &[], tap, opts)
            .await
            .map_err(|e| crate::dto::error_message(&e)),
        Target::Http { url } => Client::connect_http_with_options(url, Vec::new(), tap, opts)
            .await
            .map_err(|e| crate::dto::error_message(&e)),
        Target::Discovered { name } => {
            let discovery: Discovery = jig_core::discovery::discover();
            let entry = discovery
                .resolve(name)
                // `ResolveError`'s Display already lists the available names or
                // the ambiguous candidates — exactly the actionable message the
                // CLI shows, so it is passed through untouched.
                .map_err(|e| e.to_string())?;

            if entry.disabled {
                return Err(format!(
                    "server '{}' is marked disabled in {} — enable it there, or enter its command manually",
                    entry.name,
                    entry.source_file.display()
                ));
            }

            match &entry.transport {
                DiscoveredTransport::Stdio { command, args } => {
                    Client::connect_with_env(command, args, &entry.env, tap, opts)
                        .await
                        .map_err(|e| crate::dto::error_message(&e))
                }
                DiscoveredTransport::Http { url } => {
                    Client::connect_http_with_options(url, Vec::new(), tap, opts)
                        .await
                        .map_err(|e| crate::dto::error_message(&e))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_command_splits_on_whitespace() {
        assert_eq!(
            split_command("npx -y @modelcontextprotocol/server-everything"),
            vec!["npx", "-y", "@modelcontextprotocol/server-everything"]
        );
    }

    #[test]
    fn a_quoted_windows_path_with_spaces_survives() {
        assert_eq!(
            split_command(r#""C:\Program Files\node\node.exe" server.js"#),
            vec![r"C:\Program Files\node\node.exe", "server.js"]
        );
    }

    #[test]
    fn runs_of_whitespace_do_not_produce_empty_arguments() {
        assert_eq!(
            split_command("  node    server.js  "),
            vec!["node", "server.js"]
        );
        assert!(split_command("   ").is_empty());
        assert!(split_command("").is_empty());
    }

    #[test]
    fn an_empty_quoted_argument_is_preserved() {
        // `node script.js ""` must pass an empty argument through, not drop it.
        assert_eq!(
            split_command(r#"node script.js """#),
            vec!["node", "script.js", ""]
        );
    }

    #[test]
    fn zero_means_no_limit_in_both_advanced_fields() {
        let opts = ConnectOptions {
            timeout_secs: 0,
            max_message_bytes: 0,
        };
        let c = opts.to_client_options();
        assert!(c.request_timeout.is_none());
        assert!(c.max_message_bytes.is_none());
        assert!(
            !c.listen,
            "the app never opts into the standalone GET stream"
        );
    }

    #[test]
    fn defaults_match_jig_cores_own_defaults() {
        let c = ConnectOptions::default().to_client_options();
        assert_eq!(c.request_timeout, Some(Duration::from_secs(30)));
        assert_eq!(c.max_message_bytes, Some(64 * 1024 * 1024));
    }

    #[test]
    fn transport_labels_match_the_report_meta() {
        assert_eq!(
            transport_label(&Target::Http {
                url: "https://x".into()
            }),
            "http"
        );
        assert_eq!(
            transport_label(&Target::Stdio {
                command: "node".into(),
                args: vec![]
            }),
            "stdio"
        );
        assert_eq!(
            transport_label(&Target::Discovered { name: "x".into() }),
            "stdio"
        );
    }

    #[test]
    fn a_target_deserializes_from_the_webviews_tagged_json() {
        let t: Target =
            serde_json::from_str(r#"{"kind":"stdio","command":"node","args":["s.js"]}"#).unwrap();
        match t {
            Target::Stdio { command, args } => {
                assert_eq!(command, "node");
                assert_eq!(args, vec!["s.js"]);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // `args` is optional.
        let t: Target = serde_json::from_str(r#"{"kind":"stdio","command":"node"}"#).unwrap();
        assert!(matches!(t, Target::Stdio { ref args, .. } if args.is_empty()));

        let t: Target =
            serde_json::from_str(r#"{"kind":"discovered","name":"everything"}"#).unwrap();
        assert!(matches!(t, Target::Discovered { ref name } if name == "everything"));
    }
}
