# SOP: MCP server design & deployment

*A standard operating procedure for building Model Context Protocol servers that a
model can actually use — every rule cited to evidence, and, where possible, tied to
the exact [`jig`](https://github.com/Shodh-Labs/jig) command that verifies compliance.*

**Maintained by:** Shodh Labs · **Base evidence:** the [2026-07 census of 50 public
MCP servers](../census/2026-07-19-state-of-mcp-servers.md) (n=29 reachable) plus the
cited 2025–2026 literature · **Spec baseline:** MCP `2025-06-18`, version-aware
toward the `2026-07-28` release candidate · **Last reviewed:** 2026-07-20

---

## How to read this document

Every SOP has the same four parts:

- **Rule** — what to do, imperatively.
- **Why** — the evidence, with links and dates. Where evidence is thin or contested,
  it says so.
- **Verify** — the exact `jig` command that checks the rule, **or** an honest
  *"not yet machine-checkable"* when no instrument covers it. jig commands appear
  here only as a verification mechanism.
- **Citation** — the spec section, RFC, paper, or penalty table the rule rests on.

The differentiator of this guide over the many "MCP best practices" posts is that
almost every rule is *machine-verifiable*: you can run a command and get a pass/fail,
not a vibe. The [honesty appendix](#appendix-b--what-this-guides-evidence-actually-is)
draws the line between measured fact, editorial judgment, and claims that will age.

> **On jig's numbers being opinions.** Many thresholds below (30/50 tools, a 3× median
> cost ratio, a 4-token "terse" floor, a 160-token "verbose" ceiling, the 25/25/20/15/15
> rubric weights) are jig's *tuned defaults*, documented in
> [`crates/jig-core/src/check.rs`](../../crates/jig-core/src/check.rs) and
> [`advisor.rs`](../../crates/jig-core/src/advisor.rs). They are defensible, evidence-anchored
> editorial calls — not laws of nature. This guide labels them as such.

A note on the tooling names used below: there is **no `jig advisor` subcommand**. The
tool-set advisor runs automatically inside `jig check` and is also available after the
budget table via `jig budget --advise`. This document only prints commands that exist.

---

## Table of contents

1. [Design](#1-design) — SOP 1–12
2. [Protocol discipline](#2-protocol-discipline) — SOP 13–21
3. [Authorization (HTTP servers)](#3-authorization-http-servers) — SOP 22–23
4. [Deployment & packaging](#4-deployment--packaging) — SOP 24–27
5. [Testing & CI](#5-testing--ci) — SOP 28–31
6. [Remediation workflow](#6-remediation-workflow) — SOP 32–33
7. [Appendix A — quick-reference checklist](#appendix-a--quick-reference-checklist)
8. [Appendix B — what this guide's evidence actually is](#appendix-b--what-this-guides-evidence-actually-is)

---

## 1. Design

The design SOPs govern the surface a model reads *before it does anything*: the names,
descriptions, schemas, counts, and token cost of your tools. This is where most MCP
servers are won or lost, because a model never sees your code — it sees this surface.

### SOP 1 — Give every tool a legal, whitespace-free name

**Rule.** Every tool name must be 1–64 characters and match `^[A-Za-z0-9_./-]+$`. No
spaces, no punctuation outside `_ . / -`.

**Why.** A name with a space is literally uncallable by most function-calling APIs — the
model cannot emit it. The MCP conformance suite promotes this to a hard requirement
(`tools-name-format`), and jig penalizes a violation as a High-severity protocol fault
(8 points/name, capped at 24).

**Verify.** `jig check --stdio "<cmd>"` — protocol dimension, finding
`tools-name-format`. A whitespace name also trips the description-quality check
("contains whitespace — models cannot call it").

**Citation.** SEP-986 (MCP tool-name format); JSON-RPC method-name discipline. See
[`docs/conformance-alignment.md`](../conformance-alignment.md) and
`PROTOCOL_TOOL_NAME_FORMAT_PENALTY` in `check.rs`.

### SOP 2 — Do not ship synonym-collision names

**Rule.** No two tools should reduce to the same action once verbs are normalized:
`get_status` vs `fetch_status`, `list-users` vs `enumerateUsers`, `get_user` vs
`user_get` are all collisions.

**Why.** These are different strings but the same request; a model has no reliable basis
to choose between them, so selection becomes a coin-flip. jig's advisor tokenizes names
(kebab/snake/camel all normalized) and maps verbs through synonym groups
(`get≈fetch≈retrieve≈read`, `list≈enumerate`, `create≈add≈new`, `delete≈remove`,
`update≈edit≈modify`, `search≈find≈query`); an exact canonical-multiset match is flagged
High.

**Verify.** `jig check --stdio "<cmd>"` (Advisor block) or
`jig budget --stdio "<cmd>" --advise`.

**Citation.** `SYNONYM_GROUPS` in [`advisor.rs`](../../crates/jig-core/src/advisor.rs).
The remedy jig prints: merge the two, or rename one with a token that names the *real*
difference (scope, resource, side effect).

### SOP 3 — Do not distinguish tools by a generic suffix

**Rule.** Do not ship `get_user` alongside `get_user_info` (or `…_data`, `…_details`,
`…_result`, `…_full`). If a longer name adds only a filler word, it adds no information.

**Why.** The extra token names nothing the model can act on, so the pair is
indistinguishable in practice. Real distinguishers (`get_user` vs `get_user_admin`) are
fine — jig only flags the case where every extra token is generic.

**Verify.** `jig check` / `jig budget --advise` — advisor "generic subset" finding
(Medium).

**Citation.** `GENERIC_TOKENS` in `advisor.rs`.

### SOP 4 — Pick one naming convention and hold it

**Rule.** Choose kebab-case *or* snake-case for tool names and use it for every tool.

**Why.** A server that is "mostly snake_case" with one kebab outlier makes the surface
look accidental and gives the model a spurious signal. This is a minor, heuristic nit
(5 points in the Description-quality dimension), not a correctness fault — but it is free
to fix.

**Verify.** `jig check --stdio "<cmd>"` — Description quality dimension
("uses kebab-case while the server is mostly snake_case").

**Citation.** `dominant_convention` / `DQ_NAME_INCONSISTENT` in `check.rs`. Description
quality is explicitly labelled *heuristic* (deterministic, no LLM).

### SOP 5 — Write descriptions for selection, not documentation

**Rule.** Each tool description should say **what it does and when to reach for it**, in
roughly one to three sentences. jig's actionable band is a description longer than ~4
tokens and shorter than ~160 tokens; a two-word description and a three-paragraph essay
are both findings.

**Why.** The description is the model's entire basis for selection. A 2026 study of 856
tools across 103 MCP servers found **97.1% of tool descriptions carry at least one
"smell," and 56% fail to state their purpose clearly** (Hasan et al., *"MCP Tool
Descriptions Are Smelly!"*, [arXiv:2602.14878](https://arxiv.org/abs/2602.14878), Feb
2026). Terse descriptions starve selection; verbose ones burn context on every turn
(see SOP 11). jig flags terse (≤4 tok, Medium) and verbose (≥160 tok, Low) descriptions.

**Verify.** `jig check --stdio "<cmd>"` (Description quality) for the length bands;
`jig context --stdio "<cmd>"` to *read the exact block the model receives*,
token-annotated, with no API key.

**Honesty — partially machine-checkable.** jig measures description *length* deterministically.
It does **not** judge whether a description actually states its purpose — the smelly
paper's central finding is a semantic property jig cannot score today. Treat the length
band as necessary, not sufficient.

**Citation.** `DQ_TERSE_TOKENS` / `DQ_VERBOSE_TOKENS` in `check.rs`; arXiv:2602.14878.

### SOP 6 — Every tool described; every parameter typed and described

**Rule.** No empty tool descriptions. Every input-schema property carries a JSON Schema
`type` (or `enum`/`const`/`$ref`/`anyOf`/`oneOf`/`allOf`) **and** a `description`.

**Why.** A missing type means the model guesses the shape of an argument; a missing
parameter description means it guesses the meaning. jig's Schema-hygiene dimension
deducts per gap: 8 points for a missing tool description, 5 for a missing param type
(High — it breaks argument filling), 3 for a missing param description.

**Verify.** `jig check --stdio "<cmd>"` — Schema hygiene dimension.

**Citation.** `SCHEMA_MISSING_TOOL_DESC`, `SCHEMA_PARAM_MISSING_TYPE`,
`SCHEMA_PARAM_MISSING_DESC` in `check.rs`. All-optional schemas are legal — jig never
flags a missing `required` array.

### SOP 7 — Declare tool annotations (`readOnlyHint`, `destructiveHint`, …)

**Rule.** Add MCP tool annotations so a client can tell a read from a write from a
destructive operation without calling the tool.

**Why.** Clients use annotations to decide what to auto-approve, what to confirm, and
what to surface as dangerous. Without them, a client must treat every tool as
potentially destructive — or, worse, treat none as destructive. jig scores this as a
minor, capped deduction (1 point/tool, cap 10) precisely because it is easy and
low-risk, not because it is unimportant.

**Verify.** `jig check --stdio "<cmd>"` — Schema hygiene ("N tool(s) declare no
annotations").

**Citation.** `SCHEMA_MISSING_ANNOTATIONS` / `has_annotations` in `check.rs` (accepts a
top-level `annotations` object or any `*Hint` key).

### SOP 8 — Give each tool a human-facing `title`

**Rule.** Set a `title` on every tool for client display.

**Why.** The machine name is for the model; the title is for the human watching the
client UI. A minor, capped nit (1 point/tool, cap 10) — but it improves every
client that renders a tool picker.

**Verify.** `jig check --stdio "<cmd>"` — Description quality ("N tool(s) have no
human-facing title").

**Citation.** `DQ_MISSING_TITLE` in `check.rs`.

### SOP 9 — Stay within a tool-count budget: ≤30 comfort, 50 hard ceiling

**Rule.** Keep a single server under ~30 tools where you can, and treat 50 as a ceiling.
Past that, split into focused servers or defer rarely-used tools behind server-side tool
search.

**Why.** Tool-selection accuracy degrades with *count*, independent of any one tool's
quality. Independent 2026 measurements are blunt: GitHub's official MCP server dropped
correct-selection accuracy from **~95% (focused toolset) to ~71%** when its full surface
was loaded ([lunar.dev](https://www.lunar.dev/post/why-is-there-mcp-tool-overload-and-how-to-solve-it-for-your-ai-agents),
[dev.to](https://dev.to/thedailyagent/mcp-tool-overload-why-more-tools-make-your-agent-worse-5a49));
production telemetry finds Claude Sonnet holding ≥90% only to ~20 tools and falling off
by 30 (*MCP Server Architecture Patterns*,
[arXiv:2606.30317](https://arxiv.org/pdf/2606.30317)); and multiple reports describe a
*cliff*, not a slope, past ~30–50 tools. GitHub Copilot cut 40 tools to 13; Block
rebuilt a 30+-tool Linear server down to 2. The `2026-07-28` RC's **Tool-Search**
direction is the structural fix — let the server surface a search over tools instead of
dumping all of them into context.

**Verify.** `jig check --stdio "<cmd>"` — advisor accuracy-cliff finding (`>30` Medium,
`>50` High).

**Citation.** `CLIFF_MEDIUM_TOOLS = 30`, `CLIFF_HIGH_TOOLS = 50` in `advisor.rs`. For
scale, the [census](../census/2026-07-19-state-of-mcp-servers.md) found a median of 14
tools, p90 of 28, and a max of **89** (`dataforseo-mcp-server`).

### SOP 10 — No single tool should dominate the context bill

**Rule.** Avoid one tool costing more than ~3× your median tool (above a 200-token
floor), and avoid a top-3 that carries the majority of the surface's tokens.

**Why.** A single fat tool makes the whole surface pay for a definition few calls need.
The [census](../census/2026-07-19-state-of-mcp-servers.md) caught this exactly:
`@shopify/dev-mcp` spends 5,372 tokens across just **5** tools (~1,074 each) while
`@microsoft/clarity-mcp-server` spends 4,300 across **3** — cost is about how much you
describe each tool, not how many you ship.

**Verify.** `jig budget --stdio "<cmd>" --advise` (or `jig check`) — advisor
cost-dominance and top-3-concentration findings.

**Citation.** `COST_DOMINANCE_RATIO = 3.0` (Medium at 5×), `COST_MIN_TOKENS = 200`,
`COST_CONCENTRATION_THRESHOLD = 0.50` (Medium at 0.70) in `advisor.rs`.

### SOP 11 — Hit a context-budget target: aim ≤ median (1,679 tok), alarm past p90 (14,401 tok)

**Rule.** Price your whole tool surface in tokens and treat the ecosystem distribution
as your yardstick. Aim to land at or below the median reachable server (**1,679 gpt-4o
tokens**); treat crossing the 90th percentile (**14,401 tokens**) as an alarm.

**Why.** The tool surface is paid on *every* turn, before the user types a word. The
[census](../census/2026-07-19-state-of-mcp-servers.md) (n=29, gpt-4o `o200k_base`, exact)
found a savage right tail: min 124, median 1,679, p75 7,595, p90 14,401, and a max of
**42,288** (`dataforseo-mcp-server`) — 25× the median. Independent autopsies corroborate
the stakes: GitHub's MCP server alone is ~42k tokens of tool-definition JSON, and a
Claude Code session with 5–10 MCPs installed commonly burns 50k–67k tokens before the
first prompt ([getunblocked, 2026](https://getunblocked.com/blog/mcp-token-budget-autopsy/)).

**Verify.** `jig budget --stdio "<cmd>" --model gpt-4o` for the per-tool + total token
table; `jig check --stdio "<cmd>"` scores context cost against
[`data/percentiles.json`](../../data/percentiles.json) and reports your percentile
(e.g. *"94th percentile of n=29 measured servers — heavier than 94%"*). Absent the
dataset it falls back to documented absolute bands and says so.

**Citation.** [`docs/percentiles-schema.md`](../percentiles-schema.md),
[`docs/token-budget.md`](../token-budget.md) (canonical rendering);
`score_context` in `check.rs`. Anthropic token counts are a labelled `~approx` unless
you pass `--exact-anthropic`.

### SOP 12 — Treat tool descriptions as an attack surface

**Rule.** Never place hidden or "system" instructions in a tool description or schema.
Assume the description is read as ground truth by the model, and design for least
privilege on the tools themselves.

**Why.** *Tool poisoning* — malicious instructions embedded in a tool's description at
registration — is a live class of indirect prompt injection specific to MCP. Invariant
Labs demonstrated a poisoned description that exfiltrated a user's entire WhatsApp
history through a benign-looking call ([Invariant Labs, 2025](https://mcpmanager.ai/blog/tool-poisoning/)),
and the threat is now benchmarked (*MCPTox*,
[arXiv:2508.14925](https://arxiv.org/html/2508.14925v1)). A description is untrusted
input to the model, even when *you* wrote it, because a downstream server in the same
session may not have.

**Verify.** `jig check --stdio "<cmd>"` — the **Tool poisoning** section (and the
`injection` array under `--json`). As of `rubric-v1.3` jig runs a deterministic,
no-LLM lint over every tool name, description, and parameter description, with five
detectors: **model-directed imperatives** (instruction override, concealment,
invocation ordering, authority override — matched word-boundary from a documented
phrase table plus a mechanical `stem × control-object` cross product, so "you must
always **call**" fires and "you must always **provide** a valid API key" does not) —
High; **fake conversation turns** (chat-template tokens like `<|im_start|>`, XML-ish
role tags like `<system>`, or two or more distinct line-anchored role labels) — High;
**hidden characters** (zero-width `U+200B`–`U+200D` / `U+FEFF`, bidi controls
`U+202A`–`U+202E` and `U+2066`–`U+2069` — the Trojan Source class, CVE-2021-42574 —
and non-ASCII homoglyphs in tool *names* specifically) — High; **exfiltration shape**
(a URL within 120 characters of an outbound-transfer verb) — Medium, and worded as a
smell rather than proof; and **name/behaviour mismatch** (a `read_*` / `get_*` /
`list_*` name, or `readOnlyHint: true`, over a description carrying an un-negated
mutation verb) — Medium. Findings are **reported, never scored** into the composite —
the same posture as the tool-set advisor — but they are always **pinned**, so they
cannot be buried beneath the scored findings in "Top fixes". Every finding cites
MCPTox or the spec's trust guidance and carries a fix.

**Honesty — partially machine-checkable.** The lint matches the *shape* the published
attacks take, not intent. A semantic attack written in plain, well-formed English —
no override phrasing, no fake turns, no hidden characters, no URL — passes it
cleanly, and there is no threshold at which it would not. This is a lint, not a
red-teamer. Keep running a dedicated one such as
[Promptfoo's MCP security testing](https://www.promptfoo.dev/docs/red-team/mcp-security-testing/),
and track the official
[conformance suite](https://github.com/modelcontextprotocol/conformance)'s security
scenarios (`dns-rebinding`, and the HTTP-security set) — those remain explicitly
[outside jig's single-session observer scope](../conformance-alignment.md).

**Citation.** Invariant Labs Tool Poisoning Attack; MCPTox (arXiv:2508.14925);
Promptfoo red-team docs.

---

## 2. Protocol discipline

These SOPs govern the wire: framing, capabilities, lifecycle, and startup behavior. They
are where jig is strongest, because they are deterministic, single-session, and
"server-emits-it" — exactly the scope jig covers well
([conformance alignment](../conformance-alignment.md)).

### SOP 13 — stdout is sacred: only newline-delimited JSON-RPC

**Rule.** On the stdio transport, write **nothing** to stdout except protocol messages.
All logging, banners, and diagnostics go to **stderr**.

**Why.** MCP's stdio transport frames messages as newline-delimited JSON-RPC on stdout.
A single stray `console.log`, startup banner, or misrouted logger corrupts the framing
and can break the whole session. The [census](../census/2026-07-19-state-of-mcp-servers.md)
found **4 of 50** servers writing non-protocol lines to stdout — the single most common
real-world MCP break. jig taps the byte stream, so it names the exact offset and quotes
the offending bytes.

**The correction, told honestly.** The census originally reported `@azure/mcp` (288
lines) and `@launchdarkly/mcp-server` (10 lines) as *breaking their own handshake*.
Re-verification with each vendor's documented invocation showed those lines were **CLI
usage text triggered by our bare `npx` invocation** — both require a `server start` /
`start` subcommand — not runtime pollution. Invoked correctly, `@azure/mcp` handshakes
cleanly (protocol 100/100). The residual, true finding is narrower: both CLIs print
usage to *stdout* rather than stderr, so a *misconfigured* client sees corrupted framing
instead of a clean error. Two other servers — `@agentdeskai/browser-tools-mcp` (36
lines) and `mcp-server-code-runner` (1 line) — really did pollute stdout yet completed
the handshake anyway. The lesson cuts both ways: pollution is real and common, *and* a
grader must invoke servers the way they document, or it manufactures its own findings.
We kept the original claim visible in the census because corrections should be as public
as the findings they fix.

**Verify.** `jig check --stdio "<cmd>"` (protocol dimension; pollution is *pinned* into
Top Fixes so it can never be buried) — or capture the raw stream with
`jig inspect --stdio "<cmd>" --tap traffic.jsonl` and read the offending bytes.

**Citation.** `PROTOCOL_POLLUTION_PENALTY` (15/line, cap 60) in `check.rs`. This check
is **jig-unique** — the official conformance suite drives HTTP and does not exercise
stdio framing at all ([conformance alignment](../conformance-alignment.md)).

### SOP 14 — Advertise only capabilities the negotiated revision defines

**Rule.** Advertise a top-level capability only if it exists in the protocol revision you
negotiated. Gate anything experimental behind the `experimental` map (or, in the
`2026-07-28` RC, the `extensions` map).

**Why.** Capability legality is **version-relative**. `completions` is legal from
`2025-03-26`; `tasks` was standardized (experimentally) as a top-level capability in
`2025-11-25`; in the `2026-07-28` RC the Tasks feature was **redesigned as an extension**
advertised through `extensions`, so `tasks` is deliberately *not* a top-level key there.
A server that advertises `tasks` while negotiating `2025-06-18` is claiming a capability
that revision never defined. jig grades each advertised capability against the *negotiated*
revision and cites where the capability *is* first defined.

**Verify.** `jig check --stdio "<cmd>"` — protocol off-spec-capability finding. `jig
inspect` annotates advertised capabilities the same version-aware way.

**Citation.** `REVISIONS` table + `offspec_capabilities` in `check.rs`;
[conformance alignment](../conformance-alignment.md) on the `tasks` 2025-11-25 →
2026-07-28 story;
[MCP `2026-07-28` RC](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/).

### SOP 15 — Back every advertised capability with content

**Rule.** If you advertise `prompts` or `resources`, return a non-empty list for them.
Do not announce a capability you populate with nothing.

**Why.** A model told "this server has prompts" and then handed an empty array has been
mildly misled. The [census](../census/2026-07-19-state-of-mcp-servers.md) found **5 of 29**
reachable servers advertising a capability they returned nothing for (e.g.
`@upstash/context7-mcp` advertised both `resources` and `prompts`, populated neither).
These are small gaps, not errors — but they are gaps.

**Verify.** `jig inspect --stdio "<cmd>"` shows advertised capabilities alongside the
actual tool/resource/prompt counts, so the mismatch is visible.

**Honesty — not scored.** jig *surfaces* this in `inspect` but does **not** deduct for it
in the `jig check` composite today. It is a reported observation, not a graded rule.

**Citation.** Census §"Honesty of capabilities."

### SOP 16 — Answer unknown methods with `-32601 Method not found`

**Rule.** A method your server does not implement must return the JSON-RPC error code
`-32601`, never a different error code and never a spurious success.

**Why.** Clients and gateways rely on the standard code to distinguish "not supported"
from "failed." jig sends a deliberately bogus method and grades the reply: a wrong error
code is Medium (10 pts), *accepting* an unknown method is High (20 pts).

**Verify.** `jig check --stdio "<cmd>"` — protocol `negative` conformance finding.

**Citation.** JSON-RPC 2.0 §5.1; conformance scenario `negative`;
`PROTOCOL_UNKNOWN_METHOD_*` in `check.rs`.

### SOP 17 — Return a spec-valid `initialize` result

**Rule.** The `initialize` result must carry a non-empty `serverInfo.name` and
`serverInfo.version`, and a `capabilities` object (not null, not an array).

**Why.** Downstream tooling keys off these fields; an empty name or a non-object
capabilities map is a shape violation that a live server can still emit even when its
types nominally require the fields. jig deducts 10 points per gap.

**Verify.** `jig check --stdio "<cmd>"` — protocol `server-initialize` finding.

**Citation.** Conformance scenario `server-initialize` (MCP-Initialize);
`initialize_field_gaps` in `check.rs`.

### SOP 18 — Practice version-negotiation hygiene, and be version-aware

**Rule.** Speak the current protocol revision (`2025-06-18` today), negotiate honestly,
and design so a version bump does not silently change which capabilities you advertise
(see SOP 14). Track the `2026-07-28` RC.

**Why.** Protocol drift among *maintained* servers is not the ecosystem's problem:
**27 of 29** reachable census servers already spoke `2025-06-18`; the two laggards were a
deprecated package and one other. The bigger change coming is architectural — the
`2026-07-28` RC makes the protocol **stateless** (no `initialize` handshake, no
`Mcp-Session-Id`; protocol version, client info, and capabilities travel inline in a
`_meta` field per request), adds first-class **extensions** (Tasks, MCP Apps), hardens
auth, and adds `Mcp-Method`/`Mcp-Name` routing headers (SEP-2243). Build so you can adopt
it. ([MCP `2026-07-28` RC](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/).)

**Verify.** `jig inspect --stdio "<cmd>"` reports the negotiated `protocolVersion`;
`jig check` grades capability legality against it.

**Citation.** Census §"Protocol versions"; MCP `2026-07-28` release candidate.

### SOP 19 — Fail fast on missing credentials — never hang

**Rule.** If your server needs a credential it does not have, **exit non-zero with a
clear stderr message** (e.g. `MISSING_TOKEN`). Never block on startup waiting for input
that will never come.

**Why.** This is the ecosystem's dominant failure mode. Of 50 installable census
servers, **21 (42%) never completed a handshake** — and **14 of those exited demanding a
credential** the instant they booted. Worse, **2 servers (`@mapbox/mcp-server`,
`@heroku/mcp-server`) hung indefinitely**, neither listing tools nor erroring, forcing a
timeout. A server that exits with a clear error is diagnosable; one that hangs is not.
"Installable is not the same as runnable."

**Verify.** `jig check --stdio "<cmd>"` fails fast and, when a percentile dataset carries
`startup_failure_rate`, adds cohort context (*"in the 2026-07 census, 42% of surveyed
public MCP servers also failed at startup"*). `--timeout <seconds>` bounds every request
so a hang fails fast instead of blocking; a list that is accepted but never answered is a
High protocol finding (`list_timed_out`, 40 pts).

**Citation.** Census §"Reachability"; `startup_failure_note` /
`PROTOCOL_LIST_TIMEOUT_PENALTY` in `check.rs`;
[`docs/percentiles-schema.md`](../percentiles-schema.md).

### SOP 20 — Shut down cleanly on transport close / EOF

**Rule.** Handle transport close and EOF, and exit promptly on shutdown.

**Why.** A server that has to be killed leaks processes in long-running clients and CI. It
is an *observed* robustness signal — jig only scores what it actually saw in the session,
never an assumption.

**Verify.** `jig check --stdio "<cmd>"` — Robustness dimension ("clean shutdown" vs "the
server did not shut down cleanly").

**Citation.** `ROBUST_UNCLEAN_SHUTDOWN_SCORE` in `check.rs`.

### SOP 21 — Answer every request, and keep list latency low

**Rule.** Every request must receive a response, and `tools/list` should return quickly
(jig treats ≤1s as unremarkable, ≤3s as sluggish, slower as slow).

**Why.** A hung list handler is indistinguishable from a dead server (SOP 19). High list
latency usually means a per-request cold start or slow enumeration that the user pays on
every connection. Robustness is *observed only* — nothing is assumed.

**Verify.** `jig check --stdio "<cmd>"` — Robustness (list latency) and protocol
(list-timeout) dimensions.

**Citation.** `ROBUST_LATENCY_*` constants in `check.rs`.

---

## 3. Authorization (HTTP servers)

Authorization is MCP's number-one integration pain, and it applies to the Streamable
HTTP transport only (a stdio target has no OAuth surface — `jig auth` says so plainly).
Across the public ecosystem only **~8.5%** of servers implement the OAuth flow the spec
calls for (Astrix Research, *State of MCP Server Security 2025*, cited by
[Microsoft, 2026](https://techcommunity.microsoft.com/blog/appsonazureblog/only-8-5-of-mcp-servers-use-oauth-%E2%80%94-heres-how-to-host-one-securely-on-app-servic/4530349)),
and the failures are almost always in the *discoverable* surface — exactly what `jig
auth` grades.

### SOP 22 — Serve RFC 9728 metadata and a proper 401 challenge

**Rule.** An unauthenticated request must return **HTTP 401** with a
`WWW-Authenticate: Bearer` header that carries a `resource_metadata` parameter pointing
at your Protected Resource Metadata (RFC 9728). That metadata must include the REQUIRED
`resource` field, list at least one `authorization_servers` entry, and the `resource`
must identify *this* server (RFC 8707 audience binding — a mismatch invites
token-confusion attacks).

**Why.** The 401 + `resource_metadata` pointer is how a client discovers where to
authenticate; without it, a compliant client cannot begin the flow. The audience check is
a real security control, not a formality — an advertised `resource` pointing at another
origin is a token-confusion vector.

**Verify.** `jig auth --http "https://…/mcp"` — the "Unauthenticated challenge" and
"Protected Resource Metadata" probes, each finding carrying its spec citation. Add
`--header "Authorization: Bearer $TOKEN"` to also test that a real token is accepted
(jig never fabricates one).

**Citation.** RFC 9728 §5.1 (challenge), §2 + §3 (metadata); RFC 8707 §2 (audience);
RFC 6750 §3. See [`crates/jig-core/src/auth.rs`](../../crates/jig-core/src/auth.rs)
(`grade_prm`), pinned to MCP auth spec revision `2025-06-18`.

### SOP 23 — PKCE `S256` is mandatory; DCR and `iss` are recommended

**Rule.** Your authorization server metadata (RFC 8414) must advertise PKCE `S256` in
`code_challenge_methods_supported`, plus both `authorization_endpoint` and
`token_endpoint`. **Should**-strength: advertise a `registration_endpoint` (RFC 7591
Dynamic Client Registration) and set
`authorization_response_iss_parameter_supported` (RFC 9207 AS-mix-up defense).

**Why.** The MCP spec **requires** PKCE with `S256`; without it the code flow is open to
interception. DCR lets clients self-register instead of demanding manual credentials, and
the `iss` parameter hardens against authorization-server mix-up. jig grades a missing
`S256` as a FAIL and treats missing DCR / `iss` as NOT-ADVERTISED (a `SHOULD`), not a
failure — matching their spec strength.

**Verify.** `jig auth --http "https://…/mcp"` — the "Authorization Server Metadata"
probe (`asm_pkce_s256`, `asm_endpoints`, `asm_dcr`, `asm_iss`).

**Honesty — scope.** `jig auth` probes the *discoverable* surface only. It does **not**
perform the authorization-code + PKCE login, exchange a code for a token, or exercise
DCR — those are on the roadmap. It grades precisely the surface most servers get wrong.

**Citation.** RFC 8414 §2; RFC 7591 (DCR); RFC 9207 §3 (`iss`); MCP `2025-06-18`
authorization. `grade_asm` in `auth.rs`.

---

## 4. Deployment & packaging

### SOP 24 — Be `npx`-runnable to at least handshake + list with no config

**Rule.** `npx -y <package>` should reach the MCP handshake and list your tools **without**
an API key or config file. Gate the *use* of tools behind credentials if you must, but let
a model see the surface on connect.

**Why.** Discovery tools list packages, not working servers. The
[census](../census/2026-07-19-state-of-mcp-servers.md) selected only servers that install
with a bare `npx -y`, and still **42% failed the handshake** — mostly by demanding a key
at boot (SOP 19). A server that lists its tools before authentication (as
`@modelcontextprotocol/server-github` does with its 26 tools) is usable in far more
contexts than one that refuses to start.

**Verify.** `jig info <package> --probe` actually runs `npx -y <pkg>` and reports the live
handshake, tool count, and token cost (with a printed consent notice + 2s abort window;
`--yes` for scripts). `jig check --stdio "npx -y <pkg>"` grades the same cold path.

**Citation.** Census §"Reachability" and methodology.

### SOP 25 — Budget your cold start: small deps, no postinstall network

**Rule.** Keep the `npx` cold-start cost low. Avoid heavyweight dependency trees and
**never** do network work in a `postinstall` hook.

**Why.** The first connection pays two costs, and they are not the same cost. jig now
measures them separately: **install** (populating the npm cache) and **boot** (launch to
`initialize`). The **8-second `npx` cold start** previously quoted here for
`@modelcontextprotocol/server-everything` conflated the two — it was overwhelmingly npm
download time, not server startup. Measured apart:

| Cache state | install | boot |
|:------------|--------:|-----:|
| cold (`_npx` deleted) | **12.5s** | 8.8s |
| warm | 2.0s | 3.1s |
| warm, repeat | 2.0s | 2.8s |

The download alone, on a cold cache, is larger than the whole 8s that used to be
quoted for both halves together. Two further facts the split exposes: boot is not
constant for the same server (8.8s cold vs ~2.9s warm, on an already-downloaded
package — that residual is npm's first-use resolution work), and most of even the
warm boot is not your server. Timing the cached entrypoint directly with `node`,
bypassing the `npx` shim, the same server answers `initialize` in **0.30s**.

Only **boot** is a property of *your server*; install belongs to the registry and the
network, is paid once rather than per session, and is therefore reported but never graded.
Only boot is scored, in Robustness. High list latency compounds it (SOP 21).

**Verify.** `jig check --stdio "npx -y <pkg>"` — for `npx`-shaped commands jig runs a
pre-warm pass that populates the cache *without starting the server*, then reports the
split as `install 12.5s · boot 8.8s`, scoring boot alone. Non-`npx` commands report install
as `n/a`; `--no-prewarm` skips the pre-warm entirely (offline, air-gapped, or a cache you
know is warm) and reports install as `skipped`. Capture the full timeline with
`--tap traffic.jsonl` to see where the seconds go.

**Honesty.** Boot for an `npx` command still contains npm's own shim resolution and process
launch — jig times the launch, not the server's first instruction — so the boot figure
**over-estimates** true server boot, and by more than a rounding error: of the ~2.9s warm
boot above, roughly 2.6s is npm and 0.3s is the server. jig does not subtract it, because
the correction is not a constant and measuring it per-run would mean timing a null server
through the same path every time. Read boot as an **upper bound**, which is the safe
direction for a grade.

**Citation.** Shodh Labs workbench tap; `crates/jig-core/src/boot.rs` (pre-warm pass and
the install/boot split).

### SOP 26 — Document env vars and design the credential UX

**Rule.** Document every environment variable your server reads, and make the
credential-failure message name the missing variable (SOP 19). Never log secrets.

**Why.** The env block is where clients store API keys — jig's own discovery treats every
env value as a secret and **redacts it** (`KEY=•••`) in both the table and JSON, showing
only key names. Your server should be equally careful: read keys from the environment
only, and keep them out of logs, errors, and any rendered output.

**Verify.** `jig servers` lists servers configured on the machine with env **values
redacted**, so you can confirm what a client will pass without exposing it. And as of
`rubric-v1.3`, `jig check` and `jig info --probe` **grade the credential-failure UX**:
when a stdio server fails to start, jig re-launches it once under observation, parses the
child's stderr for a variable name (`[A-Z][A-Z0-9_]{2,}` in `KEY=value` form or on a line
carrying a configuration cue), and prints the same verdict line from either command —

| Observed failure | Verdict | Robustness sub-score |
|:--|:--|--:|
| Exits nonzero **and** names the variable | Pass — informational, no deduction | — |
| Exits nonzero without naming it | Medium — *fail fast is right; say which variable* | 60 |
| Hangs until timeout | High | 0 |
| Exits **zero** on a failed start | High | 0 |

**Honesty — partially machine-checkable.** jig can show *which* env keys a client is
configured to pass and grade *how* your server fails without them; it cannot verify your
*docs* list those variables, nor prove your server never logs a secret. That part remains
review, not measurement.

**Citation.** README §"Discovery" (env redaction);
[`docs/token-budget.md`](../token-budget.md) on key-handling discipline (mirrors what
your server should do).

### SOP 27 — Choose stdio vs Streamable HTTP deliberately, and plan for sessions at scale

**Rule.** Use **stdio** for local, single-user, subprocess servers; use **Streamable
HTTP** for remote/multi-user servers. If you go HTTP and expect to scale horizontally,
design your session strategy up front.

**Why.** The transports have different failure modes: stdio's is stdout framing (SOP 13);
HTTP's is session affinity. Today's stateful Streamable HTTP pins a client to one instance
via `Mcp-Session-Id`, which does not survive a round-robin load balancer — a well-known
pain point (python-sdk
[#880](https://github.com/modelcontextprotocol/python-sdk/issues/880),
[#756](https://github.com/modelcontextprotocol/python-sdk/issues/756) stateless-mode
memory leak, [#1180](https://github.com/modelcontextprotocol/python-sdk/issues/1180)).
The `2026-07-28` RC's stateless core is the ecosystem's answer: any instance can serve any
request, so a remote server can sit behind a plain load balancer with no sticky sessions.

**Verify.** **Largely outside jig's scope (honest).** jig connects over both transports and
follows the spec's session rules (captures `Mcp-Session-Id`, echoes it plus
`MCP-Protocol-Version`, sends `DELETE` on shutdown, and surfaces a clear error on a 404
expired session instead of silently reconnecting), and `jig inspect --http … --listen`
can watch a server's SSE stream. But horizontal-scale session persistence is a deployment
property jig does not grade. See the linked SDK issues and the
[MCP transports blog](https://blog.modelcontextprotocol.io/posts/2025-12-19-mcp-transport-future/).

**Citation.** MCP Streamable HTTP transport; python-sdk #880/#756/#1180; MCP `2026-07-28`
RC (stateless core).

---

## 5. Testing & CI

### SOP 28 — Keep eval suites in git and score by selection rate, not booleans

**Rule.** Turn `prompt → expected tool call` cases into `*.yaml` files under a `.jig/`
directory, versioned next to your server. Score each case by a **selection rate** over N
runs, never a single pass/fail.

**Why.** MCP integration is probabilistic — the same task can pick different tools on
different runs. A boolean test lies about a flaky surface; a rate exposes it. `jig eval`
scores each case as the fraction of runs that selected the expected tool *and* passed
every argument matcher *and* were schema-valid, flags a case that flips between runs as
**FLAKY** even when it passes, and excludes provider errors from the denominator (reporting
them loudly). Seed suites cheaply with `jig bench --save-case`.

**Verify.** `jig eval --stdio "<cmd>"` runs every `./.jig/*.yaml`;
`jig bench --stdio "<cmd>" --task "<task>" --model gpt-4o --runs 5` explores a single task
and its distribution.

**Citation.** README §`jig eval` / §`jig bench`. Scoring is deterministic matchers only
(`exact`/`contains`/`regex`/`one_of`/`range` + schema validity) — **no LLM judge**, so the
gate is itself reproducible.

### SOP 29 — Gate CI on the grade and the eval accuracy

**Rule.** Wire two gates into CI: a minimum report-card score and a minimum eval accuracy.

**Why.** A gate is what converts a measurement into a regression guard. `jig check
--min-score` fails the build when the composite drops below a floor; `jig eval --gate`
fails when weighted accuracy falls below a threshold or a `must_pass` case regresses.

**Verify.** `jig check --stdio "<cmd>" --min-score 80` (exit non-zero below the floor) and
`jig eval --stdio "<cmd>" --suite .jig/search.yaml --gate 0.9` (exit `3` on a miss).
`jig eval --junit eval.xml` emits CI-native JUnit XML.

**Citation.** README §`jig check` (`--min-score`) / §`jig eval` (`--gate`, exit codes).

### SOP 30 — Re-run the suite on every model-version bump

**Rule.** Treat a model-version change like a dependency bump: re-run your eval suite and
diff the results.

**Why.** Tool selection is a property of the *model × surface*, so a new model version can
regress selection on a surface you never touched. Because every `jig eval` report ends with
a **pinned-context block** (model id + reported version, temperature, runs, suite files,
system prompt, scoring rubric), a run is reproducible and two runs are comparable.

**Verify.** `jig eval --stdio "<cmd>" --json` for machine-diffable per-run detail across
model versions.

**Citation.** README §`jig eval` (pinned-context block, reproducibility).

### SOP 31 — Publish a report card (and a badge) in PRs

**Rule.** Surface the grade where reviewers see it: a report card in the PR and a score
badge in the README.

**Why.** A grade with ranked fixes replaces eyeballing a chat client. Making it visible in
review is what keeps the surface honest over time.

**Verify.** `jig check --stdio "<cmd>" --json` for the full findings payload to render in a
PR comment; `jig check --stdio "<cmd>" --badge` emits shields.io endpoint JSON for a README
badge.

**Citation.** README §`jig check` (`--json`, `--badge`); `badge_color` in `check.rs`.

---

## 6. Remediation workflow

The findings from §1–§5 are *wire-level* — "param `cx` missing a description on
`create_box`", "rename `fetch_drawing` (collides with `get_drawing`)". The fix, though,
lives in **source**. And the maintainer applying it is increasingly an AI agent whose
context window is the same scarce resource this guide spends §1 teaching you to protect on
the *serving* side. Spending it re-reading whole files to locate one fix site is the same
waste, on the other end of the pipe.

### SOP 32 — Fix findings in source, located via a code-graph query — not whole-file reads

**Rule.** Drive remediation from machine-readable findings, and locate each fix site with a
**code knowledge graph / LSP-grade navigation** query (semantic search for the tool's
registration site) instead of reading entire files into context. Keep the choice of tool
agnostic — any graph or LSP layer that answers "where is `create_box` defined" with a
targeted slice qualifies.

**Why.** The findings are already structured, and a graph turns "find where this tool is
registered" into a bounded query rather than a file crawl. Graph-assisted navigation layers
report order-of-magnitude per-question token reductions:
[`code-review-graph`](https://github.com/tirth8205/code-review-graph) reports (vendor-reported)
a **median ~82× per-question token reduction across six repos**, returning ~2,000–3,500
tokens of targeted hits plus neighbor edges instead of forcing a full-corpus read; a
related variant reports 6.8× on reviews and up to 49× on daily coding tasks. Attribute these
as the vendors' own benchmarks, not independently verified — the *principle* (query a graph,
read a slice) is what this SOP asserts; the exact multiple is theirs.

**The workflow:**
1. `jig check --stdio "<cmd>" --json` → machine-readable findings (each carries a `fix`
   string and the offending tool/param name).
2. For each finding, query the graph to the definition site — e.g.
   `code-review-graph search "create_box"` (or your LSP's "go to definition") — and read
   only that slice.
3. Apply the fix in source.
4. Re-run `jig check --stdio "<cmd>"` and diff the grade to confirm the finding cleared and
   nothing regressed.

**Verify.** **Not machine-verifiable by jig.** This is workflow guidance: jig produces the
findings (step 1) and confirms the fix (step 4), but the graph-navigation step is external.
The token-efficiency claim is the graph vendor's, cited as such.

**Citation.** [code-review-graph](https://github.com/tirth8205/code-review-graph)
(vendor-reported benchmarks); the context-scarcity rationale is this guide's own §1 and
SOP 11, applied to the maintainer side.

### SOP 33 — Check rename impact radius *before* applying an advisor collision fix

**Rule.** Before renaming a tool to resolve a collision (SOP 2/3), query the code graph for
the symbol's **impact radius** — callers, tests, and registration sites — and update them in
the same change.

**Why.** The advisor's collision and generic-subset findings are *renames*, and a rename is
exactly where a graph beats `grep`: a text search misses a call site that constructs the
name dynamically or aliases it, and hits false positives in comments and unrelated strings. A
graph's callers/callees and impact-radius queries return the actual reference set, so the
rename lands complete and the re-check (SOP 32, step 4) comes back clean instead of trading
one finding for a broken build.

**Verify.** **Not machine-verifiable by jig.** jig will confirm the *collision* cleared on
re-check, but it cannot see whether you updated every call site — that is precisely what the
graph's impact-radius query is for.

**Citation.** `advisor.rs` collision/generic-subset findings (the renames);
impact-radius / callers-callees navigation as the graph capability that makes a rename safe.

---

## Appendix A — quick-reference checklist

Every SOP as a one-line rule and its verification command. "n/a (not machine-checkable)"
marks rules jig cannot grade today — honest gaps, not oversights.

| # | Rule (one line) | Verify with |
|--:|:----------------|:------------|
| 1 | Tool names: 1–64 chars, `^[A-Za-z0-9_./-]+$`, no whitespace | `jig check` (protocol · tools-name-format) |
| 2 | No synonym-collision names (`get_`/`fetch_`, `list`/`enumerate`) | `jig check` / `jig budget --advise` |
| 3 | No generic-suffix pseudo-distinctions (`get_user` vs `get_user_info`) | `jig check` / `jig budget --advise` |
| 4 | One naming convention (kebab **or** snake) | `jig check` (description quality) |
| 5 | Descriptions written for *selection*, ~5–159 tokens | `jig check` (length) · `jig context` (read it); purpose is n/a |
| 6 | Every tool described; every param typed **and** described | `jig check` (schema hygiene) |
| 7 | Declare annotations (`readOnlyHint`, `destructiveHint`, …) | `jig check` (schema hygiene) |
| 8 | Give each tool a human `title` | `jig check` (description quality) |
| 9 | Tool count ≤30 comfort / 50 ceiling; split or Tool-Search | `jig check` (advisor · accuracy cliff) |
| 10 | No tool >3× median cost; no top-3 majority | `jig budget --advise` / `jig check` |
| 11 | Context budget ≤ median 1,679 tok; alarm past p90 14,401 | `jig budget --model gpt-4o` · `jig check` |
| 12 | Descriptions are an attack surface — no hidden instructions | `jig check` (tool-poisoning lint · reported, not scored); semantic attacks are n/a (Promptfoo / conformance suite) |
| 13 | stdout = only JSON-RPC; logs → stderr | `jig check` (protocol) · `jig inspect --tap` |
| 14 | Advertise only negotiated-revision capabilities | `jig check` (protocol · off-spec) |
| 15 | Back advertised capabilities with content | `jig inspect` (observed, not scored) |
| 16 | Unknown methods → `-32601` | `jig check` (protocol · negative) |
| 17 | Spec-valid `initialize` result (serverInfo + capabilities obj) | `jig check` (protocol · server-initialize) |
| 18 | Speak current revision; be version-aware toward the RC | `jig inspect` · `jig check` |
| 19 | Fail fast on missing credentials — never hang | `jig check` (startup / list-timeout, `--timeout`) |
| 20 | Shut down cleanly on transport close/EOF | `jig check` (robustness) |
| 21 | Answer every request; keep list latency low | `jig check` (robustness / protocol) |
| 22 | RFC 9728 metadata + proper 401 challenge (audience-bound) | `jig auth --http` |
| 23 | PKCE `S256` mandatory; DCR + `iss` recommended | `jig auth --http` |
| 24 | `npx`-runnable to handshake+list with no config | `jig info --probe` · `jig check` |
| 25 | Budget cold start; small deps, no postinstall network | `jig check` (robustness · `install X · boot Y`, boot scored, install reported) · `--tap` · `--no-prewarm` |
| 26 | Document env vars; safe credential UX; never log secrets | `jig servers` (redacted) · `jig check` / `jig info --probe` (credential-failure UX graded); docs/logging are n/a |
| 27 | stdio vs Streamable HTTP deliberately; plan sessions at scale | mostly n/a (jig follows session rules; scale is out of scope) |
| 28 | Eval suites in git (`.jig/`); score by selection rate | `jig eval` · `jig bench --save-case` |
| 29 | Gate CI on score **and** eval accuracy | `jig check --min-score` · `jig eval --gate` |
| 30 | Re-run suite on every model-version bump | `jig eval --json` (pinned context) |
| 31 | Publish report card + badge in PRs | `jig check --json` / `--badge` |
| 32 | Fix in source via code-graph query, not whole-file reads | n/a (jig gives findings + re-check; nav is external) |
| 33 | Check rename impact radius before applying collision fixes | n/a (graph impact-radius query) |

---

## Appendix B — what this guide's evidence actually is

Intellectual honesty is the point of this document, so here is the boundary between what
is measured, what is judged, and what will age. The census itself models this: it kept a
[public correction](../census/2026-07-19-state-of-mcp-servers.md) (the Azure/LaunchDarkly
stdout story in SOP 13) visible next to the original claim, because a correction teaches
more than the claim it fixes.

**Measured fact (with its limits).**
- The distribution numbers — min 124, median **1,679**, p75 7,595, **p90 14,401**, max
  **42,288** gpt-4o tokens; tool count median 14 / p90 28 / max 89; **42%** startup
  failure — come from the [2026-07-19 census](../census/2026-07-19-state-of-mcp-servers.md)
  of 50 servers, **n=29 reachable**, tokenized with gpt-4o `o200k_base` exactly. n=29 is a
  small sample: treat these as the best public numbers available, not population constants.
  The percentile dataset ([`data/percentiles.json`](../../data/percentiles.json)) still
  carries a maintainer note that its field names are provisional pending reconciliation at
  merge — the *values* are stable, the schema label is not yet frozen.
- The literature figures are external and dated: **97.1%** of tool descriptions "smelly"
  and **56%** failing to state purpose (arXiv:2602.14878, Feb 2026, 856 tools/103 servers);
  the **95%→71%** GitHub-MCP accuracy drop and ~42k-token GitHub surface (lunar.dev /
  getunblocked, 2026); **~8.5%** OAuth adoption (Astrix Research 2025, via Microsoft 2026).
  Each is cited inline; none was independently re-measured here.
- Anthropic token counts throughout are a labelled `~approx` (Claude's tokenizer is not
  public) unless produced with `jig budget --exact-anthropic`. OpenAI counts are exact.

**Editorial judgment (jig's tuned defaults, not laws).**
- The tool-count thresholds **30/50**, the cost-dominance ratios **3×/5×** and 200-token
  floor, the top-3 concentration **50%/70%**, the description bands **≤4 / ≥160 tokens**,
  the tool-name max **64 chars**, the stdout-pollution penalty **15/line (cap 60)**, and
  the rubric weights **25/25/20/15/15** are all documented constants in
  [`check.rs`](../../crates/jig-core/src/check.rs) /
  [`advisor.rs`](../../crates/jig-core/src/advisor.rs). They are anchored to the evidence
  above but remain choices. A different, defensible grader could pick differently.
- Which findings are *scored* vs *observed-only* is also a choice: description quality is
  labelled heuristic; robustness scores only what a single session observed; the tool-set
  advisor, the tool-poisoning lint and the auth dimension are **not** folded into the
  `rubric-v1.3` composite at all.

**What will age.**
- The **`2026-07-28` release candidate** is a moving target — final publication was
  scheduled for 2026-07-28 and the stateless core, extensions framework, Tasks-as-extension,
  and Tool-Search direction may shift between RC and final. SOPs 9, 14, 18, and 27 are
  written to be version-aware for exactly this reason; re-check them against the final spec.
- The `~8.5%` OAuth figure is a 2025 snapshot; the November-2025 revision made OAuth
  mandatory for internet-facing servers, so adoption should move.
- Vendor-reported code-graph token-reduction multiples (SOP 32) are benchmarks by their
  authors, not independently verified.

**What this guide could not evidence, and therefore does not claim.**
- No universal "correct" tool count, token budget, or description length exists — only
  distributions and thresholds, presented as such.
- jig still does **not** verify that your docs list your env vars, prove your server never
  logs a secret (both SOP 26), catch a *semantic* prompt injection — one written in plain
  English that carries none of the shapes the SOP 12 lint matches — or grade
  horizontal-scale session persistence (SOP 27). Those are named as gaps, not papered over.
