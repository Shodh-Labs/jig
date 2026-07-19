//! Property-based tests for `jig-core`'s parsers and round-trip laws.
//!
//! The standing bar: **arbitrary input never panics the library.** Every parser
//! that touches the wire is fed arbitrary bytes / arbitrary JSON and must
//! produce either a valid result or a typed error — never an unwind, never a
//! hang. The round-trip laws pin the invariants Jig's tap and token engine rely
//! on: a tap survives a JSONL round-trip byte-for-byte, and canonical tool
//! rendering is deterministic regardless of input key ordering.
//!
//! Case counts follow proptest's defaults (256 per property) and are
//! configurable via the `PROPTEST_CASES` environment variable, so CI stays
//! fast while a nightly run can crank the pressure up.

use std::collections::HashSet;

use std::time::Duration;

use jig_core::bench::{classify_anthropic, classify_openai, validate_args};
use jig_core::check::{MetricSamples, Percentiles};
use jig_core::eval::{load_suite_str, Matcher};
use jig_core::http::parse_sse;
use jig_core::tokens::canonical_tool_json;
use jig_core::transport::{classify_inbound, parse_response};
use jig_core::{
    advise_tool_set, evaluate, CheckInput, Direction, Observations, ProtocolTap, Severity,
    TapEntry, Tool, ToolTokenCost,
};

use proptest::prelude::*;
use serde_json::{json, Map, Value};

/// A strategy producing arbitrary JSON values of bounded depth, so payloads stay
/// realistic (and generation stays fast) while still covering scalars, arrays,
/// objects, and awkward strings.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        // Include control chars, quotes, and unicode in strings.
        ".*".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 32, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::hash_map(".*", inner, 0..6)
                .prop_map(|m| { Value::Object(m.into_iter().collect::<Map<String, Value>>()) }),
        ]
    })
}

/// A strategy producing an arbitrary [`Matcher`] of every kind — including
/// deliberately-malformed regex patterns, which must degrade to a non-match, not
/// a panic.
fn arb_matcher() -> impl Strategy<Value = Matcher> {
    prop_oneof![
        arb_json().prop_map(Matcher::Exact),
        ".*".prop_map(Matcher::Contains),
        ".*".prop_map(Matcher::Regex),
        prop::collection::vec(arb_json(), 0..5).prop_map(Matcher::OneOf),
        (
            proptest::option::of(any::<f64>()),
            proptest::option::of(any::<f64>())
        )
            .prop_map(|(min, max)| Matcher::Range { min, max }),
    ]
}

