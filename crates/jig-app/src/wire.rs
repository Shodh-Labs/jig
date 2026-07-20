//! The Wire pane's model: turning a flat `ProtocolTap` log into request/response
//! spans on a folded time axis.
//!
//! `jig-core`'s [`TapEntry`] is deliberately dumb — a sequence number, a
//! direction, an elapsed-microseconds stamp, and the verbatim JSON-RPC message.
//! It carries no request/response correlation and no duration, because the tap's
//! job is to record the wire exactly, not to interpret it. The interpretation
//! lives here, and it is pure: [`build_spans`] and [`build_axis`] are ordinary
//! functions over slices, so the whole centrepiece of the app is testable
//! without a webview, a server, or a tokio runtime.
//!
//! [`TapEntry`]: jig_core::TapEntry

use jig_core::{Direction, TapEntry};
use serde::Serialize;
use serde_json::Value;

/// What a row on the timeline actually is.
///
/// The tap does not tag these — the shape of the JSON-RPC message does. A
/// message with a `method` is a call; whether it also has an `id` is what
/// separates a request (expects a reply) from a notification (does not). The
/// direction then says who spoke. Server-push notifications are the interesting
/// case the design prototype calls out, so they get their own variant rather
/// than being folded in with our own outbound notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// `jig -> server`, has an `id`: a request awaiting a response.
    Request,
    /// `jig -> server`, no `id`: fire-and-forget (e.g. `notifications/initialized`).
    ClientNotification,
    /// `server -> jig`, no `id`: an *unsolicited* server push. The diamond.
    ServerNotification,
    /// `server -> jig`, has an `id`: the server calling us (e.g. `sampling/createMessage`).
    ServerRequest,
    /// Inbound bytes that were not a JSON object at all — stdout pollution.
    /// This is the single most common reason a real MCP server breaks a client,
    /// so it is shown on the wire rather than silently dropped.
    Pollution,
}

/// One row on the timeline.
///
/// `end_micros`/`duration_micros` are `Some` only for a [`SpanKind::Request`]
/// that actually received its response — an unanswered request stays open, and
/// the UI draws it as such rather than inventing an end time.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSpan {
    /// The tap sequence number of the message that opened this span. Stable
    /// across polls, so the UI uses it as the row key and selection identity.
    pub seq: u64,
    pub kind: SpanKind,
    pub method: Option<String>,
    /// The JSON-RPC `id`, verbatim (it may legitimately be a string or a number).
    pub id: Option<Value>,
    pub start_micros: u64,
    pub end_micros: Option<u64>,
    pub duration_micros: Option<u64>,
    /// The opening message, verbatim, for the inspector.
    pub request: Option<Value>,
    /// The closing message, verbatim, for the inspector. `None` while pending.
    pub response: Option<Value>,
    /// True when the response carried a JSON-RPC `error` member.
    pub is_error: bool,
    /// True for a request that never got a reply within the captured log.
    pub pending: bool,
    /// Byte offset in the server's stdout, when the transport knows it. Present
    /// for inbound stdio only — it is what lets a pollution finding point at the
    /// exact break in the stream.
    pub offset: Option<u64>,
}

