# Jig

> A jig holds the workpiece and guides the tool, so every cut is repeatable.
> Your MCP server is the workpiece. The model is the tool.

**Jig is a testing workbench for MCP servers** — see what the model sees, measure what your server costs in context, and test what a model *actually does* with your tools.

![Jig workbench — request/response spans from a live MCP session](docs/media/workbench-wire.png)

<p align="center"><em>A real session against <code>@modelcontextprotocol/server-everything</code>, rendered from jig's protocol tap:
the 8-second npx cold start folded out of the way, an unsolicited mid-request notification caught on the wire,
payload inspector open. Workbench UI in development — the CLI ships today.</em></p>

## Why

MCP servers can't be tested like APIs. An API is deterministic: send a request, assert on the response. An MCP server "works" only if a **model** understands your tool descriptions, selects the right tool for a task, and fills the arguments correctly. That's a probabilistic surface — and today, everyone tests it by poking their server in a chat client and eyeballing the result.

Jig makes that surface visible, measurable, and regression-safe:

- 🔍 **Inspect** — the exact rendered context a model receives from your server. Not what your code says; what the wire says.
- 🧮 **Token budget** — what your server costs in context before the user types a word, per tool, per model tokenizer.
- 🎯 **Model bench** — give it a task in plain language; watch which tool a real model selects, with what arguments, across repeated runs.
- ✅ **Eval suites** — `prompt → expected tool call` test cases, versioned in git next to your server, runnable locally and in CI.

## Status

🚧 **Early development.** Building in public — watch the repo, or open an issue to tell us how you test your MCP server today (we read everything).

## Roadmap (short version)

1. Desktop workbench: connect (stdio / streamable HTTP), inspect, token budget, direct invoke, model bench — *in progress*
2. Local eval suites (`.jig/`) with honest, statistical scoring — selection rate across N runs, never single-run pass/fail
3. CI: `jig run` in your pipeline, PR annotations, regression gates

## Development

Jig is a Rust workspace (`cargo` 1.80+). Milestone 1 ships the core engine and the `jig` CLI.

```
crates/
  jig-core         # library: stdio + Streamable HTTP transports, MCP handshake + ops, protocol tap
  jig-cli          # binary `jig`: inspect / call / budget / bench / eval subcommands
  jig-mock-server  # binary: a minimal MCP server (stdio + HTTP) used as a test fixture
```

Build and test the whole workspace:

```sh
cargo build
cargo test          # unit + integration tests (spawns the mock server)
cargo fmt --all
cargo clippy --all-targets
```

Try the CLI against the bundled mock server:

```sh
cargo build
# inspect what a server exposes (add --json for full machine output)
./target/debug/jig inspect --stdio "./target/debug/jig-mock-server" --tap traffic.jsonl
# invoke a single tool
./target/debug/jig call --stdio "./target/debug/jig-mock-server" \
    --tool echo --args '{"text":"hello"}'
# what does this server cost in context tokens, per tool, per model?
./target/debug/jig budget --stdio "./target/debug/jig-mock-server"
# which tool does a real model pick for a task? (needs ANTHROPIC_API_KEY / OPENAI_API_KEY)
./target/debug/jig bench --stdio "./target/debug/jig-mock-server" \
    --task "Book a table for two tonight" --model gpt-4o --runs 5
```

### Transports: stdio and Streamable HTTP

Every command takes **either** `--stdio "<command>"` (launch a local server as a
subprocess) **or** `--http <url>` (connect to a remote MCP endpoint over the
[Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)
transport) — the two are mutually exclusive. The protocol tap captures
HTTP-carried traffic identically to stdio.

```sh
# a remote server over Streamable HTTP (JSON or SSE responses, both handled)
jig inspect --http "https://example.com/mcp"
# many remote/SaaS servers need auth — pass headers with --header (repeatable)
jig inspect --http "https://api.example.com/mcp" \
    --header "Authorization: Bearer $TOKEN"
jig call --http "https://example.com/mcp" --tool echo --args '{"message":"hi"}'
```

Session handling follows the spec: jig captures the `Mcp-Session-Id` issued at
`initialize` and echoes it (plus the negotiated `MCP-Protocol-Version`) on every
later request, and sends an HTTP `DELETE` to end the session on shutdown. If the
server reports the session expired (HTTP 404), jig surfaces a clear error rather
than silently reconnecting — it is a diagnostic tool and tells the truth. OAuth
flows are not yet implemented; supply a token via `--header` for now.

### `jig budget` — the token-budget engine

`jig budget` answers a question no model client shows you: **what does an MCP
server cost in context tokens, before you type a word?** It prices the *tools
array as sent to the API* — for each tool the compact JSON `{name, description,
input_schema}` — plus the server's `instructions` field, per tool and totalled.

```sh
jig budget --stdio "npx -y @playwright/mcp@latest"          # default table
jig budget --stdio "<cmd>" --model gpt-4o --model gpt-4     # a column per model
jig budget --stdio "<cmd>" --markdown                       # shareable card for a PR/tweet
jig budget --stdio "<cmd>" --json                           # machine output + exactness metadata
jig budget --stdio "<cmd>" --model claude-sonnet --exact-anthropic  # exact Claude total via the API
```

