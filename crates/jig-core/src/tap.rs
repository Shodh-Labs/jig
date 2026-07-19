//! The **protocol tap**: a first-class, structured record of every raw
//! JSON-RPC message that crosses the wire, in either direction, with a
//! monotonic timestamp.
//!
//! This is Jig's differentiator. Everything a model would see — and
//! everything Jig sends on its behalf — is captured here verbatim as the
//! parsed JSON value, so it can be inspected, asserted on in tests, and
//! serialized to JSONL for offline analysis or regression fixtures.
//!
//! The tap is cheap to clone ([`ProtocolTap`] is an `Arc` handle) so the
//! reader task and the writer path can share one tap without ceremony.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The direction a message traveled, from Jig's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Jig (client) -> server. A request or notification we sent.
    Outbound,
    /// Server -> Jig (client). A response or notification we received.
    Inbound,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Outbound => f.write_str("->"),
            Direction::Inbound => f.write_str("<-"),
        }
    }
}

/// One recorded message on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TapEntry {
    /// Monotonic sequence number, starting at 0, assigned in record order.
    pub seq: u64,
    /// Which way the message traveled.
    pub direction: Direction,
    /// Microseconds since the tap was created (monotonic; from
    /// [`std::time::Instant`], never wall-clock, so it cannot go backwards).
    pub elapsed_micros: u64,
    /// Byte offset in the server's stdout stream where this line began, for
    /// inbound stdio traffic. `None` for outbound messages and for transports
    /// (HTTP) that do not carry a single byte-addressable stream. This is what
    /// lets a stdout-pollution finding point at the *exact* byte where the
    /// framing broke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// The full parsed JSON-RPC message, exactly as it appeared on the wire.
    pub message: Value,
}

impl TapEntry {
    /// Best-effort extraction of the JSON-RPC `method`, if this message has
    /// one (requests and notifications do; responses do not).
    pub fn method(&self) -> Option<&str> {
        self.message.get("method").and_then(Value::as_str)
    }
}

/// A non-protocol (framing-breaking) inbound line, with its location in the
/// server's stdout stream. Produced by
/// [`ProtocolTap::non_protocol_inbound_detailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonProtocolLine {
    /// The tap sequence number of the offending entry.
    pub seq: u64,
    /// Byte offset in the stdout stream where the line began, if the transport
    /// tracked one (stdio does; HTTP does not).
    pub offset: Option<u64>,
    /// The offending line's text (lossily decoded, non-JSON preserved verbatim).
    pub raw: String,
}

#[derive(Debug)]
struct TapInner {
    entries: Vec<TapEntry>,
    next_seq: u64,
}

/// A shared, cloneable handle to a protocol tap.
///
/// Cloning yields another handle to the *same* underlying log. All Jig
/// components involved in one session share a single tap.
#[derive(Debug, Clone)]
pub struct ProtocolTap {
    inner: Arc<Mutex<TapInner>>,
    start: Instant,
}

impl Default for ProtocolTap {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolTap {
    /// Create a fresh, empty tap. The monotonic clock starts now.
    pub fn new() -> Self {
        ProtocolTap {
            inner: Arc::new(Mutex::new(TapInner {
                entries: Vec::new(),
                next_seq: 0,
            })),
            start: Instant::now(),
        }
    }

    /// Record a message. Never panics: if the lock is poisoned we recover the
    /// inner data rather than unwinding, because losing the ability to record
    /// traffic must not take down a session.
    pub fn record(&self, direction: Direction, message: Value) {
        self.record_at(direction, None, message);
    }

    /// Record an **inbound** line together with the byte `offset` in the
    /// server's stdout stream where it began. Used by the stdio reader so a
    /// stdout-pollution finding can name the exact byte position of the break.
    pub fn record_inbound_at(&self, offset: u64, message: Value) {
        self.record_at(Direction::Inbound, Some(offset), message);
    }

    /// Shared recorder: assign a sequence number and push an entry, optionally
    /// with a stream byte offset.
    fn record_at(&self, direction: Direction, offset: Option<u64>, message: Value) {
        let elapsed_micros = self.start.elapsed().as_micros() as u64;
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let seq = guard.next_seq;
        guard.next_seq += 1;
        guard.entries.push(TapEntry {
            seq,
            direction,
            elapsed_micros,
            offset,
            message,
        });
    }

    /// Snapshot of all entries recorded so far, in order.
    pub fn entries(&self) -> Vec<TapEntry> {
        match self.inner.lock() {
            Ok(g) => g.entries.clone(),
            Err(poisoned) => poisoned.into_inner().entries.clone(),
        }
    }

