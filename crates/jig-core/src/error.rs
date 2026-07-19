//! Error taxonomy for Jig.
//!
//! Jig distinguishes three failure domains, which callers usually want to
//! treat differently:
//!
//! * [`JigError::Transport`] — the pipe to the child process broke, the
//!   process could not be spawned, or a line could not be read/written. The
//!   MCP conversation never got far enough to be meaningful.
//! * [`JigError::Protocol`] — bytes moved, but they did not conform to what
//!   MCP / JSON-RPC 2.0 requires (malformed message, missing fields, a
//!   response we cannot correlate, etc.).
//! * [`JigError::Server`] — the server behaved correctly at the protocol
//!   level and deliberately returned a JSON-RPC error object. This is a
//!   *reported* error, distinct from Jig failing to talk to the server.
//! * [`JigError::Timeout`] — the server accepted a request but did not answer
//!   within the allotted time. A hang is a first-class failure mode Jig must
//!   surface (naming the method), never wait out forever.
//! * [`JigError::MessageTooLarge`] — an inbound message exceeded the configured
//!   size cap. A diagnostic tool must bound its own memory: rather than buffer
//!   an unbounded (or deliberately hostile) payload, Jig stops and names the
//!   cap that was hit.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

/// The unified error type for all fallible `jig-core` operations.
#[derive(Debug, Error)]
pub enum JigError {
    /// The transport layer failed: spawn failure, broken pipe, EOF while a
    /// request was still pending, or an I/O error on stdin/stdout.
    #[error("transport error: {0}")]
    Transport(String),

    /// A message crossed the wire but violated the JSON-RPC 2.0 / MCP
    /// contract (unparseable JSON, missing `result`/`error`, uncorrelatable
    /// id, unexpected shape).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server returned a well-formed JSON-RPC error response. This is the
    /// server *reporting* a problem, not Jig failing to reach it.
    #[error("server error {code}: {message}")]
    Server {
        /// JSON-RPC error code (e.g. `-32601` method not found).
        code: i64,
        /// Human-readable message supplied by the server.
        message: String,
        /// Optional structured `data` payload attached by the server.
        data: Option<Value>,
    },

    /// The request was sent but no response arrived within the configured
    /// timeout. The connection may still be alive; the server simply never
    /// answered *this* method. Naming the method makes a hang diagnosable.
    #[error("request '{method}' timed out after {elapsed:?} with no response")]
    Timeout {
        /// The JSON-RPC method whose response never arrived.
        method: String,
        /// How long Jig waited before giving up.
        elapsed: Duration,
    },

    /// An inbound message exceeded the configured maximum size (see
    /// `--max-message-bytes` / [`ClientOptions::max_message_bytes`]). Jig stops
    /// reading rather than buffer an unbounded payload, and names the cap so the
    /// operator can raise it deliberately if the traffic is legitimate.
    ///
    /// [`ClientOptions::max_message_bytes`]: crate::ClientOptions::max_message_bytes
    #[error(
        "inbound message exceeded the maximum size of {limit} bytes \
         (raise the cap with --max-message-bytes, or 0 to disable it)"
    )]
    MessageTooLarge {
        /// The configured byte cap that the inbound message overran.
        limit: usize,
    },
}

impl JigError {
    /// Convenience constructor for a transport error from anything printable.
    pub(crate) fn transport(msg: impl Into<String>) -> Self {
        JigError::Transport(msg.into())
    }

    /// Convenience constructor for a protocol error from anything printable.
    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        JigError::Protocol(msg.into())
    }

    /// Returns `true` if this is a "method not found" server error
    /// (JSON-RPC `-32601`), which MCP servers use to signal that a capability
    /// / method is not supported. Callers use this to degrade gracefully.
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, JigError::Server { code: -32601, .. })
    }
}

impl From<std::io::Error> for JigError {
    fn from(e: std::io::Error) -> Self {
        JigError::Transport(e.to_string())
    }
}

impl From<serde_json::Error> for JigError {
    fn from(e: serde_json::Error) -> Self {
        // A serialization/deserialization failure is a protocol-level fault:
        // the message either could not be encoded or did not match the shape
        // MCP requires.
        JigError::Protocol(e.to_string())
    }
}

/// A `Result` alias fixed to [`JigError`].
pub type Result<T> = std::result::Result<T, JigError>;
