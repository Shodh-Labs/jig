//! `jig` — the command-line workbench for MCP servers.
//!
//! Subcommands:
//! * `jig inspect --stdio "<cmd>"` — connect, handshake, and list everything.
//! * `jig call --stdio "<cmd>" --tool <name> --args '<json>'` — invoke a tool.
//! * `jig budget --stdio "<cmd>" [--model <id>...]` — price the tool surface in
//!   context tokens, per tool and per model (see [`budget`]).
//! * `jig bench --stdio "<cmd>" --task "<text>" [--model <id>...]` — put a real
//!   model in the loop and measure which tool it selects across runs
//!   (see [`mod@bench`]).
//!
//! All support `--json` for machine output and `--tap <file>` to dump the raw
//! protocol traffic as JSONL. Every stdout write goes through [`emit`], which
//! makes `jig … | head` exit cleanly (0) instead of panicking on a broken pipe.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use jig_cli::{parse_headers, split_command};
use jig_core::{Client, ClientOptions, ProtocolTap, ToolCallResult};
use serde_json::{json, Value};

mod bench;
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

/// Default inbound message size cap in bytes, mirrored from
/// [`jig_core::DEFAULT_MAX_MESSAGE_BYTES`] so it can be a clap default.
const DEFAULT_MAX_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Translate the CLI `--timeout <seconds>` and `--max-message-bytes <n>` values
/// into [`ClientOptions`]. A timeout of `0` disables the timeout (wait forever);
/// a cap of `0` disables the inbound size cap (unbounded buffering).
pub(crate) fn client_options(timeout_secs: u64, max_message_bytes: u64) -> ClientOptions {
    ClientOptions {
        request_timeout: (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs)),
        max_message_bytes: (max_message_bytes != 0).then_some(max_message_bytes as usize),
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
        /// Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to connect to over Streamable HTTP, e.g.
        /// `https://example.com/mcp`. Mutually exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable). Typically
        /// `Authorization: Bearer <token>` for remote servers.
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
        /// Emit full, untruncated machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,
        /// Write the raw protocol traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// Maximum size in bytes of a single inbound message (0 = no cap).
        /// A larger message fails with a clear size error instead of being
        /// buffered without limit.
        #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_MESSAGE_BYTES)]
        max_message_bytes: u64,
    },
    /// Estimate the context-token cost of a server's tool surface, per model.
    Budget {
        /// The server command to run over stdio, e.g. "npx -y my-server".
        /// Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to price over Streamable HTTP. Mutually
        /// exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
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
        /// Maximum size in bytes of a single inbound message (0 = no cap).
        /// A larger message fails with a clear size error instead of being
        /// buffered without limit.
        #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_MESSAGE_BYTES)]
        max_message_bytes: u64,
        /// For Anthropic models: call the official count_tokens endpoint for an
        /// exact total (requires ANTHROPIC_API_KEY; degrades to the labelled
        /// approximation on any error).
        #[arg(long)]
        exact_anthropic: bool,
    },
    /// Bench a live model against the server's tools: which tool does a real
    /// model pick for a task, with what args, across repeated runs?
    Bench {
        /// The server command to run over stdio. Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to connect to over Streamable HTTP.
        /// Mutually exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
        /// The natural-language task to give the model (required).
        #[arg(long, value_name = "TEXT")]
        task: String,
        /// Model id to bench against; repeat for one section per model.
        /// Known: gpt-4o, gpt-4, claude-sonnet, claude-opus. Default:
        /// claude-sonnet if ANTHROPIC_API_KEY is set, else gpt-4o.
        #[arg(long = "model", value_name = "ID")]
        models: Vec<String>,
        /// Override the concrete API model string sent on the wire (hardcoded
        /// mappings age). Applies only with a single --model.
        #[arg(long, value_name = "STRING")]
        api_model: Option<String>,
        /// Number of times to send the request (default 3).
        #[arg(long, value_name = "N", default_value_t = 3)]
        runs: usize,
        /// Sampling temperature, always recorded (default 1.0).
        #[arg(long, value_name = "T", default_value_t = 1.0)]
        temp: f64,
        /// Emit full machine-readable JSON (incl. the rendered request, minus
        /// auth, and every raw provider response).
        #[arg(long)]
        json: bool,
        /// Write the raw protocol traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// Maximum size in bytes of a single inbound message (0 = no cap).
        #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_MESSAGE_BYTES)]
        max_message_bytes: u64,
    },
    /// Invoke a single tool and print its result.
    Call {
        /// The server command to run over stdio. Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to connect to over Streamable HTTP.
        /// Mutually exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
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
        /// Maximum size in bytes of a single inbound message (0 = no cap).
        /// A larger message fails with a clear size error instead of being
        /// buffered without limit.
        #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_MESSAGE_BYTES)]
        max_message_bytes: u64,
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
            http,
            header,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, header)?;
            inspect(&target, json, tap.as_deref(), timeout, max_message_bytes).await
        }
        Command::Budget {
            stdio,
            http,
            header,
            models,
            json,
            markdown,
            tap,
            timeout,
            max_message_bytes,
            exact_anthropic,
        } => {
            let target = Target::resolve(stdio, http, header)?;
            budget::run(
                &target,
                models,
                json,
                markdown,
                tap.as_deref(),
                timeout,
                max_message_bytes,
                exact_anthropic,
            )
            .await
        }
        Command::Bench {
            stdio,
            http,
            header,
            task,
            models,
            api_model,
            runs,
            temp,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, header)?;
            bench::run(
                &target,
                models,
                api_model,
                task,
                runs,
                temp,
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Call {
            stdio,
            http,
            header,
            tool,
            args,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, header)?;
            call(
                &target,
                &tool,
                args.as_deref(),
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
    }
}

