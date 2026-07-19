//! `jig` — the command-line workbench for MCP servers.
//!
//! Subcommands:
//! * `jig inspect --stdio "<cmd>"` — connect, handshake, and list everything.
//! * `jig call --stdio "<cmd>" --tool <name> --args '<json>'` — invoke a tool.
//! * `jig budget --stdio "<cmd>" [--model <id>...]` — price the tool surface in
//!   context tokens, per tool and per model (see [`budget`]).
//!
//! All support `--json` for machine output and `--tap <file>` to dump the raw
//! protocol traffic as JSONL. Every stdout write goes through [`emit`], which
//! makes `jig … | head` exit cleanly (0) instead of panicking on a broken pipe.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use jig_core::{Client, ClientOptions, ProtocolTap, ToolCallResult};
use serde_json::{json, Value};

mod budget;
mod render;

/// Write `s` to stdout, flushing, in a broken-pipe-safe way.
///
/// A downstream reader that closes early (`jig inspect | head`) makes writes
/// fail with `BrokenPipe`. The `print!`/`println!` macros *panic* on that,
/// which surfaces as an ugly `exit 101`. A CLI should instead exit cleanly and
/// quietly: we treat `BrokenPipe` as a normal end-of-output and exit 0.
pub(crate) fn emit(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("jig: error writing to stdout: {e}");
            std::process::exit(1);
        }
    }
}

/// Convenience: [`emit`] a string followed by a newline.
pub(crate) fn emit_line(s: &str) {
    emit(s);
    emit("\n");
}

/// Default per-request timeout in seconds, mirrored from
/// [`jig_core::DEFAULT_REQUEST_TIMEOUT`] so it can be a clap default.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Translate the CLI `--timeout <seconds>` value into [`ClientOptions`].
/// `0` disables the timeout (wait forever).
pub(crate) fn client_options(timeout_secs: u64) -> ClientOptions {
    ClientOptions {
        request_timeout: (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs)),
    }
}

/// Exit code used when a tool call succeeds at the protocol level but the tool
/// reports `isError: true`.
const EXIT_TOOL_ERROR: u8 = 2;
/// Exit code used for any Jig-level failure (transport/protocol/server/usage).
const EXIT_FAILURE: u8 = 1;

#[derive(Parser)]
#[command(
    name = "jig",
    version,
    about = "A testing workbench for MCP servers.",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to an MCP server, handshake, and report what it exposes.
    Inspect {
        /// The server command to run over stdio, e.g. "my-server --flag".
        #[arg(long, value_name = "COMMAND")]
        stdio: String,
        /// Emit full, untruncated machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,
        /// Write the raw protocol traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Estimate the context-token cost of a server's tool surface, per model.
    Budget {
        /// The server command to run over stdio, e.g. "npx -y my-server".
        #[arg(long, value_name = "COMMAND")]
        stdio: String,
        /// Model id to price against; repeat for one column per model.
        /// Known: gpt-4o, gpt-4, claude-sonnet, claude-opus.
        #[arg(long = "model", value_name = "ID")]
        models: Vec<String>,
        /// Emit full machine-readable JSON (incl. exactness + canonical rendering).
        #[arg(long)]
        json: bool,
        /// Emit a shareable GitHub-flavored markdown card.
        #[arg(long)]
        markdown: bool,
        /// Write the raw protocol traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// For Anthropic models: call the official count_tokens endpoint for an
        /// exact total (requires ANTHROPIC_API_KEY; degrades to the labelled
        /// approximation on any error).
        #[arg(long)]
        exact_anthropic: bool,
    },
    /// Invoke a single tool and print its result.
    Call {
        /// The server command to run over stdio.
        #[arg(long, value_name = "COMMAND")]
        stdio: String,
        /// Name of the tool to call.
        #[arg(long, value_name = "NAME")]
        tool: String,
        /// Tool arguments as a JSON object string (default: {}).
        #[arg(long, value_name = "JSON")]
        args: Option<String>,
        /// Emit the full result as machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Write the raw protocol traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jig: error: {e}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Inspect {
            stdio,
            json,
            tap,
            timeout,
        } => inspect(&stdio, json, tap.as_deref(), timeout).await,
        Command::Budget {
            stdio,
            models,
            json,
            markdown,
            tap,
            timeout,
            exact_anthropic,
        } => {
            budget::run(
                &stdio,
                models,
                json,
                markdown,
                tap.as_deref(),
                timeout,
                exact_anthropic,
            )
            .await
        }
        Command::Call {
            stdio,
            tool,
            args,
            json,
            tap,
            timeout,
        } => {
            call(
                &stdio,
                &tool,
                args.as_deref(),
                json,
                tap.as_deref(),
                timeout,
            )
            .await
        }
    }
}

