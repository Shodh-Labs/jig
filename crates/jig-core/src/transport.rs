//! Newline-delimited JSON-RPC 2.0 transport over a child process's stdio.
//!
//! Per the MCP spec (`2025-06-18`, "Transports > stdio"): messages are
//! individual JSON-RPC objects **delimited by newlines** and **MUST NOT**
//! contain embedded newlines. This is *not* LSP-style `Content-Length`
//! framing. Encoding is UTF-8. The server's stderr is for logging and is
//! drained separately so it can never block the protocol stream.
//!
//! A single background reader task owns the child's stdout, records every
//! inbound line to the [`ProtocolTap`], and routes responses back to the
//! request that is awaiting them via per-id oneshot channels.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::error::{JigError, Result};
use crate::tap::{Direction, ProtocolTap};

/// Map of in-flight request ids to the channel awaiting their response.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A live JSON-RPC-over-stdio connection to a spawned MCP server.
pub struct StdioTransport {
    child: Mutex<Child>,
    stdin: AsyncMutex<ChildStdin>,
    pending: PendingMap,
    tap: ProtocolTap,
    next_id: AtomicI64,
    reader: JoinHandle<()>,
    stderr_drain: JoinHandle<()>,
}

impl StdioTransport {
    /// Spawn `program` with `args` and wire up the stdio transport.
    ///
    /// The child is launched with `kill_on_drop` so a dropped transport never
    /// leaks a server process.
    pub fn spawn(program: &str, args: &[String], tap: ProtocolTap) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| JigError::transport(format!("failed to spawn '{program}': {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| JigError::transport("child stdin was not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| JigError::transport("child stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| JigError::transport("child stderr was not captured"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let reader = tokio::spawn(read_loop(stdout, Arc::clone(&pending), tap.clone()));
        let stderr_drain = tokio::spawn(drain_stderr(stderr));

        Ok(StdioTransport {
            child: Mutex::new(child),
            stdin: AsyncMutex::new(stdin),
            pending,
            tap,
            next_id: AtomicI64::new(1),
            reader,
            stderr_drain,
        })
    }

    /// Access the shared protocol tap for this connection.
    pub fn tap(&self) -> &ProtocolTap {
        &self.tap
    }

    /// Send a JSON-RPC request and await its correlated response `result`.
    ///
    /// Returns [`JigError::Server`] if the server replied with an error
    /// object, or [`JigError::Transport`] if the connection closed first.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = lock(&self.pending);
            guard.insert(id, tx);
        }

        self.write_message(&message).await.inspect_err(|_| {
            // Clean up the pending slot if the write never made it out.
            lock(&self.pending).remove(&id);
        })?;

        let response = rx.await.map_err(|_| {
            JigError::transport(format!(
                "connection closed before response to '{method}' (id {id})"
            ))
        })?;

        parse_response(response)
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&message).await
    }

    /// Serialize, tap, and write a single newline-delimited message.
    async fn write_message(&self, message: &Value) -> Result<()> {
        // The tap sees the exact message we are about to send.
        self.tap.record(Direction::Outbound, message.clone());

        let mut line = serde_json::to_string(message)?;
        debug_assert!(!line.contains('\n'), "outbound message must be single-line");
        line.push('\n');

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| JigError::transport(format!("failed to write to server stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| JigError::transport(format!("failed to flush server stdin: {e}")))?;
        Ok(())
    }

    /// Gracefully shut the connection down: close stdin, then kill and reap
    /// the child so no process is left behind.
    pub async fn shutdown(self) -> Result<()> {
        // Dropping stdin signals EOF to a well-behaved server.
        drop(self.stdin);

        // Abort background tasks; they hold no state we need to preserve.
        self.reader.abort();
        self.stderr_drain.abort();

        // Move the child out of the mutex so no guard is held across the
        // `await` below (`self` is consumed, so this is safe and exclusive).
        let mut child = self.child.into_inner().unwrap_or_else(|p| p.into_inner());
        // Best-effort terminate; ignore "already exited" style errors.
        let _ = child.start_kill();
        child
            .wait()
            .await
            .map_err(|e| JigError::transport(format!("failed to reap server process: {e}")))?;
        Ok(())
    }
}

/// Lock helper that recovers from poisoning instead of panicking.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Parse a raw JSON-RPC response value into either its `result` or an error.
fn parse_response(response: Value) -> Result<Value> {
    if let Some(err) = response.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>")
            .to_string();
        let data = err.get("data").cloned();
        return Err(JigError::Server {
            code,
            message,
            data,
        });
    }
    match response.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(JigError::protocol(
            "response contained neither 'result' nor 'error'",
        )),
    }
}

/// The background reader: one line = one inbound JSON-RPC message.
async fn read_loop(stdout: tokio::process::ChildStdout, pending: PendingMap, tap: ProtocolTap) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                // Record inbound traffic. If a line is not valid JSON (a
                // misbehaving server), preserve it verbatim as a string so the
                // tap still tells the truth about what arrived.
                let value: Value =
                    serde_json::from_str(&line).unwrap_or_else(|_| Value::String(line.clone()));
                tap.record(Direction::Inbound, value.clone());

                // Route responses (they carry an id and result/error) to the
                // waiting request. Notifications have no id and are ignored at
                // the transport layer.
                if let Some(id) = value.get("id").and_then(Value::as_i64) {
                    if value.get("result").is_some() || value.get("error").is_some() {
                        if let Some(tx) = lock(&pending).remove(&id) {
                            let _ = tx.send(value);
                        }
                    }
                }
            }
            Ok(None) => break, // EOF: server closed stdout.
            Err(_) => break,   // I/O error on the pipe.
        }
    }
    // On exit, drop every pending sender so awaiting requests observe the
    // closed connection as a transport error rather than hanging forever.
    lock(&pending).clear();
}

/// Drain the child's stderr so its logging can never fill a pipe buffer and
/// deadlock the protocol stream. Content is discarded (it is server logging,
/// not MCP traffic).
async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(_)) = lines.next_line().await {
        // Intentionally ignored.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_extracts_result() {
        let v = json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } });
        let out = parse_response(v).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn parse_response_maps_error_object_to_server_error() {
        let v = json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32601, "message": "Method not found" }
        });
        let err = parse_response(v).unwrap_err();
        assert!(err.is_method_not_found());
        match err {
            JigError::Server { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "Method not found");
            }
            other => panic!("expected server error, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_rejects_message_without_result_or_error() {
        let v = json!({ "jsonrpc": "2.0", "id": 1 });
        let err = parse_response(v).unwrap_err();
        assert!(matches!(err, JigError::Protocol(_)));
    }
}
