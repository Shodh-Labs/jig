//! Dimension 1: protocol compliance.

use super::util::*;
use super::*;

/// Protocol: points deducted per non-protocol (framing-breaking) stdout line.
pub(super) const PROTOCOL_POLLUTION_PENALTY: f64 = 15.0;
/// Protocol: cap on the total pollution deduction.
const PROTOCOL_POLLUTION_CAP: f64 = 60.0;
/// Protocol: points per capability advertised outside the negotiated spec.
const PROTOCOL_OFFSPEC_CAP_PENALTY: f64 = 10.0;
/// Protocol: cap on the total off-spec-capability deduction.
const PROTOCOL_OFFSPEC_CAP_CAP: f64 = 30.0;
/// Protocol: deduction when a list operation timed out (server accepted the
/// request but never answered).
const PROTOCOL_LIST_TIMEOUT_PENALTY: f64 = 40.0;
/// Protocol: deduction per tool whose name violates the MCP name format
/// (conformance scenario `tools-name-format`, SEP-986).
const PROTOCOL_TOOL_NAME_FORMAT_PENALTY: f64 = 8.0;
/// Protocol: cap on the total tool-name-format deduction.
const PROTOCOL_TOOL_NAME_FORMAT_CAP: f64 = 24.0;
/// Protocol: deduction per missing/empty required `initialize` result field
/// (conformance scenario `server-initialize`, MCP-Initialize).
const PROTOCOL_INIT_FIELD_PENALTY: f64 = 10.0;
/// Protocol: deduction when the server answers an unknown method with a
/// non-standard JSON-RPC error code (conformance scenario `negative`).
const PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY: f64 = 10.0;
/// Protocol: deduction when the server *accepts* an unknown method instead of
/// rejecting it with `-32601` (conformance scenario `negative`).
const PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY: f64 = 20.0;

/// The JSON-RPC 2.0 "Method not found" error code every MCP server must return
/// for a method it does not implement (JSON-RPC 2.0 §5.1).
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// The maximum length (characters) of a legal MCP tool name (SEP-986).
const TOOL_NAME_MAX_LEN: usize = 64;

/// How many leading bytes of a polluting line to quote in the fix text.
const POLLUTION_EXCERPT_BYTES: usize = 24;

// -- Rate-based dimension scoring (rubric-v1.1) ------------------------------

