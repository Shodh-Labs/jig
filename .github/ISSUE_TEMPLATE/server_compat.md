---
name: Server compatibility
about: A real MCP server breaks jig (or jig misreads it)
title: "compat: <server name>"
labels: compatibility
assignees: ""
---

**Which server?**

Package / repo and the command that launches it:

```
jig inspect --stdio "npx -y <package>"
```

**What breaks?**

<!-- Handshake fails? A tool schema is misreported? Budget looks wrong?
     Non-protocol output on stdout? -->

**Tap**

Attach `--tap trace.jsonl` from the failing run — the raw traffic is what we
need to reproduce.

```jsonl
<paste trace.jsonl here>
```

**Is it in the nightly battery?**

Our nightly compatibility battery runs jig against the popular servers. If this
one should be watched there, say so — we may add it.
