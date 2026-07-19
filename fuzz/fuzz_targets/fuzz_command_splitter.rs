#![no_main]
//! Fuzz the `--stdio` command splitter: any string must split into an
//! `Ok((program, args))` with a non-empty program, or a typed `Err` — never a
//! panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    if let Ok((program, _args)) = jig_cli::split_command(&input) {
        assert!(!program.is_empty());
    }
});
