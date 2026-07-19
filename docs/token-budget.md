# Token budget — canonical rendering (V1)

`jig budget` reports what an MCP server costs in **context tokens**. This note
pins down exactly what bytes are counted and how the numbers are labelled, so
the figure is never a black box.

## What gets counted

Providers tokenize the **tools array as sent to the API**. Jig counts the same
thing. For each tool the unit of counting is its **canonical rendering**:

> **V1 canonical rendering** — for each tool, a **compact** (no insignificant
> whitespace) JSON object with keys `{name, description, input_schema}`, where
> `description` is **omitted** when the tool declares none, serialized with
> **lexicographically sorted keys** for determinism. A tool's token count is the
> token count of that JSON string.

In addition, the server's `instructions` field (from the `initialize` result),
if present, is counted **verbatim** as its own line.

- **Per-tool count** = tokens of that tool's canonical JSON.
- **Instructions count** = tokens of the raw `instructions` string.
- **Grand total** = sum of every per-tool count **plus** the instructions count.

Sorted keys make the rendering (and therefore the count) independent of the
order a server happened to send fields in — the output is byte-stable and
diffable in CI.

Example canonical rendering of a simple `echo` tool:

```json
{"description":"Echo the provided text straight back.","input_schema":{"properties":{"text":{"description":"Text to echo.","type":"string"}},"required":["text"],"type":"object"},"name":"echo"}
```

The rendering step is a single pluggable function (`jig_core::tokens::canonical_tool_json`).
Client-specific rendering variants (e.g. how a particular client frames tools in
the wire prompt) are a future milestone — V1 counts the canonical tools array.

## Exactness labelling

Every number carries an exactness flag; approximations are always labelled.

| Model family | Tokenizer | Label |
|---|---|---|
| OpenAI `gpt-4o` (and o-series lineage) | `o200k_base` (`tiktoken`, exact) | `exact` |
| OpenAI `gpt-4` / `3.5` lineage | `cl100k_base` (`tiktoken`, exact) | `exact` |
| Anthropic `claude-*` (default) | `o200k_base` used as a **proxy** — Claude's tokenizer is not public | `~approx` |
| Anthropic `claude-*` with `--exact-anthropic` | official `count_tokens` endpoint | `exact` **total** (per-tool rows stay `~approx`) |

### The Anthropic approximation

Anthropic does not publish a local tokenizer for Claude 3+. By default Jig
approximates a Claude count with the `o200k_base` tokenizer and labels it
`~approx` in every output surface (table, markdown, JSON `method` field).

`--exact-anthropic` (requires `ANTHROPIC_API_KEY`) calls
`POST /v1/messages/count_tokens` for an **exact grand total**. That endpoint
returns a single request-level total, not a per-tool breakdown, so:

- the **total** is labelled `exact` (from the API), and
- the **per-tool rows remain `~approx`** (the local `o200k` proxy).

Two calls are made — a baseline (minimal message, no tools) and the full request
(tools + instructions) — and the difference isolates the tools + instructions
contribution from message framing. Any network/auth error degrades back to the
labelled approximation with a warning; the command still exits `0`. The API key
is read only from the environment and is never logged, echoed, or placed in an
error message.

## Estimating the Anthropic deviation

The `o200k_base` proxy is not the Claude tokenizer, so the default Anthropic
column is an estimate. To measure the deviation on a specific server, run the
same server twice and compare the totals:

```sh
jig budget --stdio "<cmd>" --model claude-sonnet --json               # ~approx total
jig budget --stdio "<cmd>" --model claude-sonnet --exact-anthropic --json  # exact total
```

The ratio `exact / approx` is the deviation for that tool surface. We expect it
to sit near 1.0 for English tool descriptions and drift with heavy punctuation,
code, or non-English content — but it should always be *measured*, never
assumed. (A documented fixed adjustment multiplier is intentionally **not**
baked in: without a measured basis it would be a fabricated number, which the
accuracy-honesty rule forbids.)
