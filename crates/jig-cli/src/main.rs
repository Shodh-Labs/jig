//! `jig` — the command-line workbench for MCP servers.
//!
//! Subcommands:
//! * `jig check --stdio "<cmd>"` — the one-command report card: run everything
//!   Jig knows in one session and render a scored verdict (see [`mod@check`]).
//! * `jig inspect --stdio "<cmd>"` — connect, handshake, and list everything.
//! * `jig call --stdio "<cmd>" --tool <name> --args '<json>'` — invoke a tool.
//! * `jig budget --stdio "<cmd>" [--model <id>...]` — price the tool surface in
//!   context tokens, per tool and per model (see [`budget`]).
//! * `jig context --stdio "<cmd>" [--model <id>]` — render the exact provider
//!   API request body `jig bench` would send, token-annotated. Sends nothing and
//!   needs no key (see [`mod@context`]).
//! * `jig bench --stdio "<cmd>" --task "<text>" [--model <id>...]` — put a real
//!   model in the loop and measure which tool it selects across runs
//!   (see [`mod@bench`]).
//! * `jig eval --stdio "<cmd>" [--suite <path>...]` — run a `.jig` eval suite:
//!   replay `prompt → expected tool` cases and gate on the selection rate across
//!   N runs (see [`mod@eval`]).
//! * `jig servers` — discover the MCP servers already configured on this
//!   machine (Claude Desktop/Code, Cursor, VS Code, project `.mcp.json`), merged
//!   and labelled by source (see [`servers`]).
//! * `jig search <query>` — search the MCP ecosystem (official registry + npm)
//!   for servers (see [`ecosystem`]).
//! * `jig info <name>` — detailed info for one server/package, with an optional
//!   `--probe` that actually launches it and reports the live handshake.
//!
//! The connecting verbs (`inspect`/`budget`/`call`/`bench`) additionally accept
//! `--server <name>` to connect to a server discovered by [`servers`] without
//! retyping its command line.
//!
//! Exit codes are uniform: `0` success, `1` a jig-level failure, `2` reserved
//! for a tool reporting an error (`jig call`), and `3` a `jig eval` gate /
//! `must_pass` failure.
//!
//! All support `--json` for machine output and `--tap <file>` to dump the raw
//! protocol traffic as JSONL. Every stdout write goes through [`emit`], which
//! makes `jig … | head` exit cleanly (0) instead of panicking on a broken pipe.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use jig_cli::{parse_headers, split_command};
use jig_core::discovery::{self, DiscoveredTransport};
use jig_core::{Client, ClientOptions, ProtocolTap, SourceSelector, ToolCallResult};
use serde_json::{json, Value};

mod advisor_view;
mod auth;
mod bench;
mod budget;
mod check;
mod context;
mod ecosystem;
mod eval;
mod render;
mod report;
mod servers;

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

/// Default seconds to hold the standalone GET stream open under `inspect
/// --listen`.
const DEFAULT_LISTEN_SECS: u64 = 10;

