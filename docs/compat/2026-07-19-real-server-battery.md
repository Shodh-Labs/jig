# Jig real-server compatibility battery — 2026-07-19

**Milestone:** M2c — validate `jig` against real public MCP servers and harden it against what they do.
**Environment:** Windows 11, Node.js v22.16.0, npm/npx 11.4.2, Rust 1.94.0, `jig` @ 0.1.0.
**Client proposes protocol version:** `2025-06-18`.

Jig is a diagnostic tool. The guiding principle for this exercise: **any crash, hang,
panic, or garbled output caused by a real server is a bug in jig, never "the server's
fault" from the user's seat.** Jig must degrade informatively.

All six servers below were driven with the real `jig` binary over stdio, spawned exactly
as a user would (`jig inspect --stdio "npx -y <pkg>"`). Every session's raw protocol
traffic was captured to a JSONL tap and analyzed.

---

## Compatibility matrix

| Server | Author | Handshake | Protocol | Tools | Res | Prompts | `jig call` verified | Notes |
|---|---|:--:|---|:--:|:--:|:--:|---|---|
| `@modelcontextprotocol/server-everything` | Anthropic | ✅ | 2025-06-18 | 13 | 7 | 4 | `echo`, `get-sum` → ok | Interleaved `notifications/tools/list_changed`; advertises `tasks` capability (not in 2025-06-18 spec) |
| `@modelcontextprotocol/server-filesystem` | Anthropic | ✅ | 2025-06-18 | 14 | 0 | 0 | `list_allowed_directories` → ok | Takes a directory CLI arg; returns `structuredContent` |
| `@modelcontextprotocol/server-memory` | Anthropic | ✅ | 2025-06-18 | 9 | 1 | 0 | — | Multi-paragraph tool descriptions (embedded newlines) |
| `@modelcontextprotocol/server-sequential-thinking` | Anthropic | ✅ | 2025-06-18 | 1 | 0 | 0 | — | Embedded newlines in the single tool's description |
| `@playwright/mcp` | Microsoft | ✅ | 2025-06-18 | 24 | 0 | 0 | — | Largest tool surface; clean framing |
| `@upstash/context7-mcp` | Upstash | ✅ | 2025-06-18 | 2 | 0 | 0 | — | Advertises `prompts` + `resources` but lists 0 of each |

Legend: ✅ handshake succeeded and all advertised lists were retrieved. "Res" = resources.

**Headline:** 6/6 servers handshook successfully, all negotiated `2025-06-18`, zero hangs,
zero panics, zero non-JSON pollution observed on the wire. Two Anthropic servers exposed a
**rendering** bug in jig (embedded newlines); the battery also motivated three proactive
hardening fixes for failure modes real servers *will* eventually hit. All fixed. See below.

---

## Per-server notes

### 1. `@modelcontextprotocol/server-everything` (Anthropic) — the kitchen sink
- **serverInfo:** `mcp-servers/everything v2.0.0`
- **Capabilities:** `completions, logging, prompts(listChanged), resources(listChanged,subscribe), tasks, tools(listChanged)`
- 13 tools, 7 resources (`demo://resource/...`), 4 prompts. Instructions field present (multi-paragraph markdown).
- **Odd in the tap:** a `notifications/tools/list_changed` notification arrived **interleaved between**
  our `tools/list` request (id 2) and its response — i.e. the server pushed a notification while a
  request was in flight. Jig's transport correctly ignored it at the routing layer (no id → not a
  response) while still recording it in the tap, and matched the real response to the pending request.
- **`tasks`** is advertised as a capability but is **not part of the 2025-06-18 spec** (see Spec
  observations). Jig keeps capabilities as raw JSON, so it neither rejects nor chokes on it.
- The npm package's launcher prints a banner to **stdout** when run with `--help` or a non-stdio
  transport, but in default stdio mode it does **not** pollute stdout — the tap shows 0 non-protocol
  lines. Good citizen at runtime.