**Accuracy honesty is a hard rule.** Numbers are exact where we can be exact and
*clearly labelled* where we cannot:

- **OpenAI is exact.** `gpt-4o` (`o200k_base`) and `gpt-4` (`cl100k_base`) use
  the real `tiktoken` tokenizers — labelled `exact`.
- **Anthropic is a labelled approximation by default.** Claude 3+ has no public
  local tokenizer, so Jig counts with `o200k_base` as a proxy and labels every
  such number `~approx`. With `--exact-anthropic` and `ANTHROPIC_API_KEY` set,
  Jig calls Anthropic's official `count_tokens` endpoint for an exact *total*
  (the endpoint reports a request-level total, not a per-tool breakdown, so the
  per-tool rows stay `~approx` while the total becomes `exact`). Network errors
  degrade back to the approximation with a warning — never a crash, and the key
  is never logged.

Known models: `gpt-4o`, `gpt-4`, `claude-sonnet`, `claude-opus` (adding one is a
single registry entry in `jig-core`'s `tokens` module). Output is deterministic
(stable sort, ties by name) so it can be diffed in CI. See
[`docs/token-budget.md`](docs/token-budget.md) for the exact **canonical
rendering** definition — what bytes get counted.

`--stdio` takes the full server command (double-quote paths containing spaces).
On Windows, `npx`/`npm` shims resolve automatically, so
`jig inspect --stdio "npx -y @modelcontextprotocol/server-everything"` just works.
`--tap <file>` writes every raw JSON-RPC message, both directions, as JSONL —
the protocol tap that makes a session inspectable and regression-safe.
`--timeout <seconds>` bounds every request (default `30`, `0` = wait forever) so a
server that accepts a request and never answers fails fast instead of hanging.
Exit codes: `0` success, `2` when a tool reports an error, non-zero otherwise.

Jig has been validated against a battery of real public MCP servers — see
[`docs/compat/`](docs/compat/) for the compatibility report.

### `jig bench` — the model-in-the-loop bench

`jig budget` prices a tool surface statically. `jig bench` does something no
other tool does: it puts a **real model in the loop** and measures which tool
that model actually picks for a natural-language task, with what arguments,
across repeated runs. MCP integration is probabilistic — the same task can
select different tools on different runs — and `jig bench` makes that visible and
measurable.

```sh
jig bench --stdio "<cmd>" --task "Find the docs page about rate limits" \
    --model claude-sonnet --runs 5
jig bench --stdio "<cmd>" --task "<task>" --model gpt-4o --json   # full machine output
jig bench --http "https://example.com/mcp" --task "<task>" --model claude-opus
```

What it does, honestly:

1. Connects to the server (any existing transport) and lists its tools.
2. Assembles a **real** tool-use API request — the server's tools mapped to the
   provider's function-calling format, the task as the user message, and a
   single documented system-prompt constant (visible in `--json`; it is part of
   the methodology, not a black box).
3. Sends it `--runs` times (default `3`, sequential) at `--temp` (default `1.0`,
   always recorded).
4. Classifies each response into the outcome taxonomy: `selected` (which tool +
   args), `no_tool` (answered in text), `hallucinated_tool` (a name the server
   doesn't expose), or `provider_error` (an API failure after bounded retries).
5. Validates a selected call's arguments against the tool's JSON Schema (types,
   `required`, `enum`, nested objects) and reports the **distribution** plus a
   per-run table and a one-line takeaway (`consistent` vs `UNSTABLE`).

```
Distribution:
  search_docs  4/5 (80%)
  fetch_page   1/5 (20%)

Takeaway: UNSTABLE: tool selection varied across runs (2 different tools) — see per-run detail
```

**Bring your own key.** `jig bench` calls a real provider, so it needs a key
read **from the environment only**: `ANTHROPIC_API_KEY` for `claude-*` models,
`OPENAI_API_KEY` for `gpt-*`. A missing key fails fast with a clear message
naming the variable, *before* connecting to the server. The key is never logged,
never written to `--json`, never placed in an error message, and never in the
rendered request (auth rides in a request header, not the body). The default
model is `claude-sonnet` if `ANTHROPIC_API_KEY` is set, else `gpt-4o`; override
the concrete API model string with `--api-model` (hardcoded mappings age).

**What it measures — and doesn't.** It measures how a specific model, at a
specific temperature, maps *your* task onto *this* server's tool surface, and how
stable that mapping is across runs. It does **not** execute the selected tool,
judge whether the selection was "correct", or benchmark model quality in the
abstract — it is a microscope on the tool-selection step of MCP integration, with
every input (system prompt, rendered request, temperature, N) and every raw
provider response inspectable via `--json`.

Two provider dialects are supported, verified against the current provider docs:
the **Anthropic Messages API** (`tools` / `tool_use` blocks) and **OpenAI Chat
Completions** (`tools` / `tool_calls`). Rate-limit (`429`) and server (`5xx`)
responses are retried with bounded back-off (respecting `Retry-After`); a
persistent failure degrades into a `provider_error` outcome rather than crashing
the bench — a misbehaving provider is Jig's to handle informatively, exactly as
a misbehaving server is.

### `jig eval` — the `.jig` eval suite

`jig bench` explores. `jig eval` **asserts**: it turns `prompt → expected tool
call` cases into regression tests, versioned in git next to your server and
runnable in CI. Cases live in a `.jig/` directory as `*.yaml` files:

```yaml
# .jig/search.yaml
suite: search-basics            # optional; defaults to the file stem
defaults:                       # optional per-suite defaults
  runs: 3
  temp: 1.0
cases:
  - id: find-rate-limits        # required, unique within the suite
    task: "Find the docs page about rate limits"
    expect:
      tool: search_docs                     # the expected selection (required)
      args:                                 # optional argument matchers
        query: { contains: "rate limit" }   # exact | contains | regex | one_of | range
        limit: { range: { min: 1, max: 50 } }
      not_tools: [fetch_page]               # selecting any of these = hard fail
    runs: 5                                 # override the default
    min_rate: 0.8                           # selection-rate gate (default 0.8)
    must_pass: false                        # true → its failure fails the whole run
```

A bare scalar is `exact` shorthand: `query: "rate limit"` ≡
`query: { exact: "rate limit" }`. An **unknown field is a hard error** naming the
field and file — a silently-ignored typo in a test file is a lying test. The
selected call's arguments are always JSON-Schema validated (the same validator
`jig bench` uses) in addition to any matchers.

```sh
jig eval --stdio "<cmd>"                        # runs every ./.jig/*.yaml
jig eval --stdio "<cmd>" --suite .jig/search.yaml --gate 0.9
jig eval --stdio "<cmd>" --json                 # full per-run detail
jig eval --stdio "<cmd>" --junit eval.xml       # CI-native JUnit XML
```

Each case runs N times and is scored by a **selection rate** = the fraction of
runs that selected the expected tool *and* passed every matcher *and* were
schema-valid — never a single run, never a boolean. A case at or above its
`min_rate` passes; a case that flips between runs is flagged **FLAKY** even when
it passes, because flakiness is a finding, not noise. Provider errors are
**excluded** from the rate denominator and reported loudly; a case that is
mostly provider errors is `ERRORED`, not silently passed.

```
Suite: search-basics  (.jig/search.yaml)
  case              rate        runs  verdict
  find-rate-limits  3/3 (100%)     3  PASS
  list-endpoints    2/4 (50%)      4  PASS · FLAKY
  book-table        0/3 (0%)       3  FAIL · must_pass

Totals:
  accuracy:  5/10 (50%)
  cases:     2 passed · 1 failed · 1 flaky · 0 errored
  gate:      0.80 → NOT MET (50% < 80%)
  verdict:   FAIL
```

The run fails (**exit `3`**) when a `must_pass` case does not pass, or when a
`--gate` is set and the overall weighted accuracy falls below it. Without a gate
and without any `must_pass` case there is nothing to enforce, so `jig eval` is
informational (exit `0`) — add `--gate` or mark cases `must_pass` to turn a suite
into a CI regression gate. Every report ends with a **pinned-context block**
(model id + *reported* version, temperature, runs, suite files, system prompt,
and the deterministic-scoring rubric) so a run is reproducible.

**Honest framing.** v1 scoring is **deterministic matchers only** — `exact`,
`contains`, `regex`, `one_of`, `range`, tool name, and schema validity. There is
deliberately **no LLM-judge**: a regression gate must itself be reproducible.

#### The save-case loop: exploration → regression test

`jig bench --save-case <file>` closes the loop. After a bench run it drafts a
case into a `.jig` file (creating the file and any parent directory), with the
expected tool and `exact` argument matchers taken from the majority run:

```sh
# 1) explore
jig bench --stdio "<cmd>" --task "Book a table for two outdoors on 2026-01-01" \
    --model gpt-4o --runs 3 --save-case .jig/reservations.yaml
# → drafts:
#   # TODO: review drafted case
#   - id: book-a-table-for-two-outdoors
#     task: "Book a table for two outdoors on 2026-01-01"
#     expect:
#       tool: make_reservation
#       args:
#         date: "2026-01-01"
#         party: { exact: {"seating":"outdoor","size":2} }
#     runs: 3
# 2) review the TODO, tighten the matchers, commit — now it's a regression test
jig eval --stdio "<cmd>" --suite .jig/reservations.yaml
```

Drafting is refused (nothing is written) when the majority outcome was not a tool
selection — there is no selection to assert.

## License

[Apache 2.0](LICENSE) — permanently. Everything in this repository (core library, CLI, mock server, and the `.jig` suite format) is and will remain Apache 2.0; no relicensing, no license switch later.

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/) — sign off your commits with `git commit -s`. See [CONTRIBUTING.md](CONTRIBUTING.md).

---

Built by [Shodh Labs](https://github.com/Shodh-Labs).