/// A resolved connection target: either a stdio subprocess or a remote HTTP
/// endpoint. Built once from the CLI flags, then shared by every command so the
/// stdio/HTTP branch lives in exactly one place.
pub(crate) enum Target {
    /// A local server launched over stdio.
    Stdio {
        /// The resolved program name.
        program: String,
        /// Its arguments.
        args: Vec<String>,
    },
    /// A remote server reached over Streamable HTTP.
    Http {
        /// The MCP endpoint URL.
        url: String,
        /// Extra headers (auth, etc.) sent on every request.
        headers: Vec<(String, String)>,
    },
}

impl Target {
    /// Resolve the mutually-exclusive `--stdio` / `--http` flags (plus
    /// `--header`) into a single target, rejecting nonsensical combinations
    /// with a clear message. clap already enforces the `conflicts_with`, so the
    /// both-present case is defensive.
    pub(crate) fn resolve(
        stdio: Option<String>,
        http: Option<String>,
        header: Vec<String>,
    ) -> Result<Target, String> {
        match (stdio, http) {
            (Some(_), Some(_)) => Err("--stdio and --http are mutually exclusive".to_string()),
            (None, None) => Err("one of --stdio <command> or --http <url> is required".to_string()),
            (Some(cmd), None) => {
                if !header.is_empty() {
                    return Err(
                        "--header applies only to --http (stdio servers take no HTTP headers)"
                            .to_string(),
                    );
                }
                let (program, args) = split_command(&cmd)?;
                Ok(Target::Stdio { program, args })
            }
            (None, Some(url)) => Ok(Target::Http {
                url,
                headers: parse_headers(&header)?,
            }),
        }
    }

    /// Connect and complete the MCP handshake against this target, recording
    /// into `tap`.
    pub(crate) async fn connect(
        &self,
        tap: ProtocolTap,
        timeout_secs: u64,
        max_message_bytes: u64,
    ) -> Result<Client, String> {
        let opts = client_options(timeout_secs, max_message_bytes);
        let client = match self {
            Target::Stdio { program, args } => {
                Client::connect_with_options(program, args, tap, opts).await
            }
            Target::Http { url, headers } => {
                Client::connect_http_with_options(url, headers.clone(), tap, opts).await
            }
        };
        client.map_err(|e| format!("failed to connect: {e}"))
    }
}

/// Run `jig inspect`.
async fn inspect(
    target: &Target,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    // Own the tap so we can flush it even if a later step fails.
    let tap = ProtocolTap::new();
    let result = inspect_inner(
        target,
        tap.clone(),
        as_json,
        timeout_secs,
        max_message_bytes,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

async fn inspect_inner(
    target: &Target,
    tap: ProtocolTap,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;

    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let resources = client.list_resources().await.map_err(|e| e.to_string())?;
    let prompts = client.list_prompts().await.map_err(|e| e.to_string())?;

    if as_json {
        let doc = render::inspect_json_doc(
            client.server_info(),
            client.protocol_version(),
            client.capabilities(),
            client.instructions(),
            &tools,
            &resources,
            &prompts,
        );
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
#[allow(clippy::too_many_arguments)]
async fn call(
    target: &Target,
    tool: &str,
    args_json: Option<&str>,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let arguments: Value = match args_json {
        Some(s) => serde_json::from_str(s).map_err(|e| format!("--args is not valid JSON: {e}"))?,
        None => json!({}),
    };

    let tap = ProtocolTap::new();
    let result = call_inner(
        target,
        tap.clone(),
        tool,
        arguments,
        as_json,
        timeout_secs,
        max_message_bytes,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

#[allow(clippy::too_many_arguments)]
async fn call_inner(
    target: &Target,
    tap: ProtocolTap,
    tool: &str,
    arguments: Value,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;

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
