# Jig ecosystem percentiles — `data/percentiles.json`

`jig check` grades a server's **context cost** (dimension 2) against the wider
MCP ecosystem when a percentile dataset is available, and falls back to fixed
absolute bands when it is not. This file is the contract for that optional
dataset so the scorer and the data-collection job agree byte-for-byte.

## Location & optionality

* Path: `data/percentiles.json` at the repo root (override with
  `jig check --percentiles <file>`).
* **Optional.** If the file is absent or unparseable, `jig check` scores
  context cost with documented absolute bands and the report says
  `(no ecosystem data — absolute bands)`. Present → the report says e.g.
  `94th percentile (heaviest 6%)`.

## Schema (v1)

Dead simple: one object per metric, each carrying a **sorted-ascending** array
of raw samples plus provenance. The file documents itself in `_schema`.

```json
{
  "_schema": "jig percentiles v1: each metric holds `samples`, an ascending array of raw measurements, plus `collected` (ISO date) and `n` (sample count). percentile(x) = 100 * (count of samples <= x) / n. context_cost_tokens is the gpt-4o (o200k_base, exact) grand total — every tool's canonical {name,description,input_schema} rendering plus the server `instructions` string — for one MCP server's full tool surface.",
  "context_cost_tokens": {
    "samples": [412, 980, 1432, 3400, 5120, 8800, 21000],
    "collected": "2026-07-19",
    "n": 7
  }
}
```

### Fields

| Field | Type | Meaning |
| --- | --- | --- |
| `_schema` | string | Human-readable description of the format + the `context_cost_tokens` metric definition. Required. |
| `context_cost_tokens.samples` | number[] | Ascending array of per-server gpt-4o exact total tokens. The scorer sorts defensively, so out-of-order data still works, but keep it sorted. |
| `context_cost_tokens.collected` | string | ISO-8601 date the dataset was gathered. Shown in the report footer. Optional. |
| `context_cost_tokens.n` | number | Sample count. Advisory — the scorer uses `samples.len()`. Optional. |

### Percentile definition

For a server whose gpt-4o total is `x` tokens:

```
percentile(x) = 100 * (number of samples <= x) / samples.len()
```

A server at the **94th percentile** is heavier than ~94% of the ecosystem —
i.e. in the heaviest ~6%. Context-cost score is `round(100 - percentile)`, so
a lean server (10th percentile) scores 90 and a bloated one (94th) scores 6.

## Optional top-level `startup_failure_rate`

An optional top-level number: the fraction (`0..1`, or an already-scaled
percentage `>1`) of surveyed public servers that **failed at startup / during
the handshake** — i.e. never became reachable in the census. When present,
`jig check` appends one line of ecosystem cohort context to a *startup-failure*
error (a stdio server that dies before/during the handshake):

```
For context: in the 2026-07 census, 42% of surveyed public MCP servers also failed at startup.
```

The month is taken from the top-level `collected` date. The field is **optional**
— when absent, `jig check` silently omits the cohort line. The census script
emits it as `failed / attempted` from the raw run.

## Sibling dataset: `data/dimension-spread.json`

A **different file, on purpose.** `data/dimension-spread.json` (added in
`rubric-v1.5`) holds each rubric dimension's *score* spread across the measured
fleet, and `jig check` prints it beside that dimension's score:

```
  ✓  Protocol compliance  100  [100·100·100]  clean handshake, no stdout pollution…
  ✓  Context cost          99  [43·87·97]     183 tokens…
```

so a reader can see which dimensions separate servers and which do not. It is
documented here because this is where Jig's bundled data-file contracts live —
but it is **not a percentiles metric and must not be merged into this file**:

| | `percentiles.json` | `dimension-spread.json` |
|:--|:--|:--|
| Purpose | **scoring** input | **reporting** context |
| Holds | per-server context-cost *token counts* | per-dimension *score* quartiles |
| Cohort | the curated `v1` census | the census-v2 fleet (unvetted additions) |
| Reaches the composite? | yes — context cost is scored from it | **never** |
| Overridable | `--percentiles <file>` | no |

Folding the fleet into `percentiles.json` would move published grades under cover
of a reporting change, which is why the cohorts are kept apart.

### Shape

```json
{
  "_schema": "…",
  "collected": "2026-07-21T11:41:15.332Z",
  "n": 63,
  "_caveat": "One machine, one run…",
  "dimensions": {
    "protocol": { "p25": 100.0, "median": 100.0, "p75": 100.0, "n": 63 },
    "context_cost": { "p25": 43.1, "median": 87.1, "p75": 96.6, "n": 63 }
  }
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `collected` | string | ISO-8601 timestamp of the fleet run the spreads derive from. |
| `n` | number | Servers graded in the run (63). Per-dimension `n` may be lower. |
| `_caveat` | string | One sentence on sample skew. Required — the numbers are a sample, not a population, and the file says so. |
| `dimensions.<key>` | object | Keyed by [`Dimension::key`] (`protocol`, `context_cost`, `schema_hygiene`, `description_quality`, `robustness`). |
| `dimensions.<key>.p25` / `.median` / `.p75` | number | Fleet quartiles for that dimension's score, rounded to 1 decimal. |
| `dimensions.<key>.n` | number | How many servers the dimension was *applicable* to. Lower than the top-level `n` for dimensions a server can be excluded from — schema hygiene and description quality are not scored on a server exposing no tools, so both read 62. |

Derived from `data/census2-calibration.json`; small and hand-auditable by design.
It is bundled into the binary with `include_str!`, so there is no on-disk lookup
and no flag to point it elsewhere. A missing or malformed entry degrades to
printing nothing, never to printing a wrong number.

## Notes for the data-collection job

* Emit **exactly** this shape. Extra top-level metrics are allowed and ignored
  by v1 (forward-compatible), but `context_cost_tokens.samples` must be present
  and numeric to enable percentile scoring.
* `startup_failure_rate` (optional) is `failed / attempted` for the run.
* Use the **gpt-4o exact** total (`jig budget --stdio "<cmd>" --model gpt-4o`,
  the `TOTAL` row) as each sample — the same metric `jig check` computes, so the
  comparison is apples-to-apples.
* One sample per distinct server. ~50 real public servers is the target `n`.
* Do not commit secrets or server-identifying data — samples are bare integers.
