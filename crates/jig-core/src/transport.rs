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

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::error::{JigError, Result};
use crate::tap::{Direction, ProtocolTap};

/// Map of in-flight request ids to the channel awaiting their response.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A fatal condition the background reader observed (e.g. an oversized message),
/// shared so an awaiting request can report *why* the stream ended rather than a
/// generic "connection closed".
type ReadFault = Arc<Mutex<Option<JigError>>>;

/// A bounded ring of the child's most recent stderr lines, kept so a spawn/
/// handshake/request failure can quote the server's own diagnostics.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// How much a child process wrote to its stderr over the whole session.
///
/// The transport keeps a bounded ring of the child's most recent stderr lines
/// for error context, so that ring cannot answer "how much did this server
/// log?" — an evicted line is gone, and a truncated line under-reports. These
/// counters are cumulative and are taken *before* the ring's truncation, so the
/// figure reflects what the child actually wrote.
///
/// Volume is **informational**. The stdio transport spec designates stderr for
/// logging, so a chatty server is a smell, never a defect: Jig reports this and
/// does not score it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StderrVolume {
    /// Total newline-terminated lines observed on the child's stderr.
    pub lines: usize,
    /// Total bytes observed: each line's UTF-8 byte length plus one for the
    /// newline that terminated it.
    pub bytes: usize,
}

impl StderrVolume {
    /// Whether the child wrote nothing at all to stderr.
    pub fn is_silent(&self) -> bool {
        self.lines == 0 && self.bytes == 0
    }
}

/// The shared cumulative stderr counters, written by the drain task and read by
/// [`StdioTransport::stderr_volume`].
#[derive(Debug, Default)]
struct StderrCounters {
    lines: AtomicUsize,
    bytes: AtomicUsize,
}