/// Is this message a JSON-RPC response (no `method`, but a `result` or `error`)?
fn is_response(message: &Value) -> bool {
    message.is_object()
        && message.get("method").is_none()
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// Two JSON-RPC ids match if they are equal *as JSON values*. The spec permits
/// string or number ids, and `1` and `"1"` are different ids — comparing the
/// `Value`s directly is exactly right and avoids a lossy stringification.
fn ids_match(a: &Value, b: &Value) -> bool {
    a == b
}

/// Correlate a flat tap log into spans.
///
/// Requests are matched to responses by `id`, scanning forward from the request.
/// The first unconsumed response with a matching id wins, which is correct
/// because the protocol forbids reusing an id while a request is in flight.
/// Responses that match no request are dropped rather than shown as orphans:
/// in practice they only occur when the log was truncated at the front, and a
/// row with no request to explain it is noise, not information.
pub fn build_spans(entries: &[TapEntry]) -> Vec<WireSpan> {
    // Which entry indices have already been consumed as somebody's response.
    let mut consumed = vec![false; entries.len()];
    let mut spans = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        let msg = &entry.message;

        // Non-object inbound: stdout pollution, not protocol.
        if !msg.is_object() {
            if entry.direction == Direction::Inbound {
                spans.push(WireSpan {
                    seq: entry.seq,
                    kind: SpanKind::Pollution,
                    method: None,
                    id: None,
                    start_micros: entry.elapsed_micros,
                    end_micros: None,
                    duration_micros: None,
                    request: None,
                    response: Some(msg.clone()),
                    is_error: true,
                    pending: false,
                    offset: entry.offset,
                });
            }
            continue;
        }

        // A response with no request ahead of it — skip (see fn doc).
        if is_response(msg) {
            continue;
        }

        let method = entry.method().map(str::to_string);
        let id = msg.get("id").cloned();
        let has_id = id.is_some();

        let kind = match (entry.direction, has_id) {
            (Direction::Outbound, true) => SpanKind::Request,
            (Direction::Outbound, false) => SpanKind::ClientNotification,
            (Direction::Inbound, true) => SpanKind::ServerRequest,
            (Direction::Inbound, false) => SpanKind::ServerNotification,
        };

        // Only a call with an id can be closed by a response. Look for the
        // reply travelling the other way.
        let mut end = None;
        let mut response = None;
        let mut is_error = false;
        if let Some(ref want) = id {
            let want_dir = match entry.direction {
                Direction::Outbound => Direction::Inbound,
                Direction::Inbound => Direction::Outbound,
            };
            for (j, cand) in entries.iter().enumerate().skip(i + 1) {
                if consumed[j] || cand.direction != want_dir || !is_response(&cand.message) {
                    continue;
                }
                if cand
                    .message
                    .get("id")
                    .is_some_and(|got| ids_match(got, want))
                {
                    consumed[j] = true;
                    end = Some(cand.elapsed_micros);
                    is_error = cand.message.get("error").is_some();
                    response = Some(cand.message.clone());
                    break;
                }
            }
        }

        let pending = has_id && end.is_none();
        spans.push(WireSpan {
            seq: entry.seq,
            kind,
            method,
            id,
            start_micros: entry.elapsed_micros,
            // A span's end is saturating: the tap's clock is monotonic, so this
            // can only bite if a log were hand-edited, but a negative duration
            // would corrupt the axis, so clamp rather than trust.
            end_micros: end,
            duration_micros: end.map(|e| e.saturating_sub(entry.elapsed_micros)),
            request: Some(msg.clone()),
            response,
            is_error,
            pending,
            offset: entry.offset,
        });
    }

    spans
}

/// One stretch of the time axis.
///
/// The axis is piecewise-linear: quiet stretches are *folded* — drawn at a fixed
/// small width instead of to scale — so an 8-second `npx` cold start cannot
/// squash the 7-millisecond `tools/list` it precedes into a single pixel. This
/// is the design prototype's `⟨fold: 8.0 s⟩` marker, and it is the difference
/// between a timeline you can read and one you cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisSegment {
    pub start_micros: u64,
    pub end_micros: u64,
    /// True when this stretch is elided rather than drawn to scale.
    pub folded: bool,
}

impl AxisSegment {
    /// Duration this segment covers in real time.
    pub fn duration_micros(&self) -> u64 {
        self.end_micros.saturating_sub(self.start_micros)
    }
}

/// The fraction of the axis width a single folded stretch is given, regardless
/// of how much real time it hides. Small enough to read as a break, wide enough
/// to carry its `⟨fold: N⟩` label.
pub const FOLD_WIDTH_FRACTION: f64 = 0.06;

