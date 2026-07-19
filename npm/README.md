# @shodh/jig

A testing workbench for **MCP (Model Context Protocol) servers**. Inspect a
server's tool surface, call tools, price the context-token budget of a tool set,
and bench a real model against it — all from the command line.

This npm package is a thin installer/launcher: on install it downloads the right
**prebuilt `jig` binary** for your platform from
[GitHub Releases](https://github.com/Shodh-Labs/jig/releases), verifies its
SHA-256 checksum, and puts it on your PATH. The heavy lifting is done by the
native Rust binary — see the [main repository](https://github.com/Shodh-Labs/jig)
for full documentation.

## Use

No install needed — run it straight with `npx` the same way you run MCP servers:

```sh
npx @shodh/jig inspect --stdio "npx -y @modelcontextprotocol/server-everything"
```

Or install it globally:

```sh
npm install -g @shodh/jig
jig inspect --stdio "npx -y some-mcp-server"
```

Common commands:

```sh
jig inspect --stdio "<server cmd>"                 # handshake + list tools/resources/prompts
jig call    --stdio "<server cmd>" --tool <name> --args '{"...":"..."}'
jig budget  --stdio "<server cmd>" --model gpt-4o --model claude-sonnet
jig bench   --stdio "<server cmd>" --task "book a table for two"
```

`jig` also speaks remote MCP over Streamable HTTP via `--http <url>` (with
repeatable `--header "Authorization: Bearer …"`). Run `jig --help` for the full
surface.

**Exit codes** are meaningful and forwarded faithfully by the launcher:
`0` success · `1` a jig-level failure (transport/protocol/usage) · `2` a tool
call that the server reported as `isError: true`.

## Supported platforms

Prebuilt binaries are published for:

| OS            | Arch          | Node `platform/arch` | Release target              |
| ------------- | ------------- | -------------------- | --------------------------- |
| Windows       | x64           | `win32/x64`          | `x86_64-pc-windows-msvc`    |
| macOS (Intel) | x64           | `darwin/x64`         | `x86_64-apple-darwin`       |
| macOS (Apple) | arm64         | `darwin/arm64`       | `aarch64-apple-darwin`      |
| Linux         | x64           | `linux/x64`          | `x86_64-unknown-linux-musl` |

The end-to-end install flow — download, checksum-verify, extract, and run the
shim (`jig --version` + `jig inspect`) against a real release archive served from
localhost — is exercised in CI on Ubuntu, macOS, and Windows on every change, so
"works on macOS/Linux/Windows" is a tested claim, not an aspiration.

On any other platform/arch the installer fails with a clear message pointing you
at the `cargo` build path:

```sh
cargo install --git https://github.com/Shodh-Labs/jig jig-cli
```

## Security: checksum verification

Every download is verified before it is trusted. The installer fetches the
release's `SHA256SUMS` alongside the archive and computes the SHA-256 of the
downloaded archive **in memory**; if it does not match the published digest, the
install **aborts and nothing is extracted or written**. On Windows the archive
is parsed entirely in-process (no shelling out), so the verified bytes never
touch disk before being trusted.

Publishing to npm uses [npm provenance](https://docs.npmjs.com/generating-provenance-statements),
so you can trace the published package back to the exact GitHub Actions run and
commit that produced it.

## Environment overrides

| Variable            | Effect                                                                                             |
| ------------------- | -------------------------------------------------------------------------------------------------- |
| `JIG_BINARY_PATH`   | Use this local binary verbatim and **skip all network access**. For CI, air-gapped, and testing.   |
| `JIG_DOWNLOAD_BASE` | Download from this base URL instead of GitHub Releases. Layout: `{base}/v{version}/{asset}`.        |

Examples:

```sh
# Use a binary you built or vendored yourself; no download happens.
JIG_BINARY_PATH=/path/to/jig npm install -g @shodh/jig

# Pull binaries from an internal mirror that mirrors the GitHub Releases layout.
JIG_DOWNLOAD_BASE=https://mirror.internal.example/jig npm install -g @shodh/jig
```

## Behind a proxy

Node's built-in HTTP client does **not** honor `HTTPS_PROXY`/`HTTP_PROXY`, and
this installer keeps a zero-dependency footprint, so it connects **directly**.
If a proxy is configured the installer warns and continues; if the direct
connection then fails, download the release archive manually and point
`JIG_BINARY_PATH` at the extracted binary, or install via `cargo` (above).

## How it works / offline notes

- **Windows** archives are `.zip`; the single binary entry is extracted with a
  small in-process zip reader (Node has `zlib` but no zip reader) — no external
  tools, no temp files.
- **macOS/Linux** archives are `.tar.gz`; the binary is streamed out with the
  system `tar` (always present on the supported unix targets). There is no
  pure-JS tar fallback by design.

## License

Apache-2.0. See the [main repository](https://github.com/Shodh-Labs/jig) for
source, issues, and full documentation.