impl StderrCounters {
    fn record(&self, line_bytes: usize) {
        self.lines.fetch_add(1, Ordering::Relaxed);
        // +1 for the newline the line reader consumed.
        self.bytes.fetch_add(line_bytes + 1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> StderrVolume {
        StderrVolume {
            lines: self.lines.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// Default per-request timeout: a server that accepts a request and never
/// answers must not hang Jig forever. Overridable at spawn (`None` disables it).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on a single inbound message, in bytes (64 MiB). Generous enough
/// for any legitimate MCP payload, but bounded so a runaway or hostile server
/// cannot make Jig buffer without limit. Override via
/// [`ClientOptions::max_message_bytes`](crate::ClientOptions::max_message_bytes)
/// / the `--max-message-bytes` flag; `None` disables the cap.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on how long a disconnect diagnosis waits for the separate
/// stderr-drain task to finish flushing the child's final lines. Reached only if
/// the drain is unusually slow; normally it settles in a few milliseconds once
/// the child's stderr hits EOF.
const STDERR_SETTLE_TIMEOUT: Duration = Duration::from_millis(750);
/// Poll interval while waiting for [`STDERR_SETTLE_TIMEOUT`].
const STDERR_SETTLE_POLL: Duration = Duration::from_millis(5);

/// How many trailing stderr lines to retain for error context.
const STDERR_TAIL_LINES: usize = 20;
/// Max characters kept per retained stderr line (defends against a single
/// pathologically long log line eating memory).
const STDERR_LINE_MAX: usize = 1024;

/// A live JSON-RPC-over-stdio connection to a spawned MCP server.
pub struct StdioTransport {
    child: Mutex<Child>,
    stdin: AsyncMutex<ChildStdin>,
    pending: PendingMap,
    tap: ProtocolTap,
    next_id: AtomicI64,
    /// Per-request timeout. `None` means wait indefinitely.
    request_timeout: Option<Duration>,
    /// A fatal read-side condition (e.g. an oversized inbound message) recorded
    /// by the reader so awaiting requests can report the real cause.
    read_fault: ReadFault,
    /// The child's most recent stderr lines, for enriching disconnect errors.
    stderr_tail: StderrTail,
    /// Set true when the stderr-drain task finishes (the child's stderr reached
    /// EOF and every line has been captured), so a disconnect diagnosis can wait
    /// for the full tail rather than guess at a fixed grace period.
    stderr_done: Arc<AtomicBool>,
    /// Cumulative stderr volume, tracked separately from the bounded tail ring
    /// so eviction and per-line truncation cannot distort the figure.
    stderr_counters: Arc<StderrCounters>,
    reader: JoinHandle<()>,
    stderr_drain: JoinHandle<()>,
}

impl StdioTransport {
    /// Spawn `program` with `args` and wire up the stdio transport, using the
    /// [`DEFAULT_REQUEST_TIMEOUT`] and [`DEFAULT_MAX_MESSAGE_BYTES`].
    ///
    /// The child is launched with `kill_on_drop` so a dropped transport never
    /// leaks a server process.
    pub fn spawn(program: &str, args: &[String], tap: ProtocolTap) -> Result<Self> {
        Self::spawn_with_timeout(program, args, tap, Some(DEFAULT_REQUEST_TIMEOUT))
    }

    /// Like [`StdioTransport::spawn`] but with an explicit per-request timeout
    /// (`None` waits forever). Uses [`DEFAULT_MAX_MESSAGE_BYTES`] for the inbound
    /// size cap.
    pub fn spawn_with_timeout(
        program: &str,
        args: &[String],
        tap: ProtocolTap,
        request_timeout: Option<Duration>,
    ) -> Result<Self> {
        Self::spawn_with_limits(
            program,
            args,
            tap,
            request_timeout,
            Some(DEFAULT_MAX_MESSAGE_BYTES),
        )
    }

    /// Full-control constructor: explicit per-request timeout and inbound message
    /// size cap.
    ///
    /// * `request_timeout` — `None` waits forever (use with care).
    /// * `max_message_bytes` — `None` disables the size cap (unbounded buffering,
    ///   also use with care); `Some(n)` fails a single inbound message that
    ///   exceeds `n` bytes with [`JigError::MessageTooLarge`].
    pub fn spawn_with_limits(
        program: &str,
        args: &[String],
        tap: ProtocolTap,
        request_timeout: Option<Duration>,
        max_message_bytes: Option<usize>,
    ) -> Result<Self> {
        Self::spawn_with_env(program, args, &[], tap, request_timeout, max_message_bytes)
    }

    /// Like [`StdioTransport::spawn_with_limits`], but also injects extra
    /// environment variables into the spawned child.
    ///
    /// `env` is a list of `(key, value)` pairs added *on top of* the inherited
    /// environment — this is how a server discovered from a client config
    /// (`jig inspect --server <name>`) receives the API keys/tokens its config
    /// declares. The pairs are passed only to the child process; Jig never logs
    /// or echoes the values.
    pub fn spawn_with_env(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        tap: ProtocolTap,
        request_timeout: Option<Duration>,
        max_message_bytes: Option<usize>,
    ) -> Result<Self> {
        // On Windows, `npx`/`npm`/`yarn` etc. are `.cmd` shims, and
        // `Command::new("npx")` looks for a file named literally "npx" — which
        // does not exist — so the spawn fails with "program not found". The OS
        // CreateProcess call does *not* apply PATHEXT the way the shell does.
        // Resolve the program against PATH + PATHEXT ourselves so the everyday
        // `jig inspect --stdio "npx -y <server>"` invocation just works. Modern
        // Rust (>=1.77) then spawns the resolved `.cmd`/`.bat` safely via the
        // command processor with proper argument escaping. See `resolve_program`.
        let resolved = resolve_program(program);
        let mut command = Command::new(&resolved);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
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
        let read_fault: ReadFault = Arc::new(Mutex::new(None));
        let stderr_tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_done = Arc::new(AtomicBool::new(false));
        let stderr_counters: Arc<StderrCounters> = Arc::new(StderrCounters::default());

        let reader = tokio::spawn(read_loop(
            stdout,
            Arc::clone(&pending),
            tap.clone(),
            max_message_bytes,
            Arc::clone(&read_fault),
        ));
        let stderr_drain = tokio::spawn(drain_stderr(
            stderr,
            Arc::clone(&stderr_tail),
            Arc::clone(&stderr_done),
            Arc::clone(&stderr_counters),
        ));

        Ok(StdioTransport {
            child: Mutex::new(child),
            stdin: AsyncMutex::new(stdin),
            pending,
            tap,
            next_id: AtomicI64::new(1),
            request_timeout,
            read_fault,
            stderr_tail,
            stderr_done,
            stderr_counters,
            reader,
            stderr_drain,
        })
    }

    /// Access the shared protocol tap for this connection.
    pub fn tap(&self) -> &ProtocolTap {
        &self.tap
    }

    /// How much the child has written to stderr so far this session.
    ///
    /// A live snapshot: the drain task runs concurrently, so calling this while
    /// the server is still logging returns the volume observed *up to now*.
    /// Call it after the operations you care about (and before shutdown) for a
    /// figure covering the whole session.
    pub fn stderr_volume(&self) -> StderrVolume {
        self.stderr_counters.snapshot()
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

        if let Err(write_err) = self.write_message(&message).await {
            // Clean up the pending slot if the write never made it out.
            lock(&self.pending).remove(&id);
            // A broken pipe here usually means the child already died (e.g. it
            // exited during the handshake). If so, prefer the richer diagnosis
            // that names the exit status and the server's own stderr.
            if self.child_exit_status().is_some() {
                // Let the stderr drain surface the child's last words first.
                self.await_stderr_settled().await;
                return Err(self.describe_disconnect(method, id));
            }
            return Err(write_err);
        }

        // Await the correlated response, bounded by the request timeout. A
        // server that accepts the request but never answers must surface as a
        // named `Timeout`, not an indefinite hang.
        //
        // If the sender was dropped, the reader task ended: the connection
        // closed (EOF), the stream faulted, or the child crashed. Rather than a
        // bare "connection closed", report the specific cause — a recorded read
        // fault (e.g. an oversized message) if there is one, otherwise the
        // child's exit status and its last stderr lines. A brief grace lets the
        // separate stderr-drain task flush the child's final lines so they can
        // be quoted in the error.
        let recv = async {
            match rx.await {
                Ok(v) => Ok(v),
                Err(_) => {
                    self.await_stderr_settled().await;
                    Err(self.describe_disconnect(method, id))
                }
            }
        };

        let response = match self.request_timeout {
            Some(dur) => match tokio::time::timeout(dur, recv).await {
                Ok(result) => result?,
                Err(_elapsed) => {
                    // Give up on this id so a late response is dropped cleanly
                    // rather than routed to a channel no one is holding.
                    lock(&self.pending).remove(&id);
                    return Err(JigError::Timeout {
                        method: method.to_string(),
                        elapsed: dur,
                    });
                }
            },
            None => recv.await?,
        };

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

    /// Build the error for a request whose response never arrived because the
    /// reader task ended. Prefers, in order: a recorded fatal read fault (e.g.
    /// an oversized message), the child's exit status (a crash/exit), then a
    /// bare closed-connection message. The child's recent stderr is appended
    /// whenever we have any, since that is usually where the *why* lives.
    fn describe_disconnect(&self, method: &str, id: i64) -> JigError {
        // A recorded fatal read fault is the most specific cause.
        if let Some(fault) = lock(&self.read_fault).take() {
            return fault;
        }
        let stderr = self.stderr_context();
        match self.child_exit_status() {
            Some(status) => JigError::transport(format!(
                "server process {} before responding to '{method}' (id {id}){stderr}",
                describe_status(status)
            )),
            None => JigError::transport(format!(
                "connection closed before response to '{method}' (id {id}){stderr}"
            )),
        }
    }

    /// Wait, bounded, for a disconnect to fully "settle" before it is diagnosed:
    /// the child observably exited (so its exit *status* is available, not just
    /// its closed pipes) **and** the stderr-drain task finished (so its final
    /// lines are captured). These two race the stdout EOF that triggered us, and
    /// on some platforms the process is not immediately reapable the instant its
    /// pipes close.
    ///
    /// Returns immediately when a fatal read fault is already recorded (an
    /// oversized message is self-explanatory and the child may still be alive),
    /// and caps out at [`STDERR_SETTLE_TIMEOUT`] so a child that lingers with an
    /// open stderr cannot stall the error path.
    async fn await_stderr_settled(&self) {
        if lock(&self.read_fault).is_some() {
            return;
        }
        let deadline = tokio::time::Instant::now() + STDERR_SETTLE_TIMEOUT;
        loop {
            let exited = self.child_exit_status().is_some();
            let stderr_done = self.stderr_done.load(Ordering::Relaxed);
            if (exited && stderr_done) || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(STDERR_SETTLE_POLL).await;
        }
    }

    /// Non-blocking check of whether the child has already exited, returning its
    /// status if so. Reaps the child as a side effect (harmless — shutdown
    /// tolerates an already-reaped child).
    fn child_exit_status(&self) -> Option<std::process::ExitStatus> {
        let mut child = lock(&self.child);
        child.try_wait().ok().flatten()
    }

    /// Format the retained stderr tail as a bounded, single-line suffix for an
    /// error message, or the empty string if the server logged nothing.
    fn stderr_context(&self) -> String {
        let lines = lock(&self.stderr_tail);
        if lines.is_empty() {
            return String::new();
        }
        let joined = lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" | ");
        format!(". Last server stderr: {joined}")
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

/// A transport-agnostic connection to an MCP server.
///
/// [`Client`](crate::Client) speaks to this enum, not to a concrete transport,
/// so the handshake and every operation work identically whether the server is
/// a local subprocess ([`StdioTransport`]) or a remote Streamable-HTTP endpoint
/// ([`HttpTransport`](crate::http::HttpTransport)).
///
/// An enum was chosen over a `dyn`/`async_trait` object deliberately: the two
/// transports have different construction, no shared runtime state, and Rust's
/// native `async fn` in inherent impls dispatches here without the allocation,
/// object-safety, or macro friction of async trait objects. The two variants
/// present the identical surface — `request`, `notify`, `tap`, `shutdown` — so
/// callers never match on the variant.
pub enum Transport {
    /// Newline-delimited JSON-RPC over a child process's stdio. Boxed: the
    /// stdio transport (child handle, join handles, several mutexes) is far
    /// larger than the HTTP one, and boxing keeps the enum small.
    Stdio(Box<StdioTransport>),
    /// JSON-RPC over the MCP Streamable HTTP transport.
    Http(crate::http::HttpTransport),
}

impl Transport {
    /// Send a JSON-RPC request and await its correlated response `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(t) => t.request(method, params).await,
            Transport::Http(t) => t.request(method, params).await,
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        match self {
            Transport::Stdio(t) => t.notify(method, params).await,
            Transport::Http(t) => t.notify(method, params).await,
        }
    }

    /// The protocol tap capturing this connection's traffic.
    pub fn tap(&self) -> &ProtocolTap {
        match self {
            Transport::Stdio(t) => t.tap(),
            Transport::Http(t) => t.tap(),
        }
    }

    /// How much the server has written to stderr this session, when the
    /// transport has a child stderr at all.
    ///
    /// `None` for HTTP: a remote server's stderr belongs to a process Jig never
    /// spawned and cannot observe. Reporting `0` there would be a claim Jig has
    /// no basis for, so the absence is modelled explicitly.
    pub fn stderr_volume(&self) -> Option<StderrVolume> {
        match self {
            Transport::Stdio(t) => Some(t.stderr_volume()),
            Transport::Http(_) => None,
        }
    }

    /// Open the standalone server→client stream and process pushed traffic for
    /// `duration`, returning a summary. Only the Streamable HTTP transport has
    /// such a stream (a GET SSE stream); stdio has no equivalent, so it reports
    /// a clear error rather than pretending.
    pub async fn listen(&self, duration: Duration) -> Result<crate::http::ListenSummary> {
        match self {
            Transport::Http(t) => t.listen(duration).await,
            Transport::Stdio(_) => Err(JigError::transport(
                "listening for server-initiated messages is only supported on the HTTP transport",
            )),
        }
    }

    /// Cleanly terminate the connection (kill the child, or end the HTTP session).
    pub async fn shutdown(self) -> Result<()> {
        match self {
            Transport::Stdio(t) => t.shutdown().await,
            Transport::Http(t) => t.shutdown().await,
        }
    }
}

/// Resolve a program name to a concrete path the OS can spawn.
///
/// On non-Windows platforms this is the identity: the kernel's `execvp`-style
/// lookup already searches `PATH` and there are no implicit extensions.
///
/// On Windows it is *not* the identity. `CreateProcess` (what `std`/`tokio`
/// call) does not consult `PATHEXT`, so `Command::new("npx")` searches for a
/// file named exactly `npx` and fails — even though `npx.cmd` sits right there
/// on `PATH`. Node's `npx`/`npm`, Yarn, pnpm, and countless other tools ship
/// only as `.cmd`/`.bat` shims, so this bites essentially every real-world
/// `jig inspect --stdio "npx ..."`. We reproduce the shell's resolution: if the
/// name has no directory component and no extension, walk `PATH` looking for
/// `name` + each `PATHEXT` extension and return the first hit. Anything already
/// absolute/relative, or already carrying an extension, is returned untouched.
/// If nothing matches we return the original so the caller still gets the
/// familiar "program not found" spawn error.
#[cfg(not(windows))]
fn resolve_program(program: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(program)
}

#[cfg(windows)]
fn resolve_program(program: &str) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};

    let as_path = Path::new(program);

    // A path with a directory component or an explicit extension is taken as
    // authored — the user pointed us at a specific file.
    if as_path.extension().is_some()
        || program.contains('/')
        || program.contains('\\')
        || program.contains(':')
    {
        return PathBuf::from(program);
    }

    // Extensions to try, from PATHEXT, falling back to the usual defaults.
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let exts: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| e.to_string())
        .collect();

    // Search each PATH directory for `program` + each candidate extension.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for ext in &exts {
                let candidate = dir.join(format!("{program}{ext}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }

    // No shim found: hand back the original and let spawn report the failure.
    PathBuf::from(program)
}

/// Classify one inbound stdio line's raw bytes the way the reader does: the
/// [`Value`] to record in the tap, and the request id it should be routed to
/// (`Some` only for a correlatable JSON-RPC *response* — one carrying an integer
/// id together with a `result` or `error`).
///
/// This is the single source of truth for inbound-line handling, shared by the
/// background reader and by the property/fuzz harnesses. It is **total**: any
/// byte sequence (invalid UTF-8, non-JSON, a bare scalar, a notification) yields
/// a value and a routing decision, never a panic. Non-UTF-8 input is decoded
/// lossily and non-JSON input is preserved verbatim as a JSON string, exactly
/// as the tap records it.
pub fn classify_inbound(bytes: &[u8]) -> (Value, Option<i64>) {
    let line = String::from_utf8_lossy(bytes);
    let value: Value =
        serde_json::from_str(&line).unwrap_or_else(|_| Value::String(line.into_owned()));
    let route_id = response_route_id(&value);
    (value, route_id)
}

/// The id a message should be routed to, or `None` if it is not a correlatable
/// response (a notification, a request, or a non-object).
fn response_route_id(value: &Value) -> Option<i64> {
    let id = value.get("id").and_then(Value::as_i64)?;
    if value.get("result").is_some() || value.get("error").is_some() {
        Some(id)
    } else {
        None
    }
}

/// Parse a raw JSON-RPC response value into either its `result` or an error.
///
/// Shared by both transports: the JSON-RPC response envelope is identical over
/// stdio and HTTP.
pub fn parse_response(response: Value) -> Result<Value> {
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

/// Outcome of reading one newline-delimited frame from the child's stdout.
enum Frame {
    /// A complete line's raw bytes (newline stripped).
    Line(Vec<u8>),
    /// Clean end of stream (EOF) with nothing buffered.
    Eof,
    /// A single line overran the configured size cap.
    TooLarge,
    /// An I/O error on the pipe.
    Io,
}

/// Read one newline-delimited frame with a byte cap.
///
/// Unlike [`AsyncBufReadExt::lines`], this (a) operates on raw bytes, so a
/// non-UTF-8 server cannot abort the whole stream — the bytes are captured and
/// lossily decoded for the tap — and (b) enforces `max_bytes` so a single
/// gigantic (or hostile) line cannot make Jig buffer without limit. A final
/// unterminated line before EOF is still returned (matching `lines`).
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: Option<usize>,
) -> Frame {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf().await {
            Ok(a) => a,
            Err(_) => return Frame::Io,
        };
        if available.is_empty() {
            // EOF. A buffered but unterminated final line still counts.
            return if buf.is_empty() {
                Frame::Eof
            } else {
                Frame::Line(std::mem::take(&mut buf))
            };
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..i]);
                reader.consume(i + 1);
                if let Some(max) = max_bytes {
                    if buf.len() > max {
                        return Frame::TooLarge;
                    }
                }
                return Frame::Line(buf);
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
                // Enforce the cap while still accumulating, so an unterminated
                // flood is stopped before it exhausts memory.
                if let Some(max) = max_bytes {
                    if buf.len() > max {
                        return Frame::TooLarge;
                    }
                }
            }
        }
    }
}

/// The background reader: one line = one inbound JSON-RPC message.
async fn read_loop(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    tap: ProtocolTap,
    max_bytes: Option<usize>,
    read_fault: ReadFault,
) {
    let mut reader = BufReader::new(stdout);
    // Running byte position in the stdout stream, so each inbound line can be
    // located precisely — a stdout-pollution finding names the exact offset.
    let mut stream_offset: u64 = 0;
    loop {
        match read_frame(&mut reader, max_bytes).await {
            Frame::Line(bytes) => {
                // Offset of this line's first byte; advance past the line and its
                // newline delimiter (a final unterminated line over-counts by 1,
                // but nothing follows it, so no later offset is affected).
                let offset = stream_offset;
                stream_offset += bytes.len() as u64 + 1;
                // A blank line carries no message; skip it before classifying.
                if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                // `classify_inbound` decodes lossily (a non-UTF-8 server pollutes
                // the stream but must not kill the reader), records non-JSON
                // verbatim, and tells us the routing id — the same logic the
                // property/fuzz harnesses exercise directly.
                let (value, route_id) = classify_inbound(&bytes);
                tap.record_inbound_at(offset, value.clone());

                // Route a correlatable response to its waiting request.
                // Notifications and stray messages are recorded but not routed.
                if let Some(id) = route_id {
                    if let Some(tx) = lock(&pending).remove(&id) {
                        let _ = tx.send(value);
                    }
                }
            }
            Frame::TooLarge => {
                // Record why the stream is ending so an awaiting request reports
                // the size cap rather than a bare "connection closed".
                if let Some(limit) = max_bytes {
                    *lock(&read_fault) = Some(JigError::MessageTooLarge { limit });
                }
                break;
            }
            Frame::Eof | Frame::Io => break,
        }
    }
    // On exit, drop every pending sender so awaiting requests observe the
    // closed connection as an error rather than hanging forever.
    lock(&pending).clear();
}

/// Drain the child's stderr so its logging can never fill a pipe buffer and
/// deadlock the protocol stream. The content is *not* protocol traffic, but the
/// most recent [`STDERR_TAIL_LINES`] lines are retained so a crash/exit error
/// can quote the server's own diagnostics — the difference between "connection
/// closed" and "connection closed. Last server stderr: panicked at ...".
async fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    tail: StderrTail,
    done: Arc<AtomicBool>,
    counters: Arc<StderrCounters>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut line = line;
        // Count the line at full length, before the ring truncates it — the
        // volume figure must describe what the server wrote, not what we kept.
        counters.record(line.len());
        if line.chars().count() > STDERR_LINE_MAX {
            line = line.chars().take(STDERR_LINE_MAX).collect::<String>() + "…";
        }
        let mut guard = lock(&tail);
        if guard.len() == STDERR_TAIL_LINES {
            guard.pop_front();
        }
        guard.push_back(line);
    }
    // stderr reached EOF (the child closed it, typically on exit): the tail is
    // now complete, so a disconnect diagnosis can stop waiting for it.
    done.store(true, Ordering::Relaxed);
}