/// Run `jig inspect`.
async fn inspect(
    stdio: &str,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    let (program, args) = split_command(stdio)?;

    // Own the tap so we can flush it even if a later step fails.
    let tap = ProtocolTap::new();
    let result = inspect_inner(&program, &args, tap.clone(), as_json, timeout_secs).await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

async fn inspect_inner(
    program: &str,
    args: &[String],
    tap: ProtocolTap,
    as_json: bool,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    let client = Client::connect_with_options(program, args, tap, client_options(timeout_secs))
        .await
        .map_err(|e| format!("failed to connect: {e}"))?;

    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let resources = client.list_resources().await.map_err(|e| e.to_string())?;
    let prompts = client.list_prompts().await.map_err(|e| e.to_string())?;

    if as_json {
        let doc = json!({
            "serverInfo": client.server_info(),
            "protocolVersion": client.protocol_version(),
            "capabilities": client.capabilities(),
            "instructions": client.instructions(),
            "tools": tools,
            "resources": resources,
            "prompts": prompts,
        });
        emit_line(&serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?);
    } else {
        emit(&render::inspect_report(
            &client, &tools, &resources, &prompts,
        ));
    }

    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Run `jig call`.
async fn call(
    stdio: &str,
    tool: &str,
    args_json: Option<&str>,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    let (program, args) = split_command(stdio)?;

    let arguments: Value = match args_json {
        Some(s) => serde_json::from_str(s).map_err(|e| format!("--args is not valid JSON: {e}"))?,
        None => json!({}),
    };

    let tap = ProtocolTap::new();
    let result = call_inner(
        &program,
        &args,
        tap.clone(),
        tool,
        arguments,
        as_json,
        timeout_secs,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

#[allow(clippy::too_many_arguments)]
async fn call_inner(
    program: &str,
    args: &[String],
    tap: ProtocolTap,
    tool: &str,
    arguments: Value,
    as_json: bool,
    timeout_secs: u64,
) -> Result<ExitCode, String> {
    let client = Client::connect_with_options(program, args, tap, client_options(timeout_secs))
        .await
        .map_err(|e| format!("failed to connect: {e}"))?;

    let result: ToolCallResult = client
        .call_tool(tool, arguments)
        .await
        .map_err(|e| format!("tool call failed: {e}"))?;

    if as_json {
        emit_line(&serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    } else {
        emit(&render::call_result(tool, &result));
    }

    client.shutdown().await.map_err(|e| e.to_string())?;

    if result.is_error {
        Ok(ExitCode::from(EXIT_TOOL_ERROR))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Warn (on stderr) if the server wrote anything to stdout that is not a valid
/// JSON-RPC message. This is the single most common way an MCP server breaks in
/// practice — a stray `console.log`, a startup banner, or a logger misconfigured
/// to stdout — and it silently corrupts the newline-delimited framing. Jig is a
/// diagnostic tool, so it must name the problem loudly instead of hiding it.
const MAX_POLLUTION_LINES_SHOWN: usize = 5;

pub(crate) fn warn_non_protocol_output(tap: &ProtocolTap) {
    let bad = tap.non_protocol_inbound();
    if bad.is_empty() {
        return;
    }
    eprintln!(
        "jig: warning: server wrote {} non-protocol line(s) to stdout — this breaks MCP framing \
         (MCP stdio requires stdout to carry *only* newline-delimited JSON-RPC; send logs to stderr)",
        bad.len()
    );
    for (seq, raw) in bad.iter().take(MAX_POLLUTION_LINES_SHOWN) {
        let preview: String = raw.chars().take(120).collect();
        eprintln!("jig:   [tap seq {seq}] {preview}");
    }
    if bad.len() > MAX_POLLUTION_LINES_SHOWN {
        eprintln!(
            "jig:   ... and {} more (see the tap for all of them)",
            bad.len() - MAX_POLLUTION_LINES_SHOWN
        );
    }
}

/// Write the tap to `path` if one was requested, reporting any failure to
/// stderr without changing the command's success/failure outcome.
pub(crate) fn write_tap_if_requested(tap: &ProtocolTap, path: Option<&std::path::Path>) {
    if let Some(path) = path {
        match tap.write_jsonl(path) {
            Ok(()) => eprintln!("jig: wrote {} tap entries to {}", tap.len(), path.display()),
            Err(e) => eprintln!(
                "jig: warning: failed to write tap to {}: {e}",
                path.display()
            ),
        }
    }
}

/// Split a single `--stdio` command string into program + args.
///
/// Supports double-quoted segments so paths containing spaces survive
/// (e.g. `"C:\\Program Files\\srv.exe" --flag`). This is a small, purpose-built
/// splitter rather than a full shell parser.
pub(crate) fn split_command(input: &str) -> Result<(String, Vec<String>), String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if in_quotes {
        return Err("unbalanced quotes in --stdio command".to_string());
    }
    if has_token {
        tokens.push(current);
    }

    let mut it = tokens.into_iter();
    let program = it
        .next()
        .ok_or_else(|| "--stdio command was empty".to_string())?;
    Ok((program, it.collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_plain_command() {
        let (p, a) = split_command("server --flag value").unwrap();
        assert_eq!(p, "server");
        assert_eq!(a, vec!["--flag", "value"]);
    }

    #[test]
    fn split_quoted_path_with_spaces() {
        let (p, a) = split_command("\"C:\\Program Files\\srv.exe\" --x 1").unwrap();
        assert_eq!(p, "C:\\Program Files\\srv.exe");
        assert_eq!(a, vec!["--x", "1"]);
    }

    #[test]
    fn split_empty_is_error() {
        assert!(split_command("   ").is_err());
    }

    #[test]
    fn split_unbalanced_quote_is_error() {
        assert!(split_command("\"oops").is_err());
    }
}
