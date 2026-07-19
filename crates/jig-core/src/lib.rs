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

pub mod bench;
pub mod client;
pub mod discovery;
pub mod ecosystem;
pub mod error;
pub mod http;
pub mod protocol;
pub mod tap;
pub mod tokens;
pub mod transport;

pub use bench::{
    ArgCheck, BenchConfig, BenchError, BenchModel, BenchReport, Distribution, Outcome, Provider,
    RunResult, Usage, BENCH_SYSTEM_PROMPT,
};
pub use client::{Client, ClientOptions};
pub use discovery::{
    discover, discover_from, DiscoveredTransport, Discovery, ResolveError, ServerEntry, Source,
    REDACTED,
};
pub use ecosystem::{
    search, EcoSource, NpmInfo, RegistryInfo, SearchOutcome, SearchResult, SourceSelector,
    NPM_BASE, REGISTRY_BASE,
};
pub use error::{JigError, Result};
pub use http::HttpTransport;
pub use protocol::{
    ContentBlock, Implementation, InitializeResult, Prompt, PromptArgument, Resource, Tool,
    ToolCallResult, LATEST_PROTOCOL_VERSION,
};
pub use tap::{Direction, ProtocolTap, TapEntry};
pub use tokens::{
    budget_local, canonical_tool_json, Exactness, ModelBudget, ModelCounter, TokenError,
    ToolBudget, CANONICAL_RENDERING_DOC,
};
pub use transport::{
    StdioTransport, Transport, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_REQUEST_TIMEOUT,
};