pub(super) fn score_protocol(input: &CheckInput) -> DimensionScore {
    let mut score = 100.0;
    let mut findings = Vec::new();

    // Stdout pollution: the single most common real-world MCP break. Pinned into
    // Top Fixes because it stops real clients working regardless of its score.
    if input.observations.pollution_lines > 0 {
        let n = input.observations.pollution_lines;
        let raw = PROTOCOL_POLLUTION_PENALTY * n as f64;
        let points = raw.min(PROTOCOL_POLLUTION_CAP);
        score -= points;
        let (message, fix) = pollution_finding_text(n, input.observations.first_pollution.as_ref());
        findings.push(Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolStdoutPollution,
            severity: Severity::High,
            message,
            fix,
            points,
            rank_points: None,
            pinned: true,
        });
    }

    // Capabilities advertised outside the *negotiated* spec revision. Legality
    // is version-relative (see `REVISIONS`): the same capability is clean under
    // a revision that defines it and off-spec under one that does not.
    let (revision, assumed_latest) = match revision_for(&input.protocol_version) {
        Some(r) => (r, false),
        None => (latest_revision(), true),
    };
    let offspec = offspec_capabilities(&input.capabilities, revision);
    if !offspec.is_empty() {
        let raw = PROTOCOL_OFFSPEC_CAP_PENALTY * offspec.len() as f64;
        let points = raw.min(PROTOCOL_OFFSPEC_CAP_CAP);
        score -= points;
        let (message, fix) = offspec_finding_text(&offspec, revision, assumed_latest);
        findings.push(Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolOffspecCapability,
            severity: Severity::Medium,
            message,
            fix,
            points,
            rank_points: None,
            pinned: false,
        });
    }

    // Conformance `server-initialize` (MCP-Initialize): the initialize result
    // MUST carry a non-empty serverInfo (name + version) and an object
    // capabilities map. serde already requires the fields to be present; here we
    // catch the present-but-empty / wrong-shape cases a live server can still
    // send.
    let init_gaps = initialize_field_gaps(input);
    if !init_gaps.is_empty() {
        let points = PROTOCOL_INIT_FIELD_PENALTY * init_gaps.len() as f64;
        score -= points;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolInitializeFieldInvalid,
            severity: Severity::High,
            message: format!(
                "initialize result has {} (conformance: server-initialize)",
                join_and(&init_gaps)
            ),
            fix: "return a spec-valid initialize result: a non-empty serverInfo.name and \
                  serverInfo.version, and a capabilities object"
                .to_string(),
            points,
            rank_points: None,
            pinned: false,
        });
    }

    // Conformance `tools-name-format` (SEP-986): every tool name must be 1..=64
    // chars and match `^[A-Za-z0-9_./-]+$`. A malformed name is uncallable.
    let bad_names = tool_name_format_violations(&input.tools);
    if !bad_names.is_empty() {
        let raw = PROTOCOL_TOOL_NAME_FORMAT_PENALTY * bad_names.len() as f64;
        let points = raw.min(PROTOCOL_TOOL_NAME_FORMAT_CAP);
        score -= points;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolToolNameFormat,
            severity: Severity::High,
            message: format!(
                "tool name{} {} violate MCP name format (conformance: tools-name-format, SEP-986)",
                plural(bad_names.len()),
                join_violations(&bad_names)
            ),
            fix: "rename to 1–64 chars matching ^[A-Za-z0-9_./-]+$ (no spaces or other symbols)"
                .to_string(),
            points,
            rank_points: None,
            pinned: false,
        });
    }

    // Conformance `negative`: an unknown method must be rejected with the
    // JSON-RPC `-32601 Method not found` code, never a different code or a
    // spurious success.
    match input.observations.unknown_method {
        UnknownMethodProbe::Errored(code) if code != JSONRPC_METHOD_NOT_FOUND => {
            score -= PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY;
            findings.push(Finding {
                dimension: Dimension::Protocol,
                code: FindingCode::ProtocolUnknownMethodWrongCode,
                severity: Severity::Medium,
                message: format!(
                    "unknown method answered with JSON-RPC error {code}, not {JSONRPC_METHOD_NOT_FOUND} \
                     Method not found (conformance: negative)"
                ),
                fix: format!(
                    "return error code {JSONRPC_METHOD_NOT_FOUND} for methods the server does not implement"
                ),
                points: PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY,
                rank_points: None,
                pinned: false,
            });
        }
        UnknownMethodProbe::Accepted => {
            score -= PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY;
            findings.push(Finding {
                dimension: Dimension::Protocol,
                code: FindingCode::ProtocolUnknownMethodAccepted,
                severity: Severity::High,
                message: "server returned a success result for an unknown method instead of \
                          -32601 Method not found (conformance: negative)"
                    .to_string(),
                fix: format!(
                    "reject unimplemented methods with JSON-RPC error {JSONRPC_METHOD_NOT_FOUND}"
                ),
                points: PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY,
                rank_points: None,
                pinned: false,
            });
        }
        // Conformant (-32601), inconclusive (no answer), or not probed.
        _ => {}
    }

    // A list operation the server accepted but never answered.
    if input.observations.list_timed_out {
        score -= PROTOCOL_LIST_TIMEOUT_PENALTY;
        findings.push(Finding {
            dimension: Dimension::Protocol,
            code: FindingCode::ProtocolListTimeout,
            severity: Severity::High,
            message: "a list operation timed out — the server accepted the request but never \
                      responded"
                .to_string(),
            fix: "ensure every request receives a response; check for a hang in the list handler"
                .to_string(),
            points: PROTOCOL_LIST_TIMEOUT_PENALTY,
            rank_points: None,
            pinned: false,
        });
    }

    let score = clamp_score(score);
    let summary = summarize_findings(
        &findings,
        "clean handshake, no stdout pollution, spec-valid capabilities",
    );
    DimensionScore {
        dimension: Dimension::Protocol,
        score: Some(score),
        weight: Dimension::Protocol.weight(),
        summary,
        heuristic: false,
        findings,
    }
}

