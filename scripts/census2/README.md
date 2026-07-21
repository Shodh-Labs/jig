# census v2 harness

Three zero-dependency Node scripts that produce the **rubric weight calibration
dataset**: per-dimension `jig check` scores across a 127-server MCP fleet.

Census v1 (`scripts/census/`) asked *what does a server expose and what does it
cost*. Census v2 asks *how does `jig check` score it, dimension by dimension* —
the spread you need before you can fit rubric weights to anything real.

Method note and provenance: [`docs/census/METHOD-census2.md`](../../docs/census/METHOD-census2.md).

## Run it end to end

```sh
cargo build -p jig-cli --release

# 0. fetch the npm discovery pools (not committed — see below)
node scripts/census2/build-candidates.js --fetch-pools data/pools

# 1. assemble the fleet          -> data/census2-servers.json
node scripts/census2/build-candidates.js

# 2. check every server          -> data/census2-raw.json
node scripts/census2/run-census2.js

# 3. aggregate the calibration   -> data/census2-calibration.json
node scripts/census2/aggregate-census2.js
```

Every script takes `--help`. Stage 1 needs network (npm registry), stage 2 needs
network (`npx -y` per server) and the jig binary, stage 3 is pure local.

**Expected wall time:** stage 1 ~30 s (129 registry lookups at concurrency 8),
stage 2 **~20 minutes** for 127 servers at concurrency 4 (the Jul-2026 run took
1,206 s), stage 3 instant. Stage 2 is dominated by `npx` cold starts, not by jig.

## The scripts

### `build-candidates.js` — assemble the fleet

Takes the curated census v1 fifty verbatim (args and `{TMPDIR}` placeholders
intact) and adds every package from the npm discovery pools that survives
screening: has a `bin` entry on its latest version, published within ~14 months,
MCP-related name/keywords/description, and not matching obvious non-server
patterns (`sdk`, `client library`, `inspector`, `template`, `create-*`, …).

There is deliberately **no reachability screening**. A server that refuses to
start is census data, not an exclusion.

```sh
node scripts/census2/build-candidates.js --pool-dir <dir> --asof 2026-07-19 --out <path>
```

`--asof` pins the recency clock, so the original selection can be re-screened
rather than re-dated.

**The discovery pools are not committed.** They are raw npm registry search
responses (~210 KB of JSON that is 95% metadata this pipeline never reads).
`--fetch-pools <dir>` regenerates them from the public search API:

| file | query |
| --- | --- |
| `npm-mcp-search.json` | `https://registry.npmjs.org/-/v1/search?text=keywords:mcp%20server&size=100` |
| `npm-mcp2.json` | `https://registry.npmjs.org/-/v1/search?text=keywords:modelcontextprotocol&size=80` |

npm's search ranking drifts, so a fresh fetch will not reproduce the Jul-2026
pool member for member. `data/census2-servers.json` is the record of what was
actually screened; re-running against the archived pools with
`--asof 2026-07-19T00:00:00Z` reproduces it exactly.

### `run-census2.js` — the fleet runner

Per server: `jig check --stdio "npx -y <pkg> [args]" --json --no-report --timeout 45`.
Keeps the full check document on success and an honest failure record with a
reason on failure. Two attempts per server, concurrency 4, 105 s hard wall per
invocation.

The jig binary is resolved as `--jig` → `$JIG_BIN` → `target/release` →
`target/debug`.

Progress is checkpointed to a JSONL sidecar (`<out>.progress.jsonl`) after every
server, and a re-run skips whatever is already recorded. A 20-minute fleet run
*will* get interrupted; resume is what makes the dataset collectible at all.
Delete the sidecar (or pass `--no-resume`) for a clean run.

`jig check` exits nonzero for a failing grade too, so the reachability signal is
"did we get a check document on stdout", not the exit code.

### `aggregate-census2.js` — the calibration aggregator

Per-dimension score statistics (min/p25/median/p75/p90/max, mean, stddev, plus
the raw sorted samples), composite and grade distribution, cap counts, a
finding-class frequency table, and the failure taxonomy for the unreachable tail.

Non-applicable dimensions are excluded from the samples — a dimension that did
not apply has no opinion about the server.

**Known weakness:** `jig check --json` carries no stable finding class code, so
class keys are *synthesised* by normalizing message text (backticked identifiers
→ `<name>`, digit runs → `<n>`). Rewording a message splits a class; similar
phrasing merges two. Treat `findingClasses` as an indicative frequency table and
never compare those keys across jig versions. The per-dimension score statistics
have no such problem — they read numeric fields.

## Output files

| file | what it is |
| --- | --- |
| `data/census2-servers.json` | the fleet: 127 servers plus the `_selection` screening record |
| `data/census2-raw.json` | one full `jig check --json` document per reachable server; a failure record with a reason for every other |
| `data/census2-calibration.json` | the aggregate: per-dimension spreads, grade distribution, finding-class frequencies, failure taxonomy |
| `data/census2-raw.progress.jsonl` | checkpoint sidecar; safe to delete, not part of the dataset |

## Caveats

Read these before drawing a conclusion from the dataset.

- **One machine, one run.** Windows 11 / Node v22.16.0, collected 2026-07-21.
  Nothing here is averaged over repeats, and no CI refresh runs it.
- **npx cold-start variance.** Servers are fetched unpinned (`npx -y`, latest) on
  purpose, so the dataset tracks the ecosystem as it actually ships — but the
  same package can pass on one run and time out on the next. Three of the
  failures were wall timeouts, which is exactly the shape flake takes here.
- **Selection skew.** Only half the fleet produced a score, and the reachable
  subset skews toward the curated v1 cohort — servers picked in v1 *because* they
  handshake without credentials. The pool additions were screened for
  publishability, not runnability, and disproportionately hit credential walls.
  So per-dimension distributions describe *servers that start*, which is a
  friendlier population than "MCP servers on npm".
- **Rubric-version-bound.** The scores were produced by one rubric version
  (recorded as `rubricVersion` in the calibration file). Re-scoring under a
  revised rubric requires re-running stage 2, not re-aggregating stage 3.
- **Failures are kept, not hidden.** Unreachable servers are excluded from the
  score distributions (you cannot score a surface that never rendered) but are
  fully retained with their reasons. A census that dropped its failures would
  describe a healthier ecosystem than the one that exists.
