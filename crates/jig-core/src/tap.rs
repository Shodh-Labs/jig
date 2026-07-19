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