    /// Inbound lines that are **not** valid JSON-RPC messages.
    ///
    /// Every legitimate MCP message is a JSON object; anything else on the
    /// server's stdout — a log line, a stack trace, a stray `console.log`, or
    /// even a bare JSON scalar/array — is stdout pollution that corrupts the
    /// newline-delimited framing. The reader records such lines verbatim (as a
    /// JSON string when they were not even parseable) rather than dropping them,
    /// so this method can surface them to the user. Returned as `(seq, raw)`
    /// pairs in record order, where `raw` is the offending line's text.
    pub fn non_protocol_inbound(&self) -> Vec<(u64, String)> {
        self.entries()
            .into_iter()
            .filter(|e| e.direction == Direction::Inbound && !e.message.is_object())
            .map(|e| {
                let raw = match &e.message {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (e.seq, raw)
            })
            .collect()
    }

    /// Like [`ProtocolTap::non_protocol_inbound`], but carries each offending
    /// line's byte `offset` in the stdout stream (when the transport tracked
    /// one). The finding layer uses the offset and the raw text to point a
    /// stdout-pollution fix at the exact byte and quote the first bytes.
    pub fn non_protocol_inbound_detailed(&self) -> Vec<NonProtocolLine> {
        self.entries()
            .into_iter()
            .filter(|e| e.direction == Direction::Inbound && !e.message.is_object())
            .map(|e| {
                let raw = match &e.message {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                NonProtocolLine {
                    seq: e.seq,
                    offset: e.offset,
                    raw,
                }
            })
            .collect()
    }

    /// Number of entries recorded so far.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.entries.len(),
            Err(poisoned) => poisoned.into_inner().entries.len(),
        }
    }

    /// Whether the tap is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Serialize the entire tap as JSONL (one JSON object per line). The
    /// trailing newline is included so files concatenate cleanly.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for entry in self.entries() {
            // TapEntry always serializes (message is already a Value); if it
            // somehow failed we simply skip that line rather than panic.
            if let Ok(line) = serde_json::to_string(&entry) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    /// Write the tap to `path` as JSONL.
    pub fn write_jsonl(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_jsonl())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_seq_direction_and_message() {
        let tap = ProtocolTap::new();
        tap.record(Direction::Outbound, json!({"method": "initialize"}));
        tap.record(Direction::Inbound, json!({"result": {}}));

        let entries = tap.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[0].direction, Direction::Outbound);
        assert_eq!(entries[1].direction, Direction::Inbound);
        assert_eq!(entries[0].method(), Some("initialize"));
        assert_eq!(entries[1].method(), None);
    }

    #[test]
    fn timestamps_are_monotonic_non_decreasing() {
        let tap = ProtocolTap::new();
        for _ in 0..50 {
            tap.record(Direction::Outbound, json!({}));
        }
        let entries = tap.entries();
        for pair in entries.windows(2) {
            assert!(pair[1].elapsed_micros >= pair[0].elapsed_micros);
            assert_eq!(pair[1].seq, pair[0].seq + 1);
        }
    }

    #[test]
    fn jsonl_has_one_valid_json_object_per_line() {
        let tap = ProtocolTap::new();
        tap.record(Direction::Outbound, json!({"a": 1}));
        tap.record(Direction::Inbound, json!({"b": [1, 2, 3]}));

        let jsonl = tap.to_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let v: Value = serde_json::from_str(line).expect("each line is valid JSON");
            assert!(v.get("seq").is_some());
            assert!(v.get("direction").is_some());
            assert!(v.get("message").is_some());
        }
    }

    /// Regression: found by `fuzz_tap_jsonl_roundtrip` on its first run.
    /// Without serde_json's `float_roundtrip` feature the parser is up to
    /// 2 ULP off on extreme floats, so a wire value would not survive a
    /// tap JSONL round trip — the tap would misreport what it saw.
    #[test]
    fn extreme_float_survives_jsonl_roundtrip_exactly() {
        let wire: Value = serde_json::from_str("1.000877015e+211").expect("valid JSON number");
        let tap = ProtocolTap::new();
        tap.record(Direction::Inbound, wire.clone());

        let jsonl = tap.to_jsonl();
        let reparsed: TapEntry =
            serde_json::from_str(jsonl.lines().next().expect("one line")).expect("tap line parses");
        assert_eq!(
            reparsed.message, wire,
            "tap JSONL round-trip must reproduce wire floats exactly"
        );
    }

    #[test]
    fn non_protocol_inbound_flags_stdout_pollution() {
        let tap = ProtocolTap::new();
        // A well-formed inbound response: not a violation.
        tap.record(
            Direction::Inbound,
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
        );
        // A non-JSON log line the reader preserved as a string: a violation.
        tap.record(
            Direction::Inbound,
            Value::String("[info] server started".into()),
        );
        // A bare JSON scalar on stdout: also not a valid JSON-RPC message.
        tap.record(Direction::Inbound, json!(42));
        // Outbound noise (we never emit non-objects, but prove it is ignored).
        tap.record(Direction::Outbound, Value::String("ignored".into()));

        let bad = tap.non_protocol_inbound();
        assert_eq!(bad.len(), 2);
        assert_eq!(bad[0], (1, "[info] server started".to_string()));
        assert_eq!(bad[1], (2, "42".to_string()));
    }

    #[test]
    fn non_protocol_inbound_detailed_carries_offset_and_raw() {
        let tap = ProtocolTap::new();
        tap.record_inbound_at(0, json!({ "jsonrpc": "2.0", "id": 1, "result": {} }));
        tap.record_inbound_at(37, Value::String("[info] server started".into()));
        let bad = tap.non_protocol_inbound_detailed();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].offset, Some(37));
        assert_eq!(bad[0].raw, "[info] server started");
        // The legacy accessor still agrees on the raw text.
        assert_eq!(
            tap.non_protocol_inbound(),
            vec![(1, "[info] server started".to_string())]
        );
    }

    #[test]
    fn direction_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Direction::Outbound).unwrap(),
            "\"outbound\""
        );
        assert_eq!(
            serde_json::to_string(&Direction::Inbound).unwrap(),
            "\"inbound\""
        );
    }
}