/// Translate the CLI `--timeout <seconds>` and `--max-message-bytes <n>` values
/// into [`ClientOptions`]. A timeout of `0` disables the timeout (wait forever);
/// a cap of `0` disables the inbound size cap (unbounded buffering).
pub(crate) fn client_options(timeout_secs: u64, max_message_bytes: u64) -> ClientOptions {
    ClientOptions {
        request_timeout: (timeout_secs != 0).then(|| Duration::from_secs(timeout_secs)),
        max_message_bytes: (max_message_bytes != 0).then_some(max_message_bytes as usize),
        // Listening is opt-in and set by the caller (only `inspect --listen`).
        listen: false,
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
    /// The one-command report card: connect once, score protocol compliance,
    /// context cost, schema hygiene, description quality and robustness, and
    /// render a graded verdict with a ranked to-do list.
    Check {
        /// The server command to run over stdio, e.g. "npx -y my-server".
        /// Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to check over Streamable HTTP. Mutually
        /// exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
        /// Emit the full report card as machine-readable JSON (all findings,
        /// per-dimension scores, weights, rubric version, provenance).
        #[arg(long)]
        json: bool,
        /// Emit shields.io endpoint JSON for the composite score (for a README
        /// badge). Overrides the human report.
        #[arg(long)]
        badge: bool,
        /// Exit nonzero if the composite score is below this floor (a CI gate).
        #[arg(long, value_name = "N")]
        min_score: Option<f64>,
        /// Path to the ecosystem percentiles dataset used to score context cost.
        /// Defaults to the census bundled into the binary; pass `none` to force
        /// absolute bands (no ecosystem comparison).
        #[arg(long, value_name = "FILE")]
        percentiles: Option<PathBuf>,
        /// Write the self-contained HTML report card to this path (overrides the
        /// default `./jig-report-<server>.html`). Enables the report even in
        /// --json/--badge mode.
        #[arg(long, value_name = "FILE", conflicts_with = "no_report")]
        report: Option<PathBuf>,
        /// Do not write the HTML report card (human mode writes one by default).
        #[arg(long)]
        no_report: bool,
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
    /// Probe and grade a remote server's discoverable OAuth conformance
    /// (RFC 9728 / 8414 / 7591 / 8707 / 9207 / 6750). Performs NO login flow,
    /// opens no browser, and needs no credentials: it sends one unauthenticated
    /// `initialize`, follows the challenge to the metadata, and renders a
    /// conformance table + verdict. HTTP transport only.
    Auth {
        /// A remote MCP endpoint URL to probe over Streamable HTTP, e.g.
        /// `https://example.com/mcp`.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Present only to give a clear "auth is an HTTP concern" error; auth
        /// probing does not support stdio targets.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// Extra HTTP header, "Name: Value" (repeatable). Pass
        /// `Authorization: Bearer <token>` to also test header passthrough — Jig
        /// never fabricates a token.
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
        /// Emit the full conformance report as JSON (every finding + every raw
        /// HTTP exchange, tokens redacted).
        #[arg(long)]
        json: bool,
        /// Write the probe's HTTP traffic to this file as JSONL.
        #[arg(long, value_name = "FILE")]
        tap: Option<PathBuf>,
        /// Per-request timeout in seconds (0 = wait forever).
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
    },
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
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
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
        /// (--http only) After listing, open the standalone server→client GET
        /// SSE stream and report any pushed messages. Off by default — a
        /// diagnostic tool opens that stream only when asked.
        #[arg(long)]
        listen: bool,
        /// (with --listen) How many seconds to keep the GET stream open after
        /// listing before reporting.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_LISTEN_SECS)]
        duration: u64,
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
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
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
        /// After the table, print the tool-set advisor: naming collisions,
        /// accuracy-cliff warnings, and cost-dominance advisories. Suppressed
        /// by --json/--markdown (those are machine/share artifacts).
        #[arg(long)]
        advise: bool,
    },
    /// Render exactly what the model sees: the provider API request body
    /// `jig bench` would send (tools in the provider dialect + system prompt +
    /// a placeholder task), token-annotated. Sends nothing; needs no API key.
    Context {
        /// The server command to run over stdio, e.g. "npx -y my-server".
        /// Mutually exclusive with --http.
        #[arg(long, value_name = "COMMAND", conflicts_with = "http")]
        stdio: Option<String>,
        /// A remote MCP endpoint URL to connect to over Streamable HTTP.
        /// Mutually exclusive with --stdio.
        #[arg(long, value_name = "URL", conflicts_with = "stdio")]
        http: Option<String>,
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
        /// Extra HTTP header for --http, "Name: Value" (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        header: Vec<String>,
        /// Model whose tokenizer + provider dialect to render. Known: gpt-4o,
        /// gpt-4, claude-sonnet, claude-opus. Default: claude-sonnet if
        /// ANTHROPIC_API_KEY is set, else gpt-4o (no key is used either way).
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// Override the concrete API model string placed in the rendered body.
        #[arg(long, value_name = "STRING")]
        api_model: Option<String>,
        /// Force the provider dialect (anthropic|openai), overriding the model's
        /// registry provider. No key is needed to pick a dialect.
        #[arg(long, value_enum, value_name = "PROVIDER")]
        provider: Option<ProviderArg>,
        /// Print the full JSON request body, pretty-printed, exactly as the API
        /// would receive it (minus auth).
        #[arg(long, conflicts_with = "json")]
        raw: bool,
        /// Emit machine output: the raw body + per-section token annotations +
        /// provenance (model, tokenizer, exactness, dialect).
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
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
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
        /// After the run, draft a `.jig` eval case into this file (creating it
        /// and any parent dir). The expected tool + arg matchers come from the
        /// majority run; refused if the majority outcome was not a selection.
        #[arg(long, value_name = "FILE")]
        save_case: Option<PathBuf>,
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
    /// Run a `.jig` eval suite: replay `prompt → expected tool` cases against a
    /// live model and gate on the selection rate across N runs.
    Eval {
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
        /// A `.jig` suite file or a directory of them; repeatable. Default:
        /// `./.jig/` (every `*.yaml` in it).
        #[arg(long = "suite", value_name = "PATH")]
        suites: Vec<PathBuf>,
        /// Model id to eval against. Known: gpt-4o, gpt-4, claude-sonnet,
        /// claude-opus. Default: claude-sonnet if ANTHROPIC_API_KEY is set, else
        /// gpt-4o.
        #[arg(long = "model", value_name = "ID")]
        model: Option<String>,
        /// Override the concrete API model string sent on the wire.
        #[arg(long, value_name = "STRING")]
        api_model: Option<String>,
        /// Force N runs for every case, overriding case/suite defaults.
        #[arg(long, value_name = "N")]
        runs_override: Option<usize>,
        /// Override the sampling temperature for every suite.
        #[arg(long, value_name = "T")]
        temp: Option<f64>,
        /// Gate the whole run: fail (exit 3) if the overall weighted selection
        /// accuracy is below this fraction (0..=1).
        #[arg(long, value_name = "0..1")]
        gate: Option<f64>,
        /// Emit full machine-readable JSON (every case, every run, full detail).
        #[arg(long)]
        json: bool,
        /// Write a CI-native JUnit XML report to this file.
        #[arg(long, value_name = "FILE")]
        junit: Option<PathBuf>,
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
        /// Connect to a server discovered from a local client config by name
        /// (see `jig servers`). Use `source:name` to disambiguate. Mutually
        /// exclusive with --stdio/--http.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["stdio", "http"])]
        server: Option<String>,
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
    /// Read a resource by URI (`resources/read`) and print its contents.
    Read {
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
        /// The URI of the resource to read.
        #[arg(long, value_name = "URI")]
        uri: String,
        /// Emit the full result as machine-readable JSON (blob base64 in full).
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
    /// Fetch a prompt by name (`prompts/get`) and print its messages.
    Prompt {
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
        /// Name of the prompt to fetch.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Prompt arguments as a JSON object string (default: {}).
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
        #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_MESSAGE_BYTES)]
        max_message_bytes: u64,
    },
    /// List the MCP servers already configured on this machine, by source.
    Servers {
        /// Emit machine-readable JSON (env values redacted) instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Search the MCP ecosystem (official registry + npm) for servers.
    Search {
        /// The search query.
        #[arg(value_name = "QUERY")]
        query: String,
        /// Which source(s) to query.
        #[arg(long, value_enum, default_value_t = SearchSource::All)]
        source: SearchSource,
        /// Maximum results to show per source.
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show detailed info for one server/package (registry + npm), optionally
    /// probing it live.
    Info {
        /// The server name or npm package to look up.
        #[arg(value_name = "NAME")]
        name: String,
        /// Actually run it (`npx -y <name>`) over stdio and report the live
        /// handshake: serverInfo, protocol, capabilities, tools, token cost.
        /// Runs third-party code — see the printed notice.
        #[arg(long)]
        probe: bool,
        /// With --probe, skip the 2-second consent delay.
        #[arg(long)]
        yes: bool,
        /// Emit machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,
    },
}

/// The `--provider` dialect override for `jig context`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    /// Anthropic Messages API dialect.
    Anthropic,
    /// OpenAI Chat Completions dialect.
    Openai,
}

impl From<ProviderArg> for jig_core::bench::Provider {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::Anthropic => jig_core::bench::Provider::Anthropic,
            ProviderArg::Openai => jig_core::bench::Provider::OpenAI,
        }
    }
}

/// The `--source` selector for `jig search`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchSource {
    /// Both the MCP registry and npm.
    All,
    /// The official MCP registry only.
    Registry,
    /// npm only.
    Npm,
}

impl From<SearchSource> for SourceSelector {
    fn from(s: SearchSource) -> Self {
        match s {
            SearchSource::All => SourceSelector::All,
            SearchSource::Registry => SourceSelector::Registry,
            SearchSource::Npm => SourceSelector::Npm,
        }
    }
}

/// Stack size for the thread that hosts the runtime. The clap command tree and
/// the (deeply nested, reqwest-carrying) async command futures are large; in an
/// unoptimized build — every `cargo test` / `cargo build` binary — they can
/// overflow the platform's default *main-thread* stack (only ~1 MiB on Windows),
/// overflowing on any invocation including `--help`. Hosting the runtime on a
/// thread with a generous stack makes `jig` behave identically in debug and
/// release, on every platform.
const MAIN_STACK_BYTES: usize = 32 * 1024 * 1024;

fn main() -> ExitCode {
    match std::thread::Builder::new()
        .name("jig-main".to_string())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(run_on_runtime)
    {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            eprintln!("jig: error: the main worker thread panicked");
            ExitCode::from(EXIT_FAILURE)
        }),
        Err(e) => {
            eprintln!("jig: error: could not start the main worker thread: {e}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Build a Tokio runtime on the current (big-stack) thread and drive the CLI.
fn run_on_runtime() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("jig: error: could not start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    rt.block_on(async_main())
}

async fn async_main() -> ExitCode {
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
        Command::Check {
            stdio,
            http,
            server,
            header,
            json,
            badge,
            min_score,
            percentiles,
            report,
            no_report,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
            check::run(
                &target,
                json,
                badge,
                min_score,
                percentiles,
                report,
                no_report,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Auth {
            http,
            stdio,
            header,
            json,
            tap,
            timeout,
        } => {
            let target = Target::resolve(stdio, http, None, header)?;
            auth::run(&target, json, tap.as_deref(), timeout).await
        }
        Command::Inspect {
            stdio,
            http,
            server,
            header,
            json,
            tap,
            timeout,
            max_message_bytes,
            listen,
            duration,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
            if listen && !matches!(target, Target::Http { .. }) {
                return Err("--listen requires --http (stdio has no server→client stream)".into());
            }
            inspect(
                &target,
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
                listen,
                duration,
            )
            .await
        }
        Command::Budget {
            stdio,
            http,
            server,
            header,
            models,
            json,
            markdown,
            tap,
            timeout,
            max_message_bytes,
            exact_anthropic,
            advise,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
            budget::run(
                &target,
                models,
                json,
                markdown,
                tap.as_deref(),
                timeout,
                max_message_bytes,
                exact_anthropic,
                advise,
            )
            .await
        }
        Command::Context {
            stdio,
            http,
            server,
            header,
            model,
            api_model,
            provider,
            raw,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
            context::run(
                &target,
                model,
                api_model,
                provider.map(Into::into),
                raw,
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Bench {
            stdio,
            http,
            server,
            header,
            task,
            models,
            api_model,
            runs,
            temp,
            json,
            save_case,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
            bench::run(
                &target,
                models,
                api_model,
                task,
                runs,
                temp,
                json,
                save_case.as_deref(),
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Eval {
            stdio,
            http,
            header,
            suites,
            model,
            api_model,
            runs_override,
            temp,
            gate,
            json,
            junit,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, None, header)?;
            eval::run(
                &target,
                suites,
                model,
                api_model,
                runs_override,
                temp,
                gate,
                json,
                junit.as_deref(),
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Call {
            stdio,
            http,
            server,
            header,
            tool,
            args,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, server, header)?;
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
        Command::Read {
            stdio,
            http,
            header,
            uri,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, None, header)?;
            read(
                &target,
                &uri,
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Prompt {
            stdio,
            http,
            header,
            name,
            args,
            json,
            tap,
            timeout,
            max_message_bytes,
        } => {
            let target = Target::resolve(stdio, http, None, header)?;
            prompt(
                &target,
                &name,
                args.as_deref(),
                json,
                tap.as_deref(),
                timeout,
                max_message_bytes,
            )
            .await
        }
        Command::Servers { json } => servers::run(json),
        Command::Search {
            query,
            source,
            limit,
            json,
        } => ecosystem::run_search(&query, source.into(), limit, json).await,
        Command::Info {
            name,
            probe,
            yes,
            json,
        } => ecosystem::run_info(&name, probe, yes, json).await,
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
        /// Environment variables to inject into the child (from a `--server`
        /// config entry). Empty for a plain `--stdio` command.
        env: Vec<(String, String)>,
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
    /// Resolve the mutually-exclusive `--stdio` / `--http` / `--server` flags
    /// (plus `--header`) into a single target, rejecting nonsensical
    /// combinations with a clear message. clap already enforces the
    /// `conflicts_with`, so the both-present cases are defensive.
    pub(crate) fn resolve(
        stdio: Option<String>,
        http: Option<String>,
        server: Option<String>,
        header: Vec<String>,
    ) -> Result<Target, String> {
        // `--server <name>` resolves against the machine's discovered configs.
        if let Some(name) = server {
            if stdio.is_some() || http.is_some() {
                return Err("--server is mutually exclusive with --stdio/--http".to_string());
            }
            return Self::from_discovered(&name, header);
        }

        match (stdio, http) {
            (Some(_), Some(_)) => Err("--stdio and --http are mutually exclusive".to_string()),
            (None, None) => Err(
                "one of --stdio <command>, --http <url>, or --server <name> is required"
                    .to_string(),
            ),
            (Some(cmd), None) => {
                if !header.is_empty() {
                    return Err(
                        "--header applies only to --http (stdio servers take no HTTP headers)"
                            .to_string(),
                    );
                }
                let (program, args) = split_command(&cmd)?;
                Ok(Target::Stdio {
                    program,
                    args,
                    env: Vec::new(),
                })
            }
            (None, Some(url)) => Ok(Target::Http {
                url,
                headers: parse_headers(&header)?,
            }),
        }
    }

    /// Resolve `--server <name>` (or `source:name`) against the MCP server
    /// configs discovered on this machine, building a target that carries the
    /// entry's transport and (for stdio) its environment.
    fn from_discovered(name: &str, header: Vec<String>) -> Result<Target, String> {
        let discovered = discovery::discover();
        for w in &discovered.warnings {
            eprintln!("jig: warning: {w}");
        }
        let entry = discovered.resolve(name).map_err(|e| e.to_string())?;
        if entry.disabled {
            eprintln!(
                "jig: note: server '{}' is marked disabled in {} — connecting anyway",
                entry.name,
                entry.source_file.display()
            );
        }
        match &entry.transport {
            DiscoveredTransport::Stdio { command, args } => {
                if !header.is_empty() {
                    return Err("--header applies only to HTTP servers".to_string());
                }
                Ok(Target::Stdio {
                    program: command.clone(),
                    args: args.clone(),
                    env: entry.env.clone(),
                })
            }
            DiscoveredTransport::Http { url } => Ok(Target::Http {
                url: url.clone(),
                headers: parse_headers(&header)?,
            }),
        }
    }

    /// A short transport label for the report header (`stdio` / `streamable-http`).
    pub(crate) fn transport_label(&self) -> &'static str {
        match self {
            Target::Stdio { .. } => "stdio",
            Target::Http { .. } => "streamable-http",
        }
    }

    /// Reconstruct the `jig check …` command line for this target, for the report
    /// header. Headers/env are omitted (they can carry secrets); the transport
    /// and endpoint are what a reader needs to reproduce the run.
    pub(crate) fn check_command_line(&self) -> String {
        match self {
            Target::Stdio { program, args, .. } => {
                let mut cmd = program.clone();
                for a in args {
                    cmd.push(' ');
                    cmd.push_str(a);
                }
                format!("jig check --stdio \"{cmd}\"")
            }
            Target::Http { url, .. } => format!("jig check --http {url}"),
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
        self.connect_with_listen(tap, timeout_secs, max_message_bytes, false)
            .await
    }

    /// Like [`Target::connect`], but with an explicit `listen` opt-in that
    /// enables the HTTP transport's standalone GET SSE stream (see
    /// [`Client::listen`](jig_core::Client::listen)). Only `jig inspect
    /// --listen` sets it; every other verb uses [`Target::connect`].
    pub(crate) async fn connect_with_listen(
        &self,
        tap: ProtocolTap,
        timeout_secs: u64,
        max_message_bytes: u64,
        listen: bool,
    ) -> Result<Client, String> {
        let mut opts = client_options(timeout_secs, max_message_bytes);
        opts.listen = listen;
        let client = match self {
            Target::Stdio { program, args, env } => {
                Client::connect_with_env(program, args, env, tap, opts).await
            }
            Target::Http { url, headers } => {
                Client::connect_http_with_options(url, headers.clone(), tap, opts).await
            }
        };
        client.map_err(|e| format!("failed to connect: {e}"))
    }
}

/// Run `jig inspect`.
#[allow(clippy::too_many_arguments)]
async fn inspect(
    target: &Target,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
    listen: bool,
    duration_secs: u64,
) -> Result<ExitCode, String> {
    // Own the tap so we can flush it even if a later step fails.
    let tap = ProtocolTap::new();
    let result = inspect_inner(
        target,
        tap.clone(),
        as_json,
        timeout_secs,
        max_message_bytes,
        listen,
        duration_secs,
    )
    .await;

    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

#[allow(clippy::too_many_arguments)]
async fn inspect_inner(
    target: &Target,
    tap: ProtocolTap,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
    listen: bool,
    duration_secs: u64,
) -> Result<ExitCode, String> {
    let client = target
        .connect_with_listen(tap, timeout_secs, max_message_bytes, listen)
        .await?;

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

    // Optionally hold the standalone GET stream open and report pushed traffic.
    if listen {
        let summary = client
            .listen(Duration::from_secs(duration_secs))
            .await
            .map_err(|e| format!("listen failed: {e}"))?;
        if as_json {
            let doc = json!({
                "opened": summary.opened,
                "status": summary.status,
                "notifications": summary.notifications,
                "pings": summary.pings,
                "otherRequests": summary.other_requests,
                "durationSecs": summary.duration.as_secs_f64(),
            });
            emit_line(&serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?);
        } else {
            emit("\n");
            emit(&render::listen_summary(&summary));
        }
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

/// Run `jig read` (`resources/read`).
async fn read(
    target: &Target,
    uri: &str,
    as_json: bool,
    tap_path: Option<&std::path::Path>,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let tap = ProtocolTap::new();
    let result = read_inner(
        target,
        tap.clone(),
        uri,
        as_json,
        timeout_secs,
        max_message_bytes,
    )
    .await;
    warn_non_protocol_output(&tap);
    write_tap_if_requested(&tap, tap_path);
    result
}

async fn read_inner(
    target: &Target,
    tap: ProtocolTap,
    uri: &str,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;

    let result = client
        .read_resource(uri)
        .await
        .map_err(|e| format!("resources/read failed: {e}"))?;

    if as_json {
        emit_line(&serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    } else {
        emit(&render::resource_read_result(uri, &result));
    }

    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Run `jig prompt` (`prompts/get`).
async fn prompt(
    target: &Target,
    name: &str,
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
    let result = prompt_inner(
        target,
        tap.clone(),
        name,
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
async fn prompt_inner(
    target: &Target,
    tap: ProtocolTap,
    name: &str,
    arguments: Value,
    as_json: bool,
    timeout_secs: u64,
    max_message_bytes: u64,
) -> Result<ExitCode, String> {
    let client = target.connect(tap, timeout_secs, max_message_bytes).await?;

    let result = client
        .get_prompt(name, arguments)
        .await
        .map_err(|e| format!("prompts/get failed: {e}"))?;

    if as_json {
        emit_line(&serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    } else {
        emit(&render::prompt_get_result(name, &result));
    }

    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Warn (on stderr) if the server wrote anything to stdout that is not a valid
/// JSON-RPC message. This is the single most common way an MCP server breaks in
/// practice — a stray `console.log`, a startup banner, or a logger misconfigured
/// to stdout — and it silently corrupts the newline-delimited framing. Jig is a
/// diagnostic tool, so it must name the problem loudly instead of hiding it.
const MAX_POLLUTION_LINES_SHOWN: usize = 5;

pub(crate) fn warn_non_protocol_output(tap: &ProtocolTap) {
    let bad = tap.non_protocol_inbound_detailed();
    if bad.is_empty() {
        return;
    }
    eprintln!(
        "jig: warning: server wrote {} non-protocol line(s) to stdout — this breaks MCP framing \
         (MCP stdio requires stdout to carry *only* newline-delimited JSON-RPC; send logs to stderr)",
        bad.len()
    );
    for line in bad.iter().take(MAX_POLLUTION_LINES_SHOWN) {
        let preview: String = line.raw.chars().take(120).collect();
        let at = match line.offset {
            Some(off) => format!("byte {off}"),
            None => format!("tap seq {}", line.seq),
        };
        eprintln!("jig:   [{at}] {preview}");
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
