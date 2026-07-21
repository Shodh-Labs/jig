# Census v2 — Method and Data Provenance

*How the `jig check` per-dimension dataset was collected, on a 127-server fleet, with [`jig`](https://github.com/Shodh-Labs/jig).*

**Collected:** 2026-07-21 · **Servers attempted:** 127 · **Checked:** 63 · **Failed:** 64 · **Wall time:** 20 minutes

---

This is a method note, not a findings write-up. Census v1 ([The State of MCP Servers, July 2026](2026-07-19-state-of-mcp-servers.md)) asked what fifty servers *expose* and what they *cost*. Census v2 asks a narrower question of a wider fleet: **what does `jig check` score each server, dimension by dimension?** That spread is the prerequisite for fitting rubric weights to something real rather than to intuition.

No scores, grades, or weights appear below. They live in the data, they are moving under a rubric revision, and pinning them into prose would date this document on the day it merged. What is fixed — and what this note records — is *how the fleet was chosen* and *what was measured*.

---

## The fleet: 127 servers

The list is [`data/census2-servers.json`](../../data/census2-servers.json), assembled by [`scripts/census2/build-candidates.js`](../../scripts/census2/build-candidates.js) from two sources.

**The curated fifty, verbatim.** Every server from census v1 ([`data/census-servers.json`](../../data/census-servers.json)) carries over unchanged, arguments and `{TMPDIR}` placeholders intact. Keeping them identical is what lets the two censuses be read against each other.

**Plus 77 screened additions.** Two npm registry search dumps captured 2026-07-19 supplied 129 unique candidate packages not already in v1. Each was screened against the npm registry and kept only if it satisfied all four criteria:

1. its latest version declares a `bin` entry — i.e. `npx -y <package>` actually runs something;
2. that version was published within ~14 months — actively published, not abandoned;
3. its name, keywords, or description mention MCP — drops the generic hits a keyword search drags in;
4. it does not match obvious non-server patterns — `sdk`, `client library`, `framework for building`, `inspector`, `boilerplate`, `template`, `starter`, `scaffold`, `create-*`, `generator`.

Screening outcome:

| | count |
| --- | ---: |
| Pool candidates (unique, non-v1) | 129 |
| **Accepted** | **77** |
| Rejected — no `bin` entry | 22 |
| Rejected — non-server pattern | 20 |
| Rejected — stale (>~14 months) | 10 |

50 + 77 = **127**.

**Nothing was screened for reachability.** A package that demands a credential the instant it boots stays in the fleet and fails in the data. Filtering on runnability before measuring would have manufactured a healthier ecosystem than the one that exists — and, as it turns out, would have thrown away half the fleet.

### Provenance of the discovery pools

The two npm search dumps are **not committed**: ~210 KB of raw registry JSON that is 95% metadata this pipeline never reads. They are regenerable — each is the verbatim body of one public search API call:

| pool file | query | captured |
| --- | --- | ---: |
| `npm-mcp-search.json` | `https://registry.npmjs.org/-/v1/search?text=keywords:mcp%20server&size=100` | 2026-07-19 (100 of 60,982 matches) |
| `npm-mcp2.json` | `https://registry.npmjs.org/-/v1/search?text=keywords:modelcontextprotocol&size=80` | 2026-07-19 (80 of 2,081 matches) |

`node scripts/census2/build-candidates.js --fetch-pools data/pools` fetches both.

Be honest about what that does and does not buy you: npm's search ranking is not stable over time, so a fresh fetch will **not** reproduce the July-2026 pool member for member. `data/census2-servers.json` is the record of what was actually screened. Re-running the builder against archived pools with `--asof 2026-07-19T00:00:00Z` reproduces that file exactly, including the 129/77/22/20/10 screening counts — the `--asof` flag exists precisely so the recency window is re-screened rather than re-dated.

---

## What was measured

One command per server, via [`scripts/census2/run-census2.js`](../../scripts/census2/run-census2.js):

```sh
jig check --stdio "npx -y <package> [args]" --json --no-report --timeout 45
```

The full check document is kept for every server that produces one, and a failure record with a reason for every server that does not.

**Parameters.** jig per-request timeout 45 s; hard wall 105 s per invocation, wide enough for an `npx` cold-start install plus jig's own timeout firing cleanly first; concurrency 4; two attempts per server. `server-filesystem` and friends receive a freshly-created temporary directory in place of `{TMPDIR}`. Packages are fetched **unpinned** (`npx -y`, latest), deliberately, so the dataset tracks the ecosystem as it ships.

**Reachability is judged by output, not exit code.** `jig check` exits nonzero for a failing grade as well as for a broken server, so the harness treats "did a check document arrive on stdout" as the signal. Conflating the two would have counted every low-scoring server as unreachable.

**Progress is checkpointed.** Every completed server is appended to a JSONL sidecar, and a re-run skips what is already recorded. A twenty-minute fleet run against the live npm registry gets interrupted; the resume path is what made this collectible at all rather than a methodological nicety.

**Machine:** Windows 11, Node v22.16.0, jig release binary. One machine, one run, no CI refresh.

---

## What happened

**127 attempted · 63 checked · 64 failed** — a 50% failure rate, up from v1's 42%, which is what you would expect after widening the net from a hand-curated cohort to a keyword-screened one.

The failures fall into clean buckets:

| Failure mode | Count |
| --- | ---: |
| Exited with code 1 before responding to `initialize` (overwhelmingly credential walls) | 51 |
| First stderr line was a stdout-pollution warning — non-protocol lines broke MCP framing | 8 |
| Hit the 105 s hard wall without ever responding | 3 |
| Other (exited code 0 before `initialize`; transport dropped at `tools/list`) | 2 |

The credential wall is the dominant failure mode of the MCP ecosystem, and this is the second census in a row to say so. The stdout-pollution bucket is classified by the *first* `jig:` line on stderr, which is the pollution warning; the fatal error usually followed. Some of those are the same CLI-usage-on-stdout pattern census v1 corrected itself about on 2026-07-20 — servers that require a subcommand and print help text to stdout when invoked bare. Treat 8 as an upper bound on genuine runtime pollution, not a count of it.

Per-server reasons are retained in [`data/census2-raw.json`](../../data/census2-raw.json) and aggregated into a `failureTaxonomy` in [`data/census2-calibration.json`](../../data/census2-calibration.json). Nothing was dropped.

---

## Aggregation

[`scripts/census2/aggregate-census2.js`](../../scripts/census2/aggregate-census2.js) reduces the raw run to [`data/census2-calibration.json`](../../data/census2-calibration.json): per-dimension score statistics with the sorted samples retained, composite and grade distribution, cap counts, a finding-class frequency table, and the failure taxonomy.

Non-applicable dimensions are excluded from the samples. A dimension that did not apply to a server has no opinion about it, and folding it in as a score would be inventing data.

**Finding-class keys: fixed going forward, not retroactively.** `jig check --json` now emits a stable machine-readable `code` on every finding — `<dimension>.<class>`, e.g. `protocol.stdout_pollution` — alongside the existing `{dimension, message, fix, severity, points}`. When the raw file carries it, the aggregator uses that code verbatim as the class key, and the resulting table *is* an identity key: it survives rewording and is comparable across jig versions. The output records which source was used in `_findingClassKeySource` (`code`, `message`, `mixed`, or `none`).

**The committed census2 datasets predate the field.** [`data/census2-raw.json`](../../data/census2-raw.json) was collected before `code` existed, so it carries none, and [`data/census2-calibration.json`](../../data/census2-calibration.json)'s class keys are message-derived. Re-running the aggregator cannot recover the codes — only a fresh stage-2 fleet run can. **The published datasets are therefore no more comparable than they were**; nothing about this change makes their class keys valid to compare against a newer run.

For those message-derived keys, the aggregator falls back to normalizing the human-readable message: backticked identifiers become `<name>`, digit runs become `<n>`, lowercased, prefixed with dimension and severity. That works well enough to rank what the fleet trips over, and not at all as an identity key:

- rewording a message splits one class into two across jig versions;
- two unrelated checks that happen to phrase similarly merge into one;
- a message embedding an un-backticked, non-numeric variable fragments into one class per server.

A message-derived finding-class table is an indicative frequency count and **is not comparable across jig versions, nor with a code-derived table**. The per-dimension score statistics carry no such caveat — they read numeric fields.

---

## Caveats

- **One machine, one run, no repeats.** Nothing here is averaged. Unlike the v1 census there is no scheduled refresh.
- **npx cold-start variance is real.** Unpinned installs against the live registry mean the same package can pass one run and time out the next; the three wall timeouts are exactly the shape flake takes here.
- **Selection skew, and it runs one way.** Only half the fleet produced a score, and the scored half skews toward the curated v1 cohort — servers selected in v1 *because* they handshake without credentials. The 77 pool additions were screened for publishability, not runnability, and disproportionately hit credential walls. Every distribution in the calibration file therefore describes *servers that start*, a friendlier population than "MCP servers on npm".
- **Rubric-version-bound.** The scores were produced by a single rubric version, recorded as `rubricVersion` in the calibration file. Re-scoring under a revised rubric means re-running the fleet, not re-aggregating the existing raw file.
- **Inclusion is not endorsement,** and is explicitly not a claim that a server works. The census records honest pass/fail.

---

## Reproduce it

```sh
cargo build -p jig-cli --release
node scripts/census2/build-candidates.js --fetch-pools data/pools
node scripts/census2/build-candidates.js
node scripts/census2/run-census2.js
node scripts/census2/aggregate-census2.js
```

Harness and per-flag documentation: [`scripts/census2/README.md`](../../scripts/census2/README.md).

Fleet list: [`data/census2-servers.json`](../../data/census2-servers.json) · Raw per-server check documents: [`data/census2-raw.json`](../../data/census2-raw.json) · Calibration aggregate: [`data/census2-calibration.json`](../../data/census2-calibration.json).

*Wall-clock time for the full 127-server run: 20 minutes at concurrency 4.*

---

## Re-running this changes robustness scores (2026-07-21)

The fleet was run twice on the same machine, from the same list: once under
`rubric-v1.4`, and once again after `rubric-v1.5` shipped. Reachability was
identical both times — 63 checked, 64 failed, the same servers in each bucket —
and four of the five dimensions returned byte-identical scores.

**Robustness did not.** It moved on 62 of 63 servers, and it moved down:

| | robustness delta, run 2 − run 1 |
|:--|--:|
| mean | −7.78 |
| median | −8.24 |
| worst | −20.21 |
| servers moving more than 5 points | 40 of 63 |

Robustness is the only dimension with a **live-measured input**: it times how
long a server takes to answer `initialize`, less the measured `npx` launcher
floor. Everything else is computed from the tool surface, which is a fixed
document. So robustness is the only dimension that can register machine
conditions — load, disk cache, npm cache warmth — as if they were properties of
the server.

The second run followed a release build on a busy laptop. That is almost
certainly the whole explanation, and it is exactly the kind of confounder the
`rubric-v1.4` launcher-floor subtraction was built to remove; it removed the
*shim's* constant cost, not the machine's variable one.

Two consequences, stated plainly:

1. **A single run is not a stable grade for a server whose boot is slow.** The
   `rubric-v1.5` changelog's grade-impact table deliberately re-scores the *same*
   cards under both rubrics rather than comparing two runs, because comparing
   runs conflates the rubric change with this drift. A fresh `rubric-v1.5` run of
   the same fleet lands at 34/15/10/3/1 rather than the table's 41/9/9/3/1 —
   the difference is measurement, not scoring.
2. **The ecosystem leaderboard should not publish per-vendor grades off one
   run.** It needs a stated measurement method — repeated runs, a quiet machine,
   and a published tolerance — or it will report our laptop's mood as a vendor's
   quality.

Neither the census v2 dataset nor `data/dimension-spread.json` is invalidated by
this: both were collected in run 1, and the spread file's purpose is reporting
context, not scoring. But the robustness spread in it should be read as *one
machine on one afternoon*, and the same caveat applies to any robustness figure
quoted from this dataset.

---

## A code-keyed re-run (2026-07-22)

`jig check --json` now emits a stable `code` on every finding, so defect classes
no longer have to be inferred from message text. The fleet was re-run with a
binary carrying that field; the result is
[`data/census2-coded-raw.json`](../../data/census2-coded-raw.json) and
[`data/census2-coded-calibration.json`](../../data/census2-coded-calibration.json),
whose aggregate records `"_findingClassKeySource": "code"`.

**This is the dataset to use for anything that counts defect classes.** The
earlier files remain as collected and stay message-derived; re-aggregating them
cannot recover codes, because the codes were never in the raw documents.

The difference is not cosmetic. Message-normalisation split one defect across
four keys, because the message enumerates the offending parameters:

```
15 servers  "`<name>`: parameter `<name>` missing a description"
13 servers  "`<name>`: parameters `<name>`, `<name>` missing a description"
11 servers  "`<name>`: parameters `<name>`, `<name>`, `<name>`, `<name>` missing …"
 9 servers  "`<name>`: parameters `<name>`, `<name>`, `<name>` missing a description"
```

Under codes that is one class — `schema_hygiene.param_missing_description`, 288
occurrences across 22 servers — and the fleet's class count drops from 32
message-derived keys to 15 real classes. The old table did not merely look
untidy; it understated how widespread the most common defects are, because each
defect's mass was divided among its phrasings.

One honest note on comparability between runs: this run checked **62** servers
where the earlier runs checked 63. `european-parliament-mcp-server` is
intermittently reachable — it also appeared in only one of the three leaderboard
runs — so fleet composition varies slightly at the margin even when the list does
not.
