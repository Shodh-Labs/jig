//! `jig-core` — the engine behind Jig, a testing workbench for MCP servers.
//!
//! It provides a JSON-RPC 2.0 client over a newline-delimited stdio transport,
//! the MCP handshake and core operations (`tools/list`, `resources/list`,
//! `prompts/list`, `tools/call`), and — the differentiator — a first-class
//! [`ProtocolTap`] that records every raw message crossing the wire.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> jig_core::Result<()> {
//! use jig_core::Client;
//!
//! let client = Client::connect("my-mcp-server", &[]).await?;
//! println!("connected to {}", client.server_info().name);
//! for tool in client.list_tools().await? {
//!     println!("- {}", tool.name);
//! }
//! // Every message is available for inspection.
//! for entry in client.tap().entries() {
//!     println!("{} {:?}", entry.direction, entry.method());
//! }
//! client.shutdown().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod advisor;
pub mod auth;
pub mod bench;
pub mod check;
pub mod client;
pub mod context;
pub mod discovery;
pub mod ecosystem;
pub mod error;
pub mod eval;
pub mod http;
pub mod login;
pub mod protocol;
pub mod tap;
pub mod tokens;
pub mod transport;

pub use advisor::{advise as advise_tool_set, ToolTokenCost};
pub use auth::{
    probe as probe_auth, AuthFinding, AuthReport, AuthServerMetadata, HttpExchange,
    Probe as AuthProbe, ProtectedResourceMetadata, Status as AuthStatus, Verdict as AuthVerdict,
    WwwAuthenticate, MCP_AUTH_SPEC_REVISION,
};
pub use bench::{
    ArgCheck, BenchConfig, BenchError, BenchModel, BenchReport, Distribution, Outcome, Provider,
    RunResult, Usage, BENCH_SYSTEM_PROMPT,
};
pub use check::{
    badge_color, bundled_percentiles, capability_offspec_note, evaluate, CheckInput,
    ContextProvenance, Dimension, DimensionScore, Finding, Observations, Percentiles,
    PollutionSite, Report as CheckReport, Severity, UnknownMethodProbe, BUNDLED_PERCENTILES_JSON,
    RUBRIC_VERSION,
};
pub use client::{Client, ClientOptions};
pub use context::{
    build as build_context, schema_to_human_lines, ContextView, InstructionsSection, ToolContext,
    CONTEXT_TASK_PLACEHOLDER, CONTEXT_TEMPERATURE,
};
pub use discovery::{
    discover, discover_from, DiscoveredTransport, Discovery, ResolveError, ServerEntry, Source,
    REDACTED,
};
pub use ecosystem::{
    search, EcoSource, NpmInfo, RegistryInfo, SearchOutcome, SearchResult, SourceSelector,
    NPM_BASE, REGISTRY_BASE,
};
pub use error::{JigError, Result};
pub use eval::{
    load_suite_file, load_suite_str, run_eval, score_case, Case, CaseReport, CaseVerdict, Defaults,
    EvalConfig, EvalError, Expect, Matcher, RunReport, Suite, SuiteReport,
};
pub use http::{HttpTransport, ListenSummary};
pub use login::{
    login, AuthenticatedSession, CallbackParams, LoginConfig, LoginOutcome, LoginStep, Pkce,
    Secret, LOGIN_CLIENT_NAME,
};
pub use protocol::{
    ContentBlock, Implementation, InitializeResult, Prompt, PromptArgument, PromptGetResult,
    PromptMessage, Resource, ResourceContents, ResourceReadResult, Tool, ToolCallResult,
    LATEST_PROTOCOL_VERSION,
};
pub use tap::{Direction, NonProtocolLine, ProtocolTap, TapEntry};
pub use tokens::{
    budget_local, canonical_tool_json, Exactness, ModelBudget, ModelCounter, TokenError,
    ToolBudget, CANONICAL_RENDERING_DOC,
};
pub use transport::{
    StdioTransport, Transport, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_REQUEST_TIMEOUT,
};