/// Build a folded time axis over a set of spans.
///
/// Any interval longer than `fold_threshold_micros` during which no span starts
/// or ends is folded. Returns segments in ascending time order, together
/// covering exactly the span of the session.
pub fn build_axis(spans: &[WireSpan], fold_threshold_micros: u64) -> Vec<AxisSegment> {
    if spans.is_empty() {
        return Vec::new();
    }

    // Every instant at which something observable happens.
    let mut marks: Vec<u64> = Vec::with_capacity(spans.len() * 2);
    for s in spans {
        marks.push(s.start_micros);
        if let Some(e) = s.end_micros {
            marks.push(e);
        }
    }
    marks.sort_unstable();
    marks.dedup();

    let first = *marks.first().expect("spans is non-empty");
    let last = *marks.last().expect("spans is non-empty");
    if first == last {
        // A single instant: one degenerate, unfolded segment. Callers divide by
        // the total drawn width, so this must not be empty.
        return vec![AxisSegment {
            start_micros: first,
            end_micros: last,
            folded: false,
        }];
    }

    let mut segments: Vec<AxisSegment> = Vec::new();
    for pair in marks.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let folded = b - a > fold_threshold_micros;
        // Merge into the previous segment when the fold state is unchanged, so
        // the axis is a minimal set of alternating drawn/folded runs.
        match segments.last_mut() {
            Some(prev) if prev.folded == folded => prev.end_micros = b,
            _ => segments.push(AxisSegment {
                start_micros: a,
                end_micros: b,
                folded,
            }),
        }
    }
    segments
}