- **`jig call` results:** `echo {"message":"hello from jig"}` → "Echo: hello from jig";
  `get-sum {"a":2,"b":40}` → "The sum of 2 and 40 is 42." Both exit 0.

### 2. `@modelcontextprotocol/server-filesystem` (Anthropic) — takes CLI args
- **serverInfo:** `secure-filesystem-server v0.2.0`; **Capabilities:** `tools(listChanged)`.
- Spawned as `npx -y @modelcontextprotocol/server-filesystem "<temp dir>"`; the directory argument
  is passed through jig's `--stdio` command splitter (double-quoted to survive the space in the path).
- 14 tools, incl. deprecated `read_file` (description literally says "DEPRECATED: Use read_text_file").
- **`jig call` `list_allowed_directories`** → ok, returning both a text content block **and** a
  `structuredContent` object (2025-06-18 structured content). Jig renders both. Interesting: the
  server reported the temp dir under **two** path spellings — the 8.3 short form
  (`VARUNS~1`) and the long form (`Varun Sharma`) — a Windows path-normalization detail, not a jig issue.

### 3. `@modelcontextprotocol/server-memory` (Anthropic)
- **serverInfo:** `memory-server v0.6.3`; **Capabilities:** `resources(listChanged,subscribe), tools(listChanged)`.
- 9 knowledge-graph tools, 1 resource (`memory://knowledge-graph`).
- **Triggered the embedded-newline rendering bug** (Bug #3): tool descriptions span multiple
  paragraphs; before the fix, the second paragraph broke out of the report's indented cell.

### 4. `@modelcontextprotocol/server-sequential-thinking` (Anthropic)
- **serverInfo:** `sequential-thinking-server v0.2.0`; **Capabilities:** `tools(listChanged)`.
- Exactly 1 tool (`Sequential Thinking`) with a 9-property input schema and a description that
  contains a newline right after the first sentence — the clearest reproduction of Bug #3.

### 5. `@playwright/mcp` (Microsoft) — third-party #1
- **serverInfo:** `Playwright v1.62.0-alpha-1783623505000`; **Capabilities:** `tools`.
- 24 tools — the broadest surface in the battery — including `browser_run_code_unsafe` (whose own
  description flags it as unsafe). Clean framing, no stdout noise.
- Note: an early run showed `EXIT 101`, which turned out to be a **broken-pipe panic caused by piping
  jig into `head`** (head closed the pipe early), *not* a server problem. Re-running without `head`
  gave exit 0. Flagged as an open question below.

### 6. `@upstash/context7-mcp` (Upstash) — third-party #2
- **serverInfo:** `Context7 v3.2.4`; **Capabilities:** `prompts, resources, tools(listChanged)`.
- 2 tools (`resolve-library-id`, `get-library-docs`), plus an `instructions` field.
- **Spec quirk:** advertises the `prompts` and `resources` capabilities, but `prompts/list` and
  `resources/list` both return **empty arrays**. Legal per spec (a capability means "supported", not
  "non-empty"), but potentially misleading. Jig handles it gracefully — the capability gate lets the
  calls through, and empty results render as "(none advertised)".

---

## Bugs found in jig — and fixed

Every item below is fixed with a regression test and is green. Test count: **25 → 34**.

### Bug #1 — No request timeout: a silent server hangs jig forever *(director review finding; fixed pre-battery)*
- **Symptom:** if a server accepted a request and never answered, jig awaited the response channel
  indefinitely. A hung server = a hung tool.
- **Fix:** a default **30s per-request timeout** in the stdio transport, bounding every request
  including the `initialize` handshake. New `JigError::Timeout { method, elapsed }` variant names the
  stalled method and how long jig waited. Surfaced as `--timeout <seconds>` on both `inspect` and
  `call` (`0` = wait forever). On timeout, the pending request slot is cleaned up so a late reply is
  dropped cleanly.
- **Verified live:** `jig inspect --stdio "npx -y <cold pkg>" --timeout 1` →
  `jig: error: failed to connect: request 'initialize' timed out after 1s with no response`.
- **Tests:** `request_that_gets_no_response_times_out` (mock `test/hang` method that accepts but never
  answers) and `no_timeout_still_completes_normal_requests` (`None` path still works).

### Bug #2 — Windows `npx`/`npm` spawn failure *(fixed pre-battery)*
- **Symptom:** `Command::new("npx")` fails with "program not found" on Windows. `npx`/`npm`/`yarn`/`pnpm`
  are `.cmd` shims and `CreateProcess` does **not** consult `PATHEXT` the way the shell does — it
  looks for a file named literally `npx`, which does not exist. This breaks essentially every
  real-world `jig inspect --stdio "npx ..."`, the exact command in this battery.
- **Fix:** `resolve_program()` reproduces the shell's resolution on Windows — for a bare name with no
  directory component and no extension, it walks `PATH` × `PATHEXT` and returns the first hit
  (identity function on non-Windows). Modern Rust (≥1.77) then spawns the resolved `.cmd` safely via
  the command processor with proper argument escaping. Documented at length in `transport.rs`.
- **Verified live:** the entire battery ran via `npx -y ...` on Windows with zero spawn failures.
- **Tests:** `resolve_program_finds_cmd_shim_on_windows`,
  `resolve_program_passes_through_explicit_paths_and_extensions`, plus a non-Windows identity test.

### Bug #3 — Embedded newlines garble the human report *(found via server-memory & server-sequential-thinking)*
- **Symptom:** tool descriptions / server instructions containing `\n` (multi-paragraph text) smeared
  across lines and destroyed the report's column alignment — a second, unindented line broke out of
  the tool's cell.
- **Fix:** the report now **collapses internal whitespace** (newlines, tabs, runs of spaces) to a
  single space for every single-line cell (descriptions, instructions) before truncation.
- **Verified live:** re-inspecting `server-sequential-thinking` now shows the description as one tidy
  truncated line.
- **Test:** `truncate_collapses_embedded_newlines_to_single_line`.

### Bug #4 — Silent stdout pollution *(hardening; the #1 real-world MCP failure mode)*
- **Symptom:** MCP stdio framing requires stdout to carry **only** newline-delimited JSON-RPC. A
  server that writes anything else to stdout (a stray `console.log`, a startup banner, a logger
  misconfigured to stdout, a stack trace) corrupts the stream. Jig recorded such lines in the tap but
  **never told the user** — a silent, baffling failure exactly where a diagnostic tool must be loud.
- **Fix:** new `ProtocolTap::non_protocol_inbound()` flags every inbound line that is not a JSON
  object. Both CLI commands now print a prominent stderr warning naming the offending line(s):
  `jig: warning: server wrote N non-protocol line(s) to stdout — this breaks MCP framing ...`. The
  handshake still completes where possible, so the warning accompanies a usable result rather than
  replacing it.
- **Verified live:** none of the six battery servers polluted stdout, so the fix was validated against
  a mock `--pollute-stdout` fixture that emits a plain-text line before the protocol traffic.
- **Test:** `stdout_pollution_is_captured_but_does_not_break_handshake` +
  `non_protocol_inbound_flags_stdout_pollution` unit test.

### Bug #5 — List pagination not followed *(hardening; a diagnostic tool must never lie about the list)*
- **Symptom:** `tools/list` / `resources/list` / `prompts/list` may return a `nextCursor`. Jig fetched
  only the first page, so against a paginating server it would silently show a **partial** list —
  unacceptable for a tool whose whole job is "see exactly what the model sees."
- **Fix:** all three list operations now follow `nextCursor` to completion, accumulating every page,
  bounded by a 1000-page cap and a guard against a cursor that never advances (both anti-infinite-loop
  safety valves).
- **Verified live:** none of the six battery servers paginated (all lists fit one page), so validated
  against a mock `--paginate` fixture serving one tool per page.
- **Test:** `tools_list_follows_cursor_pagination` (asserts all pages gathered *and* that N separate
  requests were actually made, via the tap).

Also added: `initialize_result_tolerates_old_version_and_missing_optionals` — locks in that jig
accepts a pre-2025 negotiated `protocolVersion` and a sparse initialize result (no capabilities, no
instructions) rather than rejecting the handshake.

---

## Spec observations (where real servers deviate from / stretch the 2025-06-18 spec)

1. **Undocumented capabilities.** `server-everything` advertises a **`tasks`** capability that does
   not appear in the 2025-06-18 specification (it is a newer/experimental surface). Real servers ship
   ahead of — and behind — the spec revision jig targets. Keeping `capabilities` as raw JSON (rather
   than a closed enum) is what lets jig report it faithfully instead of erroring. **Recommendation:**
   consider a curated "known capabilities" set so jig can *label* unrecognized ones as experimental
   in the report, without rejecting them.

2. **Capability advertised, list empty.** `context7-mcp` advertises `prompts` and `resources` yet
   returns empty lists for both. The spec treats a capability as "this method is supported," which is
   consistent, but it means "advertises X" ≠ "exposes any X." Jig's capability gate + graceful-empty
   handling is the right posture.

3. **Notifications interleaved with request/response.** `server-everything` emitted
   `notifications/tools/list_changed` between a request and its matching response. Correlation must be
   by JSON-RPC `id`, never by arrival order — jig does this correctly and records the notification in
   the tap for visibility.

4. **Structured content is real and common.** `server-filesystem` returns both a text content block
   and a `structuredContent` object on the same call (a 2025-06-18 feature). Jig renders both.

5. **`name` vs `title`.** Real servers set a human-facing `title` (e.g. "Echo Tool") distinct from the
   machine `name` used in `tools/call` (e.g. `echo`). The report currently shows the `title`; a user
   copying it into `jig call --tool` would use the wrong identifier. See open questions.

6. **No protocol downgrades observed.** Every server in the battery negotiated `2025-06-18`, the exact
   version jig proposes. Tolerance of *older* negotiated versions is covered by a unit test but was
   not exercised by a live server here.

---

## Open questions for the director

1. **Broken-pipe panic when piped to `head`.** `jig inspect ... | head` makes jig exit 101 (a Rust
   broken-pipe panic on `println!`) once `head` closes the pipe. It is standard Rust behavior and not
   server-caused, so I left it out of scope — but `| head` / `| less` is a common user reflex. Worth a
   small "reset SIGPIPE / swallow BrokenPipe" fix in a later pass?

2. **Report should show the callable `name`, not just `title`.** Real servers' human `title`
   ("Echo Tool") differs from the `tools/call` `name` ("echo"). A user copying the report's label into
   `jig call --tool` gets the wrong identifier. Should the report show `name` (with `title` secondary)?

3. **Labeling experimental/unknown capabilities** (e.g. `tasks`): keep silently passing them through,
   or annotate them in the report as "not in the targeted spec revision"?

4. **Third-party coverage.** Battery includes 2 non-Anthropic servers (Microsoft Playwright, Upstash
   Context7). Want a broader recurring set (e.g. a community server that is known to log to stdout, to
   exercise Bug #4's warning against a *real* offender) for the "we test against everything" story?

---

## Reproduction

```sh
cargo build
# The four reference servers:
./target/debug/jig inspect --stdio "npx -y @modelcontextprotocol/server-everything" --tap everything.jsonl
./target/debug/jig inspect --stdio "npx -y @modelcontextprotocol/server-filesystem \"C:\\some\\dir\"" 
./target/debug/jig inspect --stdio "npx -y @modelcontextprotocol/server-memory"
./target/debug/jig inspect --stdio "npx -y @modelcontextprotocol/server-sequential-thinking"
# Third-party:
./target/debug/jig inspect --stdio "npx -y @playwright/mcp@latest"
./target/debug/jig inspect --stdio "npx -y @upstash/context7-mcp"
# A safe read-only call:
./target/debug/jig call --stdio "npx -y @modelcontextprotocol/server-everything" --tool get-sum --args '{"a":2,"b":40}'
```

First contact downloads packages via npx; use `--timeout 60` (or higher) on a cold cache so package
download does not eat into the request window.
