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

## Notes for the data-collection job

* Emit **exactly** this shape. Extra top-level metrics are allowed and ignored
  by v1 (forward-compatible), but `context_cost_tokens.samples` must be present
  and numeric to enable percentile scoring.
* Use the **gpt-4o exact** total (`jig budget --stdio "<cmd>" --model gpt-4o`,
  the `TOTAL` row) as each sample — the same metric `jig check` computes, so the
  comparison is apples-to-apples.
* One sample per distinct server. ~50 real public servers is the target `n`.
* Do not commit secrets or server-identifying data — samples are bare integers.