/// Project a timestamp onto the axis, returning a position in `0.0..=1.0`.
///
/// Unfolded segments are linear in real time and share the axis width in
/// proportion to their duration; each folded segment occupies
/// [`FOLD_WIDTH_FRACTION`] of the total regardless of the time it hides. A
/// timestamp inside a folded stretch maps linearly within that fixed width, so
/// ordering is always preserved even where scale is not.
pub fn project(axis: &[AxisSegment], t: u64) -> f64 {
    if axis.is_empty() {
        return 0.0;
    }
    // Total drawn width in arbitrary units: real microseconds for drawn
    // segments, a synthetic constant for folded ones.
    let drawn_total: f64 = axis
        .iter()
        .map(|s| {
            if s.folded {
                0.0
            } else {
                s.duration_micros() as f64
            }
        })
        .sum();
    let fold_count = axis.iter().filter(|s| s.folded).count() as f64;

    // Folded stretches together claim `fold_count * FOLD_WIDTH_FRACTION` of the
    // axis; the drawn time shares what is left.
    let fold_share = (fold_count * FOLD_WIDTH_FRACTION).min(0.9);
    let drawn_share = 1.0 - fold_share;

    let mut pos = 0.0_f64;
    for seg in axis {
        let seg_width = if seg.folded {
            FOLD_WIDTH_FRACTION.min(fold_share)
        } else if drawn_total > 0.0 {
            drawn_share * (seg.duration_micros() as f64 / drawn_total)
        } else {
            drawn_share / axis.len() as f64
        };

        if t <= seg.start_micros {
            return pos.clamp(0.0, 1.0);
        }
        if t <= seg.end_micros {
            let d = seg.duration_micros();
            let frac = if d == 0 {
                0.0
            } else {
                (t - seg.start_micros) as f64 / d as f64
            };
            return (pos + seg_width * frac).clamp(0.0, 1.0);
        }
        pos += seg_width;
    }
    pos.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, dir: Direction, micros: u64, message: Value) -> TapEntry {
        TapEntry {
            seq,
            direction: dir,
            elapsed_micros: micros,
            offset: None,
            message,
        }
    }

    #[test]
    fn request_and_response_pair_into_one_span_with_a_round_trip() {
        let entries = vec![
            entry(
                0,
                Direction::Outbound,
                1_000,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            ),
            entry(
                1,
                Direction::Inbound,
                8_500,
                json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}}),
            ),
        ];
        let spans = build_spans(&entries);
        assert_eq!(spans.len(), 1, "the pair must collapse to a single row");
        let s = &spans[0];
        assert_eq!(s.kind, SpanKind::Request);
        assert_eq!(s.method.as_deref(), Some("tools/list"));
        assert_eq!(s.duration_micros, Some(7_500));
        assert!(!s.pending);
        assert!(!s.is_error);
        assert!(s.response.is_some(), "the inspector needs the raw response");
    }

    #[test]
    fn server_push_notification_is_distinct_from_our_own_notification() {
        let entries = vec![
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            ),
            entry(
                1,
                Direction::Inbound,
                50,
                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
            ),
        ];
        let spans = build_spans(&entries);
        assert_eq!(spans[0].kind, SpanKind::ClientNotification);
        assert_eq!(spans[1].kind, SpanKind::ServerNotification);
        // Neither has a duration: they are point events, not round trips.
        assert!(spans.iter().all(|s| s.duration_micros.is_none()));
    }

    #[test]
    fn a_server_initiated_request_is_its_own_kind_and_pairs_on_our_reply() {
        let entries = vec![
            entry(
                0,
                Direction::Inbound,
                100,
                json!({"jsonrpc":"2.0","id":"a","method":"ping"}),
            ),
            entry(
                1,
                Direction::Outbound,
                300,
                json!({"jsonrpc":"2.0","id":"a","result":{}}),
            ),
        ];
        let spans = build_spans(&entries);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::ServerRequest);
        assert_eq!(spans[0].duration_micros, Some(200));
    }

    #[test]
    fn an_error_response_closes_the_span_and_is_flagged() {
        let entries = vec![
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"jsonrpc":"2.0","id":7,"method":"tools/call"}),
            ),
            entry(
                1,
                Direction::Inbound,
                900,
                json!({"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}),
            ),
        ];
        let spans = build_spans(&entries);
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].is_error,
            "a JSON-RPC error member must mark the span"
        );
        assert_eq!(spans[0].duration_micros, Some(900));
    }

    #[test]
    fn an_unanswered_request_stays_pending_rather_than_inventing_an_end() {
        let entries = vec![entry(
            0,
            Direction::Outbound,
            0,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )];
        let spans = build_spans(&entries);
        assert!(spans[0].pending);
        assert_eq!(spans[0].end_micros, None);
        assert_eq!(spans[0].duration_micros, None);
    }

    #[test]
    fn string_and_number_ids_are_not_confused() {
        // `1` and `"1"` are different JSON-RPC ids. Pairing must not conflate
        // them, or two concurrent calls would swap responses.
        let entries = vec![
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"jsonrpc":"2.0","id":1,"method":"a"}),
            ),
            entry(
                1,
                Direction::Outbound,
                10,
                json!({"jsonrpc":"2.0","id":"1","method":"b"}),
            ),
            entry(
                2,
                Direction::Inbound,
                100,
                json!({"jsonrpc":"2.0","id":"1","result":{}}),
            ),
            entry(
                3,
                Direction::Inbound,
                200,
                json!({"jsonrpc":"2.0","id":1,"result":{}}),
            ),
        ];
        let spans = build_spans(&entries);
        let a = spans
            .iter()
            .find(|s| s.method.as_deref() == Some("a"))
            .unwrap();
        let b = spans
            .iter()
            .find(|s| s.method.as_deref() == Some("b"))
            .unwrap();
        assert_eq!(a.duration_micros, Some(200), "id 1 (number) closed at 200");
        assert_eq!(
            b.duration_micros,
            Some(90),
            "id \"1\" (string) closed at 100"
        );
    }

    #[test]
    fn interleaved_requests_pair_to_their_own_responses() {
        let entries = vec![
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"jsonrpc":"2.0","id":1,"method":"slow"}),
            ),
            entry(
                1,
                Direction::Outbound,
                10,
                json!({"jsonrpc":"2.0","id":2,"method":"fast"}),
            ),
            entry(
                2,
                Direction::Inbound,
                20,
                json!({"jsonrpc":"2.0","id":2,"result":{}}),
            ),
            entry(
                3,
                Direction::Inbound,
                500,
                json!({"jsonrpc":"2.0","id":1,"result":{}}),
            ),
        ];
        let spans = build_spans(&entries);
        let slow = spans
            .iter()
            .find(|s| s.method.as_deref() == Some("slow"))
            .unwrap();
        let fast = spans
            .iter()
            .find(|s| s.method.as_deref() == Some("fast"))
            .unwrap();
        assert_eq!(slow.duration_micros, Some(500));
        assert_eq!(fast.duration_micros, Some(10));
    }

    #[test]
    fn non_object_inbound_is_surfaced_as_pollution() {
        let entries = vec![entry(
            0,
            Direction::Inbound,
            5,
            json!("Server listening on port 3000"),
        )];
        let spans = build_spans(&entries);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Pollution);
        assert!(spans[0].is_error, "pollution is what breaks real clients");
    }

    #[test]
    fn a_long_quiet_stretch_is_folded_and_short_ones_are_not() {
        let spans = build_spans(&[
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"id":1,"method":"initialize"}),
            ),
            // 8-second npx cold start.
            entry(
                1,
                Direction::Inbound,
                8_000_000,
                json!({"id":1,"result":{}}),
            ),
            entry(
                2,
                Direction::Outbound,
                8_010_000,
                json!({"id":2,"method":"tools/list"}),
            ),
            entry(
                3,
                Direction::Inbound,
                8_017_500,
                json!({"id":2,"result":{}}),
            ),
        ]);
        let axis = build_axis(&spans, 1_000_000);
        assert!(
            axis.iter().any(|s| s.folded),
            "the 8s boot gap must fold: {axis:?}"
        );
        assert!(
            axis.iter().any(|s| !s.folded),
            "the millisecond traffic must stay drawn to scale"
        );
        // The folded stretch is the boot gap, not the fast calls.
        let folded: Vec<_> = axis.iter().filter(|s| s.folded).collect();
        assert_eq!(folded.len(), 1);
        assert!(folded[0].duration_micros() >= 7_900_000);
    }

    #[test]
    fn projection_is_monotonic_and_bounded_across_a_fold() {
        let spans = build_spans(&[
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"id":1,"method":"initialize"}),
            ),
            entry(
                1,
                Direction::Inbound,
                8_000_000,
                json!({"id":1,"result":{}}),
            ),
            entry(
                2,
                Direction::Outbound,
                8_010_000,
                json!({"id":2,"method":"tools/list"}),
            ),
            entry(
                3,
                Direction::Inbound,
                8_017_500,
                json!({"id":2,"result":{}}),
            ),
        ]);
        let axis = build_axis(&spans, 1_000_000);
        let mut last = -1.0_f64;
        for t in [0, 1_000, 4_000_000, 8_000_000, 8_010_000, 8_017_500] {
            let p = project(&axis, t);
            assert!(
                (0.0..=1.0).contains(&p),
                "t={t} projected out of range: {p}"
            );
            assert!(p >= last, "projection went backwards at t={t}");
            last = p;
        }
        assert!(
            project(&axis, 8_017_500) > 0.9,
            "the last event should sit near the right edge"
        );
    }

    #[test]
    fn the_fold_buys_real_width_for_the_fast_traffic() {
        // The whole point: after folding, the 7.5ms tools/list must occupy a
        // readable slice of the axis, not ~0.09% of it as it would linearly.
        let spans = build_spans(&[
            entry(
                0,
                Direction::Outbound,
                0,
                json!({"id":1,"method":"initialize"}),
            ),
            entry(
                1,
                Direction::Inbound,
                8_000_000,
                json!({"id":1,"result":{}}),
            ),
            entry(
                2,
                Direction::Outbound,
                8_010_000,
                json!({"id":2,"method":"tools/list"}),
            ),
            entry(
                3,
                Direction::Inbound,
                8_017_500,
                json!({"id":2,"result":{}}),
            ),
        ]);
        let axis = build_axis(&spans, 1_000_000);
        let width = project(&axis, 8_017_500) - project(&axis, 8_010_000);
        assert!(
            width > 0.20,
            "tools/list should be clearly visible after folding, got {width}"
        );
    }

    #[test]
    fn an_empty_log_yields_no_spans_and_no_axis() {
        assert!(build_spans(&[]).is_empty());
        assert!(build_axis(&[], 1_000).is_empty());
        // And projecting against an empty axis must not panic.
        assert_eq!(project(&[], 42), 0.0);
    }

    #[test]
    fn a_single_instant_axis_is_degenerate_but_safe() {
        let spans = build_spans(&[entry(
            0,
            Direction::Outbound,
            500,
            json!({"method":"notifications/initialized"}),
        )]);
        let axis = build_axis(&spans, 1_000);
        assert_eq!(axis.len(), 1);
        let p = project(&axis, 500);
        assert!((0.0..=1.0).contains(&p));
    }
}