/// Human-readable description of how a child process ended.
fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {code}"),
        // No code → terminated by a signal (Unix) or otherwise abnormally.
        None => format!("terminated abnormally ({status})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_volume_counts_lines_with_their_newline() {
        let c = StderrCounters::default();
        assert!(c.snapshot().is_silent());

        c.record(5); // a 5-byte line + its newline
        c.record(0); // a bare newline
        let v = c.snapshot();
        assert_eq!(v.lines, 2);
        assert_eq!(v.bytes, 7);
        assert!(!v.is_silent());
    }

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

    #[cfg(windows)]
    #[test]
    fn resolve_program_finds_cmd_shim_on_windows() {
        // `cmd` is guaranteed present on Windows as `cmd.exe`, but there is no
        // bare file named `cmd` — exactly the PATHEXT gap that breaks
        // `Command::new("npx")`. Resolution must turn it into a real file.
        let resolved = resolve_program("cmd");
        assert!(
            resolved.is_file(),
            "expected a concrete existing file, got {resolved:?}"
        );
        assert_ne!(
            resolved,
            std::path::PathBuf::from("cmd"),
            "resolution should have appended an extension"
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_program_passes_through_explicit_paths_and_extensions() {
        // A name that already carries an extension is honored verbatim.
        assert_eq!(
            resolve_program("server.exe"),
            std::path::PathBuf::from("server.exe")
        );
        // A path with a directory component is the user's explicit choice.
        assert_eq!(
            resolve_program(r"C:\tools\server"),
            std::path::PathBuf::from(r"C:\tools\server")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_program_is_identity_off_windows() {
        assert_eq!(resolve_program("npx"), std::path::PathBuf::from("npx"));
    }

    // ---- read_frame: the capped, byte-oriented line reader ------------------

    async fn frames(input: &[u8], max: Option<usize>) -> Vec<Frame> {
        let mut reader = BufReader::new(input);
        let mut out = Vec::new();
        loop {
            let f = read_frame(&mut reader, max).await;
            let stop = matches!(f, Frame::Eof | Frame::Io | Frame::TooLarge);
            out.push(f);
            if stop {
                break;
            }
        }
        out
    }

    fn as_line(f: &Frame) -> &[u8] {
        match f {
            Frame::Line(b) => b,
            _ => panic!("expected a line frame"),
        }
    }

    #[tokio::test]
    async fn read_frame_splits_on_newlines_and_reports_eof() {
        let fs = frames(b"one\ntwo\nthree\n", None).await;
        // three lines then a clean EOF
        assert_eq!(fs.len(), 4);
        assert_eq!(as_line(&fs[0]), b"one");
        assert_eq!(as_line(&fs[1]), b"two");
        assert_eq!(as_line(&fs[2]), b"three");
        assert!(matches!(fs[3], Frame::Eof));
    }

    #[tokio::test]
    async fn read_frame_returns_unterminated_final_line() {
        let fs = frames(b"complete\npartial", None).await;
        assert_eq!(as_line(&fs[0]), b"complete");
        // The trailing line with no newline is still delivered before EOF.
        assert_eq!(as_line(&fs[1]), b"partial");
        assert!(matches!(fs[2], Frame::Eof));
    }

    #[tokio::test]
    async fn read_frame_preserves_non_utf8_bytes() {
        // Invalid UTF-8 must not abort the reader; the raw bytes come through.
        let fs = frames(&[0xFF, 0xFE, b'\n'], None).await;
        assert_eq!(as_line(&fs[0]), &[0xFF, 0xFE]);
    }

    #[tokio::test]
    async fn read_frame_enforces_the_size_cap() {
        // A line longer than the cap yields TooLarge, not an unbounded buffer.
        let big = vec![b'x'; 100];
        let fs = frames(&big, Some(16)).await;
        assert!(matches!(fs.last().unwrap(), Frame::TooLarge));
    }

    #[tokio::test]
    async fn read_frame_allows_lines_up_to_the_cap() {
        // Exactly at the cap is fine; the cap is a strict "greater than".
        let line = vec![b'a'; 16];
        let mut input = line.clone();
        input.push(b'\n');
        let fs = frames(&input, Some(16)).await;
        assert_eq!(as_line(&fs[0]), line.as_slice());
    }

    #[test]
    fn message_too_large_error_names_the_limit() {
        let e = JigError::MessageTooLarge { limit: 64 };
        let msg = e.to_string();
        assert!(msg.contains("64"), "got: {msg}");
        assert!(msg.contains("max-message-bytes"), "got: {msg}");
    }
}