proptest! {
    // ---- Eval matchers are total over arbitrary JSON -----------------------

    /// Evaluating any matcher (incl. a malformed regex) against arbitrary JSON
    /// yields a bool, never a panic.
    #[test]
    fn matcher_matches_never_panics(m in arb_matcher(), v in arb_json()) {
        let _ = m.matches(&v);
    }

    /// Loading an arbitrary string as a suite yields a suite or a typed error —
    /// never a panic or a hang.
    #[test]
    fn load_suite_never_panics(text in ".*") {
        let _ = load_suite_str(&text, "prop.yaml");
    }

    // ---- Framing / parsers never panic on arbitrary input ------------------

    /// The stdio line handler classifies *any* bytes into a tap value and a
    /// routing decision without panicking. A routed id always corresponds to a
    /// message that actually carries a `result` or `error` (the routing
    /// contract), and non-object input is never routed.
    #[test]
    fn classify_inbound_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let (value, route_id) = classify_inbound(&bytes);
        if let Some(id) = route_id {
            prop_assert!(value.is_object());
            prop_assert!(value.get("result").is_some() || value.get("error").is_some());
            prop_assert_eq!(value.get("id").and_then(Value::as_i64), Some(id));
        }
    }

    /// A valid JSON-RPC response line is always routed to its id; the recorded
    /// value is exactly the parsed message.
    #[test]
    fn classify_inbound_routes_valid_responses(id in any::<i64>(), ok in any::<bool>()) {
        let msg = if ok {
            json!({ "jsonrpc": "2.0", "id": id, "result": { "x": 1 } })
        } else {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -1, "message": "e" } })
        };
        let line = serde_json::to_string(&msg).unwrap();
        let (value, route_id) = classify_inbound(line.as_bytes());
        prop_assert_eq!(route_id, Some(id));
        prop_assert_eq!(value, msg);
    }

    /// The SSE parser is total: arbitrary text yields either a vector of
    /// messages or a typed protocol error — never a panic.
    #[test]
    fn parse_sse_never_panics(text in ".*") {
        let _ = parse_sse(&text, "prop");
    }

    /// SSE parsing of arbitrary *bytes* (via lossy text) also never panics.
    #[test]
    fn parse_sse_never_panics_on_bytes(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let text = String::from_utf8_lossy(&bytes);
        let _ = parse_sse(&text, "prop");
    }

    /// `parse_response` maps any JSON value to a result, a server error, or a
    /// protocol error — never a panic.
    #[test]
    fn parse_response_never_panics(v in arb_json()) {
        let _ = parse_response(v);
    }

    // ---- Bench: classification & validation are total over arbitrary JSON ----

    /// Classifying an arbitrary Anthropic-shaped response never panics, whatever
    /// junk the (mock or misbehaving) provider returns. The outcome is always
    /// one of the taxonomy variants.
    #[test]
    fn classify_anthropic_never_panics(v in arb_json()) {
        let tools: HashSet<String> = ["echo", "make_reservation"].iter().map(|s| s.to_string()).collect();
        let c = classify_anthropic(&v, &tools);
        // The tag is always a known taxonomy label (proves a variant was formed).
        prop_assert!(matches!(
            c.outcome.tag(),
            "selected" | "no_tool" | "hallucinated_tool" | "provider_error"
        ));
    }

    /// Same total-ness for the OpenAI dialect — including the malformed
    /// `arguments`-string path, which must degrade, never unwind.
    #[test]
    fn classify_openai_never_panics(v in arb_json()) {
        let tools: HashSet<String> = ["echo", "make_reservation"].iter().map(|s| s.to_string()).collect();
        let c = classify_openai(&v, &tools);
        prop_assert!(matches!(
            c.outcome.tag(),
            "selected" | "no_tool" | "hallucinated_tool" | "provider_error"
        ));
    }

    /// The argument validator is total: arbitrary (schema, args) pairs yield a
    /// verdict, never a panic.
    #[test]
    fn validate_args_never_panics(schema in arb_json(), args in arb_json()) {
        let _ = validate_args(&schema, &args);
    }

    /// A well-formed `{ "result": ... }` envelope always yields that result.
    #[test]
    fn parse_response_extracts_arbitrary_result(inner in arb_json()) {
        let env = json!({ "jsonrpc": "2.0", "id": 1, "result": inner.clone() });
        let got = parse_response(env).expect("a result envelope must parse");
        prop_assert_eq!(got, inner);
    }

    // ---- Round-trip law: TapEntry -> JSONL -> TapEntry = identity ----------

    /// A tap of arbitrary JSON payloads serializes to JSONL and parses back to
    /// an identical set of entries — the invariant offline analysis relies on.
    #[test]
    fn tap_jsonl_round_trip_is_identity(
        payloads in prop::collection::vec((any::<bool>(), arb_json()), 0..12)
    ) {
        let tap = ProtocolTap::new();
        for (inbound, msg) in &payloads {
            let dir = if *inbound { Direction::Inbound } else { Direction::Outbound };
            tap.record(dir, msg.clone());
        }
        let original = tap.entries();

        let jsonl = tap.to_jsonl();
        let parsed: Vec<TapEntry> = jsonl
            .lines()
            .map(|l| serde_json::from_str::<TapEntry>(l).expect("each JSONL line parses back"))
            .collect();

        prop_assert_eq!(parsed, original);
    }

    // ---- Determinism law: canonical tool rendering -------------------------

    /// Canonical tool rendering is deterministic: repeated calls on the same
    /// tool produce byte-identical output, and — crucially — the output does not
    /// depend on the key order in which the input schema's object was built. A
    /// server that emits the same schema with keys in a different order must
    /// price identically, or the budget would be non-reproducible.
    #[test]
    fn canonical_rendering_is_order_independent(
        name in "[a-zA-Z0-9_]{1,24}",
        desc in proptest::option::of(".{0,64}"),
        keys in prop::collection::hash_set("[a-zA-Z0-9_]{1,12}", 0..8),
    ) {
        // Build a schema whose `properties` object is assembled twice, in two
        // different insertion orders, from the same key set.
        let forward: Vec<String> = keys.iter().cloned().collect();
        let mut reversed = forward.clone();
        reversed.reverse();

        let schema_from = |order: &[String]| -> Value {
            let mut props = Map::new();
            for k in order {
                props.insert(k.clone(), json!({ "type": "string" }));
            }
            let mut root = Map::new();
            root.insert("type".to_string(), json!("object"));
            root.insert("properties".to_string(), Value::Object(props));
            Value::Object(root)
        };

        let make_tool = |schema: Value| -> Tool {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(name));
            if let Some(d) = &desc {
                m.insert("description".to_string(), json!(d));
            }
            m.insert("inputSchema".to_string(), schema);
            serde_json::from_value(Value::Object(m)).expect("tool parses")
        };

        let a = canonical_tool_json(&make_tool(schema_from(&forward)));
        let b = canonical_tool_json(&make_tool(schema_from(&reversed)));
        let a2 = canonical_tool_json(&make_tool(schema_from(&forward)));

        prop_assert_eq!(&a, &b, "rendering depends on input key order");
        prop_assert_eq!(&a, &a2, "rendering is not idempotent");
    }

    // ---- Report card: scoring is total and bounded ------------------------

    /// The `jig check` scorer never panics over an arbitrary tool list and
    /// arbitrary observations, and every score it produces — the composite and
    /// every applicable dimension — stays within `0..=100`. This is the core
    /// safety contract of the report card: no input can make it crash or emit a
    /// nonsensical grade.
    #[test]
    fn evaluate_is_total_and_bounded(
        tools in arb_tools(),
        pollution in 0usize..500,
        list_timed_out in any::<bool>(),
        latency_ms in proptest::option::of(0u64..10_000),
        clean_shutdown in any::<bool>(),
        instructions in proptest::option::of(".{0,64}"),
        samples in prop::collection::vec(0f64..200_000.0, 0..40),
    ) {
        let input = CheckInput {
            server_name: "prop-server".to_string(),
            server_version: "0.0.0".to_string(),
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({ "tools": {} }),
            instructions,
            tools,
            observations: Observations {
                pollution_lines: pollution,
                list_timed_out,
                list_latency: latency_ms.map(Duration::from_millis),
                clean_shutdown,
                stderr_noise_bytes: None,
            },
        };

        // With and without an ecosystem dataset — both scoring paths are total.
        let percentiles = if samples.is_empty() {
            None
        } else {
            Some(Percentiles {
                context_cost_tokens: MetricSamples { samples },
                collected: None,
            })
        };

        for pct in [None, percentiles.as_ref()] {
            let report = evaluate(&input, pct);
            prop_assert!(
                (0.0..=100.0).contains(&report.composite),
                "composite out of range: {}",
                report.composite
            );
            for d in &report.dimensions {
                if let Some(s) = d.score {
                    prop_assert!(
                        (0.0..=100.0).contains(&s),
                        "dimension {} score out of range: {}",
                        d.dimension.label(),
                        s
                    );
                }
            }
            // Ranking is total too, and honors the requested cap.
            prop_assert!(report.top_fixes(3).len() <= 3);
        }
    }

    // ---- Tool-set advisor: total and deterministic ------------------------

    /// The tool-set advisor never panics over an arbitrary tool list and
    /// arbitrary per-tool costs, and is **deterministic**: the same input yields
    /// byte-identical findings in the same order, stably sorted by severity.
    #[test]
    fn advisor_is_total_and_deterministic(
        tools in arb_tools(),
        costs in prop::collection::vec(0usize..5_000, 0..12),
    ) {
        // Attach arbitrary token counts to whatever tools exist; extra costs are
        // ignored and missing ones default to 0 inside the advisor.
        let tool_costs: Vec<ToolTokenCost> = tools
            .iter()
            .zip(costs.iter())
            .map(|(t, &tok)| ToolTokenCost { name: t.name.clone(), tokens: tok })
            .collect();

        let a = advise_tool_set(&tools, &tool_costs);
        let b = advise_tool_set(&tools, &tool_costs);

        // Determinism: identical (severity, message, fix) sequence.
        prop_assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            prop_assert!(x.severity == y.severity);
            prop_assert_eq!(&x.message, &y.message);
            prop_assert_eq!(&x.fix, &y.fix);
        }

        // Stable ordering: severity rank is non-decreasing down the list.
        let rank = |s| match s {
            Severity::High => 0u8,
            Severity::Medium => 1,
            Severity::Low => 2,
            Severity::Info => 3,
        };
        for w in a.windows(2) {
            prop_assert!(rank(w[0].severity) <= rank(w[1].severity));
        }
    }
}

/// A strategy for an arbitrary tool list: names may contain spaces and mixed
/// separators (exercising the naming heuristics), descriptions may be absent,
/// and input schemas are arbitrary JSON.
fn arb_tools() -> impl Strategy<Value = Vec<Tool>> {
    let one = (
        "[a-zA-Z0-9_ -]{0,20}",
        proptest::option::of(".{0,80}"),
        arb_json(),
    )
        .prop_map(|(name, desc, schema)| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(name));
            if let Some(d) = desc {
                m.insert("description".to_string(), json!(d));
            }
            m.insert("inputSchema".to_string(), schema);
            serde_json::from_value::<Tool>(Value::Object(m)).expect("tool with a name parses")
        });
    prop::collection::vec(one, 0..10)
}
