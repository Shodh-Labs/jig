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
  jig-cli          # binary `jig`: check / inspect / call / read / prompt / budget / bench / servers / search / info
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
# the one-command report card: score everything in one session
./target/debug/jig check --stdio "./target/debug/jig-mock-server"
# inspect what a server exposes (add --json for full machine output)
./target/debug/jig inspect --stdio "./target/debug/jig-mock-server" --tap traffic.jsonl
# invoke a single tool
./target/debug/jig call --stdio "./target/debug/jig-mock-server" \
    --tool echo --args '{"text":"hello"}'
# read a resource by URI (text prints verbatim; a blob prints its mimeType + base64 length)
./target/debug/jig read --stdio "./target/debug/jig-mock-server --resources-prompts" \
    --uri "mock://text/hello"
# fetch a prompt by name, filling its arguments
./target/debug/jig prompt --stdio "./target/debug/jig-mock-server --resources-prompts" \
    --name greet --args '{"name":"Ada"}'
# what does this server cost in context tokens, per tool, per model?
./target/debug/jig budget --stdio "./target/debug/jig-mock-server"
# which tool does a real model pick for a task? (needs ANTHROPIC_API_KEY / OPENAI_API_KEY)
./target/debug/jig bench --stdio "./target/debug/jig-mock-server" \
    --task "Book a table for two tonight" --model gpt-4o --runs 5
```

### `jig check` — the one-command report card

`jig check` is the front door: one command connects once and runs everything Jig
knows, then renders a **scored verdict with a to-do list** — Lighthouse for MCP
servers. Instruments require interpretation; a grade plus ranked fixes converts.

```sh
jig check --stdio "npx -y @playwright/mcp@latest"     # human report card
jig check --stdio "<cmd>" --json                       # full findings + per-dimension scores
jig check --stdio "<cmd>" --badge                      # shields.io endpoint JSON for a README badge
jig check --stdio "<cmd>" --min-score 80               # CI gate: exit nonzero below the floor
jig check --http "https://example.com/mcp" --header "Authorization: Bearer $TOKEN"
```

One session, one grade over five weighted dimensions (`rubric-v1`):

| Dimension | Weight | What it scores |
|:----------|-------:|:---------------|
| Protocol compliance | 25 | handshake, stdout framing/pollution, spec-valid capabilities, timeouts |
| Context cost | 25 | gpt-4o exact total tokens (percentile vs the ecosystem, or absolute bands) |
| Schema hygiene | 20 | per tool: descriptions, param types & descriptions, annotations |
| Description quality | 15 | *heuristic* — description length, name consistency, titles (no LLM) |
| Robustness | 15 | *observed only* — list latency, clean shutdown |

Every deduction is a typed finding carrying the fix, and the three highest-impact
fixes are surfaced up top:

```
jig check · jig-mock-server v0.1.0
protocol 2025-06-18 · rubric-v1

  ✓  98 / 100   grade A

  ✓  Protocol compliance  100   clean handshake, no stdout pollution, spec-valid capabilities
  ✓  Context cost          99   183 tokens (no ecosystem data — absolute bands)
  ✓  Schema hygiene        94   `make_reservation`: parameter `party` missing a description (+1 more)
  ✓  Description quality   97   heuristic · 3 tool(s) have no human-facing title
  ✓  Robustness           100   list 3ms, clean shutdown

Top fixes
  1. [schema_hygiene] `make_reservation`: parameter `party` missing a description
     → describe each parameter of `make_reservation` so the model can fill it correctly
  2. [schema_hygiene] 3 tool(s) declare no annotations (readOnlyHint, destructiveHint, …)
     → add tool annotations so clients can reason about side effects
  3. [description_quality] 3 tool(s) have no human-facing title
     → add a `title` to each tool for nicer client display

