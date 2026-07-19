# The State of MCP Servers — July 2026

*A census of 50 popular, publicly-installable Model Context Protocol servers, measured with [`jig`](https://github.com/Shodh-Labs/jig).*

**Collected:** 2026-07-19 · **Servers attempted:** 50 · **Reachable:** 29 · **Failed:** 21 · **Tokenizer:** OpenAI `gpt-4o` (`o200k_base`, exact)

---

## What we did

We took 50 of the most-downloaded MCP servers on npm that install with a single `npx -y <package>` — no API key, no config file, no credential required to reach the MCP handshake — and we asked each one two questions:

1. **What do you expose?** (`jig inspect`) — protocol version, tools, resources, prompts, advertised capabilities.
2. **What do you cost?** (`jig budget`) — the number of context tokens your tool surface spends *before the user types a single word*.

We never call a tool. A server that lists its tools but needs a key to *use* them still counts — we only measure the context every model pays on connect. The full server list, the raw per-server results, and the sorted percentile samples all live in [`data/`](../../data); the exact method is at the bottom of this document.

Two numbers frame everything below:

> **The median reachable server costs 1,679 gpt-4o tokens to load. The heaviest costs 42,288 — 25× more — and the model pays that on every single turn.**

---

## Reachability: a third of "installable" servers won't start

Of 50 servers we could install, **29 completed the MCP handshake and 21 did not** — a 42% failure rate among packages that npm hands you without complaint. This is the headline finding for anyone building on MCP: *installable is not the same as runnable.*

The failures were not random. They fall into clean buckets:

| Failure mode | Count | Examples |
| --- | ---: | --- |
| Exits immediately demanding a credential | 14 | `@stripe/mcp`, `@sentry/mcp-server`, `@brave/brave-search-mcp-server`, `tavily`-style keys |
| Broke its own framing by writing to stdout | 2 | `@azure/mcp`, `@launchdarkly/mcp-server` |
| Needs a live backend (database / cluster) | 2 | `@benborla29/mcp-server-mysql`, `kubernetes-mcp-server` |
| Wrong entrypoint / platform | 1 | `xcodebuildmcp` (needs an `mcp` subcommand; macOS-only) |
| Hung indefinitely without ever responding | 2 | `@mapbox/mcp-server`, `@heroku/mcp-server` |

The credential-at-startup bucket is the big one, and it exposes a real design split in the ecosystem: **some servers refuse to start without a key; others hand a model their entire tool surface with no key at all.** `@modelcontextprotocol/server-github` lists all 26 of its tools before you authenticate; `@notionhq/notion-mcp-server` — well, see the next section. Neither posture is wrong, but a model (and its context budget) experiences them very differently.

---

## Context cost: the long tail is brutal

Across the 29 servers we could price, the distribution of tool-surface cost (gpt-4o tokens) is heavily right-skewed:

| | tokens |
| --- | ---: |
| Minimum (`@eslint/mcp`) | 124 |
| 25th percentile | 913 |
| **Median** | **1,679** |
| 75th percentile | 7,595 |
| 90th percentile | 14,401 |
| Maximum (`dataforseo-mcp-server`) | 42,288 |

Half the servers are genuinely cheap — under ~1,700 tokens, a rounding error in a modern context window. But the top decile is expensive enough to matter on every turn, and the tail is savage. The ten heaviest:

| Server | Tools | gpt-4o tokens |
| --- | ---: | ---: |
| `dataforseo-mcp-server` | 89 | 42,288 |
| `@notionhq/notion-mcp-server` | 24 | 17,430 |
| `firecrawl-mcp` | 26 | 16,891 |
| `@antv/mcp-server-chart` | 27 | 13,778 |
| `@wonderwhy-er/desktop-commander` | 26 | 11,057 |
| `@professional-wiki/mediawiki-mcp-server` | 29 | 8,349 |
| `@cyanheads/git-mcp-server` | 28 | 7,635 |
| `drawio-mcp-server` | 28 | 7,595 |
| `mcp-server-kubernetes` | 23 | 5,066 |
| `@shopify/dev-mcp` | 5 | 5,372 |

Tool *count* tracks cost loosely but not perfectly: `@shopify/dev-mcp` spends 5,372 tokens across just **5** tools (roughly 1,074 tokens each — verbose schemas), while `@microsoft/clarity-mcp-server` spends 4,300 across only **3**. Cost is about how much you describe each tool, not just how many you ship.

**Tool-count distribution (reachable servers):** min 1, median **14**, p90 28, max **89**.

---

## Stdout pollution: the classic MCP footgun, caught in the wild

MCP's stdio transport requires stdout to carry *only* newline-delimited JSON-RPC. Anything else — a startup banner, a stray `console.log`, a logger misconfigured to stdout — corrupts the framing. Jig watches for exactly this, and it found it:

- **4 of 50 servers wrote non-protocol lines to stdout.** For two of them it was fatal: **`@azure/mcp` emitted 288 non-protocol lines** and **`@launchdarkly/mcp-server` emitted 10**, and in both cases the pollution broke the handshake outright — they appear in our failure column not because they lack a feature, but because they talk over their own protocol channel.
- Two more polluted stdout but survived: **`@agentdeskai/browser-tools-mcp` (36 lines)** and **`mcp-server-code-runner` (1 line)** completed the handshake anyway, but a stricter client could reject them.

This is the single most common way an MCP server breaks in practice, and 8% of a popular sample doing it — 4% *fatally* — says it is still an unsolved footgun.

---

## Honesty of capabilities: advertised ≠ populated

A server announces its capabilities in the handshake (`tools`, `resources`, `prompts`, `logging`, …). We checked whether the ones it advertised were actually backed by content. **5 of 29 reachable servers advertised a capability they returned nothing for:**

| Server | Advertised but empty |
| --- | --- |
| `@upstash/context7-mcp` | `resources`, `prompts` |
| `@wonderwhy-er/desktop-commander` | `prompts` |
| `drawio-mcp-server` | `resources` |
| `@kazuph/mcp-fetch` | `resources` |
| `nx-mcp` | `resources` |

These are small gaps — a client that lists resources gets an empty array, not an error — but they are gaps: a model told "this server has prompts" and then handed none has been slightly misled.

At the other extreme, **`@assistant-ui/mcp-docs-server` exposes 299 resources** (its documentation corpus) while spending only 871 tokens on tools — a useful reminder that resource count and tool-token cost are entirely orthogonal axes. Only **6 of 29** servers ship a top-level `instructions` string at all.

---

## Protocol versions: adoption is nearly complete

Among reachable servers, **27 of 29 already speak the current `2025-06-18` protocol.** Only two still emit the older `2024-11-05`: the *deprecated* `@modelcontextprotocol/server-github` npm package (long since superseded by the Go `github-mcp-server`) and `@mantine/mcp-server`. Protocol drift, at least among actively-maintained servers, is not the ecosystem's problem. Startup reliability is.

---

## Five findings worth remembering

1. **The heaviest server costs 42,288 tokens before you say anything.** `dataforseo-mcp-server` ships **89 tools** and **21 prompts**; its tool surface alone is 25× the median server and a meaningful fraction of a mid-size context window — paid on every turn, whether or not the user needs SEO data.

2. **Two popular servers break their own handshake by talking over stdout.** `@azure/mcp` (288 stray lines) and `@launchdarkly/mcp-server` (10) fail to connect not for lack of a feature but because they pollute the one channel MCP reserves for protocol. This is the footgun `jig inspect` was built to name out loud.

3. **"Installable" hides a 42% startup-failure rate.** Twenty-one of fifty npm packages that install cleanly never complete a handshake — overwhelmingly because they demand a credential the instant they boot. Discovery tools that list these servers as "available" are describing a package, not a working server.

4. **`@assistant-ui/mcp-docs-server` exposes 299 resources** — the largest resource surface in the sample by two orders of magnitude — while remaining tool-cheap (871 tokens). Whatever heuristic you use to rank "big" servers, pick your axis deliberately.

5. **Some servers fail loudly; two fail by hanging.** `@mapbox/mcp-server` and `@heroku/mcp-server` neither list their tools nor exit with an error — they simply never respond, forcing a timeout. A server that blocks on startup without credentials is harder to diagnose than one that exits with `MISSING_TOKEN`, and worth calling out as an anti-pattern.

---

## Methodology

**Date:** 2026-07-19. **Machine:** the run in this report was collected on Windows 11 / Node v22.16.0; the weekly refresh (`.github/workflows/census.yml`) runs the identical script on `ubuntu-latest` / Node 22. Servers are fetched **unpinned** (`npx -y`, latest) on purpose, so the baseline tracks the ecosystem as it actually ships.

**The list.** 50 servers curated in [`data/census-servers.json`](../../data/census-servers.json), selected for: single-command `npx` install with no credential required for the handshake; active publication (latest release within ~12 months, with a few deliberately-kept deprecated official servers as drift data points); measurable npm download traffic; and authorship diversity (official `@modelcontextprotocol` servers capped at 8 of 50). Inclusion is not endorsement, and is explicitly not a claim the server works — the census records honest pass/fail.

**The measurement.** For each server, [`scripts/census/run-census.js`](../../scripts/census/run-census.js) shells out to the `jig` binary twice over stdio:

```sh
jig inspect --stdio "npx -y <package> [args]" --json --timeout 45
jig budget  --stdio "npx -y <package> [args]" --json --model gpt-4o --timeout 45
```

with a ~90-second hard wall per invocation and one full retry per server on failure. `server-filesystem` receives a freshly-created temporary directory as its required path argument. Token counts are gpt-4o `o200k_base`, computed exactly (not approximated) over jig's deterministic canonical tool rendering.

**Exclusions.** The 21 servers that failed the handshake are **excluded from every percentile and distribution** above (you cannot price a surface that never rendered) but are **fully retained** in [`data/census-raw.json`](../../data/census-raw.json) with the reason for each failure. A census that silently dropped its failures would misrepresent the ecosystem as healthier than it is; the 42% failure rate *is* a finding. The two timeout failures (`@mapbox`, `@heroku`) were re-run once by hand with a 110-second window and confirmed to hang rather than flake.

**Reproduce it:**

```sh
cargo build -p jig-cli
node scripts/census/run-census.js
```

Raw per-server data: [`data/census-raw.json`](../../data/census-raw.json) · Percentile samples consumed by `jig check`: [`data/percentiles.json`](../../data/percentiles.json).

*Wall-clock time for the full 50-server run: 27 minutes.*
