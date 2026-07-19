#![no_main]
//! Fuzz the stdio inbound-line handler: arbitrary bytes fed to
//! `classify_inbound` (the tap/route logic the background reader uses) must
//! always produce a value and a routing decision without panicking, and the
//! routing contract must hold — a routed id implies a correlatable response.

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let (value, route_id) = jig_core::transport::classify_inbound(data);
    if let Some(id) = route_id {
        // Invariant: only a JSON object carrying a matching integer id and a
        // result/error is ever routed.
        assert!(value.is_object());
        assert!(value.get("result").is_some() || value.get("error").is_some());
        assert_eq!(value.get("id").and_then(Value::as_i64), Some(id));
    }
});
