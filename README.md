# Jig

> A jig holds the workpiece and guides the tool, so every cut is repeatable.
> Your MCP server is the workpiece. The model is the tool.

**Jig is a testing workbench for MCP servers** — see what the model sees, measure what your server costs in context, and test what a model *actually does* with your tools.

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
  jig-core         # library: stdio JSON-RPC transport, MCP handshake + ops, protocol tap
  jig-cli          # binary `jig`: inspect / call / budget subcommands
  jig-mock-server  # binary: a minimal MCP server used as a test fixture
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
```

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

## License

[Apache 2.0](LICENSE) — permanently. Everything in this repository (core library, CLI, mock server, and the `.jig` suite format) is and will remain Apache 2.0; no relicensing, no license switch later.

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/) — sign off your commits with `git commit -s`. See [CONTRIBUTING.md](CONTRIBUTING.md).

---

Built by [Shodh Labs](https://github.com/Shodh-Labs).
