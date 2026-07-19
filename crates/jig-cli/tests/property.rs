//! Property-based tests for the CLI's hostile-input parsers.
//!
//! `--stdio "<command>"` and `--header "Name: Value"` are the two places a user
//! hands Jig an arbitrary string. Neither parser may ever panic; every
//! malformed input must be a typed `Err`. These properties fuzz both with
//! arbitrary text and assert the invariants the rest of the CLI relies on.
//!
//! Case counts follow proptest's defaults (256), configurable via
//! `PROPTEST_CASES`.

use jig_cli::{parse_headers, split_command};
use proptest::prelude::*;

proptest! {
    /// The `--stdio` splitter never panics on any input, and whenever it
    /// succeeds the program token is non-empty (an empty command is an error).
    #[test]
    fn split_command_never_panics(input in ".*") {
        if let Ok((program, _args)) = split_command(&input) {
            prop_assert!(!program.is_empty());
        }
    }

    /// Unquoted whitespace-separated tokens round-trip: joining tokens with
    /// single spaces and splitting yields exactly those tokens back. This pins
    /// the everyday `program --flag value` behaviour against regressions.
    #[test]
    fn split_command_round_trips_simple_tokens(
        tokens in prop::collection::vec("[a-zA-Z0-9_./=-]{1,12}", 1..8)
    ) {
        let joined = tokens.join(" ");
        let (program, args) = split_command(&joined).expect("simple tokens always split");
        let mut all = vec![program];
        all.extend(args);
        prop_assert_eq!(all, tokens);
    }

    /// A balanced pair of quotes around a segment containing spaces keeps it a
    /// single token, whatever the (quote/backslash-free) contents.
    #[test]
    fn split_command_keeps_quoted_segment_together(
        inner in "[a-zA-Z0-9 ._/\\\\:-]{0,24}"
    ) {
        let input = format!("prog \"{inner}\"");
        let (program, args) = split_command(&input).expect("balanced quotes split");
        prop_assert_eq!(program, "prog");
        prop_assert_eq!(args.len(), 1);
        prop_assert_eq!(&args[0], &inner);
    }

    /// The `--header` parser never panics on any input, and whenever it
    /// succeeds every parsed header name is non-empty.
    #[test]
    fn parse_headers_never_panics(inputs in prop::collection::vec(".*", 0..6)) {
        if let Ok(pairs) = parse_headers(&inputs) {
            for (name, _value) in pairs {
                prop_assert!(!name.trim().is_empty());
            }
        }
    }

    /// A `Name: Value` header where neither side contains a colon parses to the
    /// trimmed pair.
    #[test]
    fn parse_headers_parses_well_formed(
        name in "[A-Za-z][A-Za-z0-9-]{0,20}",
        value in "[A-Za-z0-9 ._/=-]{0,32}"
    ) {
        let raw = format!("{name}: {value}");
        let pairs = parse_headers(std::slice::from_ref(&raw)).expect("well-formed header parses");
        prop_assert_eq!(pairs.len(), 1);
        prop_assert_eq!(&pairs[0].0, name.trim());
        prop_assert_eq!(&pairs[0].1, value.trim());
    }
}
