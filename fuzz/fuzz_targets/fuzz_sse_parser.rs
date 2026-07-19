#![no_main]
//! Fuzz the SSE parser: arbitrary bytes (lossily decoded to text) fed to
//! `parse_sse` must always yield either a `Vec` of messages or a typed protocol
//! error — never a panic, never a hang.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    // The method name is irrelevant to framing; a fixed one keeps runs stable.
    let _ = jig_core::http::parse_sse(&text, "fuzz");
});
