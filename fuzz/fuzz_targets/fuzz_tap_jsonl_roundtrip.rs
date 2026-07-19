#![no_main]
//! Fuzz the tap's JSONL round-trip law: for any JSON message payload, recording
//! it in a `ProtocolTap`, serializing to JSONL, and parsing the lines back must
//! reproduce the exact same entries. Non-JSON input is simply ignored (there is
//! no message to record).

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

use jig_core::{Direction, ProtocolTap, TapEntry};

fuzz_target!(|data: &[u8]| {
    // Interpret the input as a JSON message payload; skip non-JSON.
    let Ok(msg) = serde_json::from_slice::<Value>(data) else {
        return;
    };

    let tap = ProtocolTap::new();
    tap.record(Direction::Outbound, msg.clone());
    tap.record(Direction::Inbound, msg);
    let original = tap.entries();

    let jsonl = tap.to_jsonl();
    let parsed: Vec<TapEntry> = jsonl
        .lines()
        .map(|l| serde_json::from_str::<TapEntry>(l).expect("each JSONL line must parse back"))
        .collect();

    assert_eq!(parsed, original, "tap JSONL round-trip is not identity");
});