Description quality is heuristic (deterministic, no LLM).
Context cost scored with absolute bands (no ecosystem dataset available).
```

**Honest scoring.** Context cost is graded against an ecosystem percentile
dataset when `data/percentiles.json` is present (the report then says e.g.
`94th percentile (heaviest 6%)`); absent, it uses documented absolute bands and
says so. Description quality is deterministic heuristics — labelled "heuristic",
never an LLM verdict — and robustness scores only what was actually observed in
the session, never assumed. `--min-score <n>` is the CI gate: wire
`jig check --stdio "<cmd>" --min-score 80` into a pipeline and a regression that
drops the grade fails the build. See
[`docs/percentiles-schema.md`](docs/percentiles-schema.md) for the dataset
format.

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

#### Listening for server-initiated messages (`--listen`)

Per the spec a Streamable HTTP server MAY open a standalone `GET` SSE stream to
push **notifications** and even server→client **requests** (`ping`,
`sampling/createMessage`, `roots/list`, …). `jig inspect --http … --listen`
opts in: after listing, jig holds that stream open for `--duration` seconds and
reports what arrived — everything captured in the tap.

```sh
# after inspecting, watch the server stream for 10s
jig inspect --http "https://example.com/mcp" --listen --duration 10 --tap traffic.jsonl
# => GET stream: opened (HTTP 200). Observed 2 notification(s), 1 server ping(s),
#    0 other server request(s) in 10.0s. (See the tap for full detail.)
```

Listening is **off by default** (a diagnostic tool does what you ask). jig
advertises no client capabilities, so it answers each server→client request
honestly: an empty result for `ping` (per spec), and JSON-RPC
`-32601 method not found` for anything else — each exchange tapped both
directions. A server that offers no stream answers the `GET` with HTTP 405,
which jig records as spec-permitted, not an error.
### Discovery: `jig servers`, `jig search`, `jig info`

Three verbs close the loop from *"what MCP servers exist / what do I already
have?"* to *"inspect it"*.

**`jig servers`** — the MCP servers already configured on this machine, merged
and labelled by source. It reads the config files the popular clients write:

| Source          | Location |
|-----------------|----------|
| `claude-desktop`| `%APPDATA%\Claude\claude_desktop_config.json` (Windows), `~/Library/Application Support/Claude/…` (macOS), `~/.config/Claude/…` (Linux) |
| `claude-code`   | `~/.claude.json` (top-level **and** per-project `mcpServers`) |
| `project`       | `.mcp.json`, searched from the current directory upward |
| `cursor`        | `~/.cursor/mcp.json` |
| `vscode`        | `.vscode/mcp.json` (`servers` or `mcpServers` key — both accepted) |

Foreign files are parsed tolerantly: unknown fields are ignored and a malformed
file becomes a warning, never a crash. **Environment-variable values are always
redacted** (`KEY=•••`) in both the table and `--json` — only key names are shown,
because these blocks routinely hold API keys and tokens.

```sh
jig servers            # human table
jig servers --json     # machine-readable (env values redacted)
```

Any connecting verb can then reach a discovered server by name with
`--server <name>` (mutually exclusive with `--stdio`/`--http`); its declared
`env` block is passed to the spawned process. Use `--server <source>:<name>`
when the same name appears in more than one source:

```sh
jig inspect --server github
jig budget  --server project:my-server
```

**`jig search <query>`** — search the MCP ecosystem. Two sources are queried
**concurrently** and merged (registry first), each result labelled with its
source and version:

* the **official MCP registry** (`registry.modelcontextprotocol.io`, `GET
  /v0/servers?search=…`), and
* **npm** (`/-/v1/search`), filtered to plausible MCP servers — a hit is kept
  when its name contains `mcp` or its keywords include `mcp` / `model context
  protocol`.

A network failure of one source degrades gracefully: the other source's results
are still shown, the failure is reported (`registry unreachable`), and the exit
code is `0` if *any* source succeeded (`1` only if all failed).

```sh
jig search filesystem
jig search github --source npm --limit 10
jig search database --json
```

**`jig info <name>`** — detailed info for one server/package: the registry entry
(if found) and npm metadata (description, version, published date, install
command), each labelled by source with *"not found in X"* stated plainly.

```sh
jig info @modelcontextprotocol/server-everything
jig info some-mcp-server --probe --yes
```

#### `--probe` and its security model

`jig info --probe` doesn't just read metadata — it **actually runs the server**
(`npx -y <pkg>` over stdio) and reports the live handshake: `serverInfo`,
protocol version, capabilities, the tool count + names, and the total context
cost in tokens (reusing the [budget engine](#jig-budget--the-token-budget-engine),
gpt-4o, exact).

This runs **third-party code from npm on your machine.** Because `jig` is a
non-interactive CLI (no TTY prompt), the consent mechanism is a printed notice
plus a short delay before anything executes:

```
jig: probe runs third-party code from npm on your machine (npx -y <pkg>) — Ctrl-C to abort
```

You have a 2-second window to `Ctrl-C`. `--yes` skips the delay for scripted use.
Without `--probe`, `jig info` performs read-only metadata lookups and never
executes anything.

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

## License

[Apache 2.0](LICENSE) — permanently. Everything in this repository (core library, CLI, mock server, and the `.jig` suite format) is and will remain Apache 2.0; no relicensing, no license switch later.

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/) — sign off your commits with `git commit -s`. See [CONTRIBUTING.md](CONTRIBUTING.md).

---

Built by [Shodh Labs](https://github.com/Shodh-Labs).
