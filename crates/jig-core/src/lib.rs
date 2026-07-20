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
pub mod boot;
pub mod check;
pub mod client;
pub mod clients;
pub mod context;
pub mod credential;
pub mod discovery;
pub mod ecosystem;
pub mod error;
pub mod eval;
pub mod http;
pub mod injection;
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
    classify_sampling_text, distribution_of, finalize_args_check, host_models_of,
    provider_endpoint, render_sampling_params, ArgCheck, BenchConfig, BenchError, BenchModel,
    BenchReport, Distribution, Outcome, Provider, RunResult, SamplingBenchReport, Usage,
    BENCH_SYSTEM_PROMPT, SAMPLING_MODEL_UNKNOWN, SAMPLING_RESPONSE_PROTOCOL,
};
pub use boot::{is_npx, npx_package, prewarm_args, Timing as BootTiming};
pub use check::{
    badge_color, bundled_percentiles, capability_offspec_note, evaluate, CheckInput, ContextCap,
    ContextProvenance, Dimension, DimensionScore, Finding, Observations, Percentiles,
    PollutionSite, ProtocolCap, Report as CheckReport, Severity, UnknownMethodProbe,
    BUNDLED_PERCENTILES_JSON, RATE_SCORE_FLOOR, RUBRIC_VERSION,
};
pub use client::{Client, ClientOptions};
pub use clients::{
    known_clients, ClientError, ClientRendering, ClientSpec, Evidence, RenderedName, CLIENTS,
    DEFAULT_CLIENT,
};
pub use context::{
    build as build_context, build_for_client as build_context_for_client, schema_to_human_lines,
    ClientVariant, ContextBuildError, ContextView, InstructionsSection, ToolContext,
    CONTEXT_TASK_PLACEHOLDER, CONTEXT_TEMPERATURE,
};
pub use credential::{
    grade as grade_startup, named_variable, StartupObservation, Verdict as StartupVerdict,
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
pub use injection::scan as scan_injection;
pub use login::{
    login, AuthenticatedSession, CallbackParams, LoginConfig, LoginOutcome, LoginStep, Pkce,
    Secret, LOGIN_CLIENT_NAME,
};
pub use protocol::{
    ContentBlock, Implementation, InitializeResult, Prompt, PromptArgument, PromptGetResult,
    PromptMessage, Resource, ResourceContents, ResourceReadResult, Tool, ToolAnnotations,
    ToolCallResult, LATEST_PROTOCOL_VERSION,
};
pub use tap::{Direction, NonProtocolLine, ProtocolTap, TapEntry};
pub use tokens::{
    budget_local, canonical_tool_json, Exactness, ModelBudget, ModelCounter, TokenError,
    ToolBudget, CANONICAL_RENDERING_DOC,
};
pub use transport::{
    StderrVolume, StdioTransport, Transport, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_REQUEST_TIMEOUT,
};
