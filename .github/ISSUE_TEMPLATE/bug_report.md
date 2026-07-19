---
name: Bug report
about: jig did something wrong
title: ""
labels: bug
assignees: ""
---

**What happened, and what did you expect?**

**The exact command**

```
jig ... --stdio "..."
```

**A protocol tap**

Re-run with `--tap trace.jsonl` and attach it (or the relevant lines). This is
the raw JSON-RPC traffic and is usually enough to see the problem.

```jsonl
<paste trace.jsonl here>
```

**Server command** (what `--stdio` launches):

**Environment**

- OS + version:
- jig version (`jig --version`):
- Rust (`rustc --version`), if built from source:
