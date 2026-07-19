# Contributing to Jig

Thanks for your interest! Jig is early — the most valuable contribution right now is
telling us how you test your MCP server today: [open an issue](https://github.com/Shodh-Labs/jig/issues).

## License and sign-off

Jig is [Apache 2.0](LICENSE), permanently. We use the
[Developer Certificate of Origin](https://developercertificate.org/) (DCO) instead of a CLA:
sign off each commit to certify you have the right to contribute it under Apache 2.0.

```
git commit -s -m "your message"
```

That's the whole process — no paperwork, no copyright assignment.

## Development

```
cargo build --workspace
cargo test  --workspace
cargo fmt --all
cargo clippy --all-targets
```

All four must be clean before a PR. The integration tests exercise the real `jig` binary
against `jig-mock-server` — see `crates/jig-mock-server/tests/`.

## Ground rules for code

- The library (`jig-core`) never panics on server-controlled input. A misbehaving MCP
  server is a *finding to report*, not an excuse to crash — jig is a diagnostic tool and
  must degrade informatively.
- The protocol tap tells the truth: raw wire traffic, recorded verbatim, even (especially)
  when it's malformed.
- Numbers shown to users are exact or clearly labeled as approximations. Never silently wrong.