/// One off-spec capability: its key and the earliest known revision that
/// defines it (so a finding can point to where it *is* standardized).
struct OffSpecCap {
    /// The advertised capability key.
    name: String,
    /// The earliest known revision defining this key, if any known revision does.
    introduced_in: Option<&'static str>,
}

/// Top-level capability keys advertised outside the negotiated `revision`.
fn offspec_capabilities(caps: &Value, revision: &Revision) -> Vec<OffSpecCap> {
    let Some(map) = caps.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<OffSpecCap> = map
        .keys()
        .filter(|k| !revision.capabilities.contains(&k.as_str()))
        .map(|k| OffSpecCap {
            name: k.clone(),
            introduced_in: capability_introduced_in(k),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Build the (message, fix) for an off-spec-capability finding, naming the
/// negotiated revision and, per capability, where it is first defined.
fn offspec_finding_text(
    offspec: &[OffSpecCap],
    revision: &Revision,
    assumed_latest: bool,
) -> (String, String) {
    let clauses: Vec<String> = offspec
        .iter()
        .map(|c| match c.introduced_in {
            Some(rev) => format!("`{}` (first defined in revision {rev})", c.name),
            None => format!("`{}` (not defined in any known MCP revision)", c.name),
        })
        .collect();
    let assumed = if assumed_latest {
        format!(
            " — negotiated version is unknown to jig, validated against the latest known revision {}",
            revision.id
        )
    } else {
        String::new()
    };
    let message = format!(
        "capability {} not defined in the negotiated MCP revision {}{}",
        clauses.join(", "),
        revision.id,
        assumed
    );
    let fix = "gate off-spec capabilities on the negotiated protocol version, or negotiate a \
               revision that defines them"
        .to_string();
    (message, fix)
}

/// Build the (message, fix) for a stdout-pollution finding, enriched with the
/// exact byte offset and a hex/utf8 excerpt of the first polluting line.
fn pollution_finding_text(n: usize, site: Option<&PollutionSite>) -> (String, String) {
    let message = format!(
        "{n} non-protocol line(s) on stdout — this corrupts MCP's newline-delimited framing"
    );
    let fix = match site {
        Some(site) => {
            let (utf8, hex) = pollution_excerpt(&site.line);
            let at = match site.offset {
                Some(off) => format!("at byte offset {off}"),
                None => "on stdout".to_string(),
            };
            format!(
                "route all logging to stderr; the first polluting line is {at}: \"{utf8}\" \
                 (hex {hex}) — stdout must carry only newline-delimited JSON-RPC"
            )
        }
        None => "route all logging to stderr; stdout must carry only newline-delimited JSON-RPC"
            .to_string(),
    };
    (message, fix)
}

/// A short utf8 + hex excerpt of a polluting line's leading bytes.
fn pollution_excerpt(line: &str) -> (String, String) {
    let bytes = line.as_bytes();
    let take = bytes.len().min(POLLUTION_EXCERPT_BYTES);
    let hex = bytes[..take]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let ellipsis = if bytes.len() > take { "…" } else { "" };
    let utf8: String = line.chars().take(POLLUTION_EXCERPT_BYTES).collect();
    (format!("{utf8}{ellipsis}"), format!("{hex}{ellipsis}"))
}

/// Missing/empty required `initialize` result fields (conformance:
/// server-initialize). Names the concrete gap so the fix is actionable.
fn initialize_field_gaps(input: &CheckInput) -> Vec<String> {
    let mut gaps = Vec::new();
    if input.server_name.trim().is_empty() {
        gaps.push("an empty serverInfo.name".to_string());
    }
    if input.server_version.trim().is_empty() {
        gaps.push("an empty serverInfo.version".to_string());
    }
    // Absent capabilities deserialize to JSON null here; the spec requires an
    // object. A null/array/scalar capabilities value is a shape violation.
    if !input.capabilities.is_object() {
        gaps.push("a non-object capabilities value".to_string());
    }
    gaps
}

/// Join phrases with commas and a trailing "and": `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Tool names that violate the MCP name format (SEP-986): each returned as
/// `(name, reason)`.
fn tool_name_format_violations(tools: &[Tool]) -> Vec<(String, String)> {
    tools
        .iter()
        .filter_map(|t| tool_name_format_reason(&t.name).map(|why| (t.name.clone(), why)))
        .collect()
}

/// The reason `name` violates the MCP tool-name format, or `None` if it is
/// legal: 1..=64 characters, each in `[A-Za-z0-9_./-]`.
fn tool_name_format_reason(name: &str) -> Option<String> {
    let len = name.chars().count();
    if len == 0 {
        return Some("is empty".to_string());
    }
    if len > TOOL_NAME_MAX_LEN {
        return Some(format!("is {len} chars (max {TOOL_NAME_MAX_LEN})"));
    }
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
    if !name.chars().all(legal) {
        return Some("has characters outside [A-Za-z0-9_./-]".to_string());
    }
    None
}

/// Join `(name, reason)` violations as `` `name` (reason) ``, comma-separated.
fn join_violations(v: &[(String, String)]) -> String {
    v.iter()
        .map(|(n, why)| format!("`{n}` {why}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::testkit::*;
    use serde_json::json;

    #[test]
    fn pollution_deducts_from_protocol_with_finding() {
        let mut input = clean_input();
        input.observations.pollution_lines = 1;
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(85.0));
        assert!(p.findings.iter().any(|f| f.message.contains("stdout")));
        assert_eq!(p.findings[0].severity, Severity::High);
    }

    #[test]
    fn pollution_penalty_is_capped() {
        let mut input = clean_input();
        input.observations.pollution_lines = 100;
        let report = evaluate(&input, None);
        // 100 * 15 caps at 60 → score 40, not negative.
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(40.0)
        );
    }

    #[test]
    fn offspec_capability_is_flagged() {
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "tasks": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(90.0));
        assert!(p.findings.iter().any(|f| f.message.contains("tasks")));
    }

    #[test]
    fn same_capability_graded_by_negotiated_revision() {
        // `completions`: legal from 2025-03-26, off-spec under 2024-11-05.
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "completions": {} });

        input.protocol_version = "2025-06-18".to_string();
        let clean = evaluate(&input, None);
        assert_eq!(
            clean.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0),
            "completions is in-spec for 2025-06-18"
        );

        input.protocol_version = "2024-11-05".to_string();
        let flagged = evaluate(&input, None);
        let p = flagged.dimension(Dimension::Protocol).unwrap();
        assert_eq!(
            p.score,
            Some(90.0),
            "completions is off-spec for 2024-11-05"
        );
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("completions") && f.message.contains("2024-11-05")));
    }

    #[test]
    fn tasks_off_spec_under_2025_06_18_but_clean_under_2025_11_25() {
        let mut input = clean_input();
        input.capabilities = json!({ "tools": {}, "tasks": {} });

        input.protocol_version = "2025-06-18".to_string();
        let flagged = evaluate(&input, None);
        let p = flagged.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(90.0));
        // The finding cites where `tasks` is actually first defined.
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("tasks") && f.message.contains("2025-11-25")));

        input.protocol_version = "2025-11-25".to_string();
        let clean = evaluate(&input, None);
        assert_eq!(
            clean.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0),
            "tasks is defined in 2025-11-25"
        );
    }

    #[test]
    fn unknown_revision_validates_against_latest_and_notes_assumption() {
        let mut input = clean_input();
        input.protocol_version = "2099-01-01".to_string();
        // `extensions` is defined only in the latest known revision (2026-07-28).
        input.capabilities = json!({ "tools": {}, "extensions": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        // extensions is legal under the latest revision → no off-spec finding.
        assert_eq!(p.score, Some(100.0));

        // But `tasks` (not top-level in the latest revision) is still flagged,
        // and the finding notes the unknown-version assumption.
        input.capabilities = json!({ "tools": {}, "tasks": {} });
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert!(p.findings.iter().any(|f| {
            f.message.contains("tasks")
                && f.message.contains("unknown to jig")
                && f.message.contains("2026-07-28")
        }));
    }

    #[test]
    fn malformed_tool_name_flagged_as_conformance_violation() {
        let input = CheckInput {
            tools: vec![tool(
                "bad name!",
                Some("a reasonably sized tool description here"),
                json!({ "type": "object", "properties": {} }),
            )],
            ..clean_input()
        };
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(p.score, Some(100.0 - PROTOCOL_TOOL_NAME_FORMAT_PENALTY));
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("tools-name-format") && f.message.contains("SEP-986")));
    }

    #[test]
    fn overlong_tool_name_flagged() {
        let long = "a".repeat(65);
        assert!(tool_name_format_reason(&long).is_some());
        assert!(tool_name_format_reason("get_user").is_none());
        assert!(tool_name_format_reason("get.user/v2-final").is_none());
        assert!(tool_name_format_reason("").is_some());
    }

    #[test]
    fn empty_initialize_fields_flagged() {
        let mut input = clean_input();
        input.server_name = "  ".to_string();
        input.capabilities = json!([]); // not an object
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        // Two gaps × 10 each = 20.
        assert_eq!(p.score, Some(80.0));
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("server-initialize")));
    }

    #[test]
    fn unknown_method_wrong_code_and_accepted_are_flagged() {
        // Wrong error code.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Errored(-32000);
        let report = evaluate(&input, None);
        let p = report.dimension(Dimension::Protocol).unwrap();
        assert_eq!(
            p.score,
            Some(100.0 - PROTOCOL_UNKNOWN_METHOD_WRONG_CODE_PENALTY)
        );
        assert!(p
            .findings
            .iter()
            .any(|f| f.message.contains("negative") && f.message.contains("-32601")));

        // Accepted an unknown method outright.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Accepted;
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0 - PROTOCOL_UNKNOWN_METHOD_ACCEPTED_PENALTY)
        );

        // A conformant -32601 is clean.
        let mut input = clean_input();
        input.observations.unknown_method = UnknownMethodProbe::Errored(-32601);
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(100.0)
        );
    }

    #[test]
    fn pollution_fix_names_byte_offset_and_excerpt() {
        let mut input = clean_input();
        input.observations.pollution_lines = 1;
        input.observations.first_pollution = Some(PollutionSite {
            offset: Some(42),
            line: "[info] started".to_string(),
        });
        let report = evaluate(&input, None);
        let f = report
            .dimension(Dimension::Protocol)
            .unwrap()
            .findings
            .iter()
            .find(|f| f.message.contains("non-protocol line"))
            .unwrap();
        assert!(f.fix.contains("byte offset 42"), "fix: {}", f.fix);
        assert!(f.fix.contains("[info] started"), "fix: {}", f.fix);
        // Hex excerpt of the first bytes ('[' == 0x5b).
        assert!(f.fix.contains("5b"), "fix: {}", f.fix);
    }

    #[test]
    fn pollution_is_pinned_into_top_fixes_even_when_outranked() {
        // A server with heavy context cost + several broken tools whose findings
        // each outrank the single-line pollution deduction by weighted impact.
        let mut input = clean_input();
        input.observations.pollution_lines = 1; // protocol -15 (×25 = 375)
        let big = "lorem ipsum dolor sit amet ".repeat(4000);
        input.tools = vec![
            tool(
                "giant",
                Some(big.trim()),
                json!({ "type": "object", "properties": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {}, "f": {}
                } }),
            ),
            tool(
                "second",
                Some("another tool here for context"),
                json!({ "type": "object", "properties": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {}, "f": {}
                } }),
            ),
        ];
        let report = evaluate(&input, None);
        let fixes = report.top_fixes(3);
        assert!(
            fixes
                .iter()
                .any(|f| f.pinned && f.message.contains("stdout")),
            "pollution must be pinned into the top fixes: {:?}",
            fixes.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn list_timeout_deducts_from_protocol() {
        let mut input = clean_input();
        input.observations.list_timed_out = true;
        let report = evaluate(&input, None);
        assert_eq!(
            report.dimension(Dimension::Protocol).unwrap().score,
            Some(60.0)
        );
    }
}
