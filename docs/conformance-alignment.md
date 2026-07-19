# Jig ↔ official MCP conformance suite alignment

The Model Context Protocol project ships an official conformance suite at
[`modelcontextprotocol/conformance`](https://github.com/modelcontextprotocol/conformance)
— the harness that now gates SEP promotion. This document maps its **server-side
scenarios** onto what `jig check`'s protocol-compliance dimension actually
verifies, honestly, so we ride the standard instead of inventing a parallel one.

## Scope: what jig can and cannot cover

The official suite drives a server over a live transport and can run
multi-request, multi-session, and client-feature (sampling/elicitation)
scenarios. `jig check` is deliberately a **single connect → inspect → one-probe
session**: one `initialize` handshake, paginated `tools`/`resources`/`prompts`
listing, and one deliberately-bogus method probe. So jig can cover the
*structural, single-session, server-emits-it* scenarios well, and cannot cover
client-driven or multi-session ones. Scenarios below are marked accordingly.

`jig` also carries checks the official suite does **not** — most importantly
**stdout-framing pollution**, which only exists on the stdio transport the
conformance suite does not exercise. That check stays jig-unique and remains our
headline finding.

## Mapping table

Scenario ids/names are the suite's own (`src/scenarios/server/…`,
`ClientScenario.name`). Status is one of **Covered**, **Partial**, **Not
covered** (with the reason it is out of jig's one-session model).

| Official server scenario | Spec refs | jig equivalent | Status |
| --- | --- | --- | --- |
| `server-initialize` (lifecycle) | MCP-Initialize | Handshake completes; `initialize` result validated for a non-empty `serverInfo.name`/`version` and an object `capabilities` map. | **Covered** (session-id visible-ASCII sub-check is HTTP-only → not covered) |
| `tools-list` | MCP-Tools-List | `tools/list` is called and paginated; each tool's `name`/`description`/`inputSchema` feed the protocol + schema-hygiene checks. | **Covered** |
| `tools-name-format` | SEP-986 | New protocol check: every tool name must be 1–64 chars and match `^[A-Za-z0-9_./-]+$`. | **Covered (implemented this milestone)** |
| `negative` (unknown method) | JSON-RPC 2.0 §5.1 | New protocol check: probe a bogus method; require `-32601 Method not found` (flag any other code, or a spurious success). | **Covered (implemented this milestone)** |
| Capability legality | schema `ServerCapabilities` per revision | Version-aware capability table (`2024-11-05`→`2026-07-28`): a capability is off-spec only relative to the *negotiated* revision. | **Covered (jig-specific, hardened this milestone)** |
| `json-schema-2020-12` | SEP-1613, SEP-2106 | Schema hygiene inspects each tool's `inputSchema` for types/descriptions/annotations, but does **not** verify 2020-12 keyword *preservation* (jig observes the server's own tools; it does not inject a reference tool). | **Partial** |
| `prompts` | MCP-Prompts | `prompts/list` + `prompts/get` are supported by the client, but there is no dedicated prompt-conformance scoring. | **Partial** |
| `resources` | MCP-Resources | `resources/list` + `resources/read` are supported, but there is no dedicated resource-conformance scoring. | **Partial** |
| `negative-mrtr` | (missing-required tool-result) | Requires driving tool-call result validation across requests. | **Not covered** (multi-request) |
| `caching` | MCP caching | Cache semantics need repeated correlated requests. | **Not covered** (multi-request) |
| `elicitation-defaults`, `elicitation-enums` | MCP elicitation | Server→client elicitation requires a client that answers; jig does not act as an elicitation client. | **Not covered** (client-side) |
| `input-required-result`, `input-required-result-helpers` | MCP tools (input-required) | Multi-step input-required tool flow. | **Not covered** (multi-step) |
| `server-stateless` / `stateless` | SEP-2575 | Stateless HTTP architecture (per-request `_meta`, `server/discover`, header errors). | **Not covered** (HTTP + multi-request) |
| `http-standard-headers` | MCP transports | HTTP header conformance on the Streamable HTTP transport. | **Not covered** (HTTP header semantics) |
| `dns-rebinding` | MCP security | `Origin`-header / DNS-rebinding defense. | **Not covered** (HTTP security) |
| `sse-multiple-streams`, `sse-polling` | MCP transports | Server→client SSE stream behavior (jig has `inspect --listen` but no conformance scoring). | **Not covered** (HTTP SSE) |
| `tasks` | Tasks extension (2025-11-25 experimental → 2026-07-28 extension) | Not exercised as a lifecycle; but the capability table recognizes `tasks` per revision so it is not mis-flagged. | **Not covered** (multi-request lifecycle) |

## Summary counts

| Status | Count | Scenarios |
| --- | --- | --- |
| Covered | 5 | `server-initialize`, `tools-list`, `tools-name-format`, `negative`, capability legality |
| Partial | 3 | `json-schema-2020-12`, `prompts`, `resources` |
| Not covered | 11 | `negative-mrtr`, `caching`, `elicitation-defaults`, `elicitation-enums`, `input-required-result`, `input-required-result-helpers`, `stateless`, `http-standard-headers`, `dns-rebinding`, `sse-multiple-streams`+`sse-polling`, `tasks` |

The "Not covered" set is dominated by **client-side**, **multi-request**, and
**HTTP-transport** scenarios that are outside jig's one-session-observer model by
design. The gains worth chasing next, all single-session, are the two **Partial**
rows: injecting a reference JSON-Schema-2020-12 tool is not possible (jig does not
control the server), but a dedicated prompts/resources structural pass is.

## Checks implemented this milestone (from the suite's own definitions)

1. **`tools-name-format` (SEP-986)** — tool names must be 1–64 characters and
   match `^[A-Za-z0-9_./-]+$`. Finding text cites the scenario id and SEP.
2. **`server-initialize` (MCP-Initialize)** — the `initialize` result must carry
   a non-empty `serverInfo.name`/`version` and an object `capabilities` map.
3. **`negative` (JSON-RPC 2.0 §5.1)** — an unknown method must be answered with
   `-32601 Method not found`; a different error code or a spurious success is a
   finding.

Each finding names the scenario in its message so a user can cross-reference the
official suite.
