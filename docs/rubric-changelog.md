# Rubric changelog

Jig's report card is versioned. Every score Jig emits — human report, `--json`,
`--badge`, the HTML report card — carries the `rubricVersion` that produced it.

> **Scores from different rubric versions are not comparable.** A `rubric-v1`
> 73 and a `rubric-v1.1` 73 were produced by different arithmetic and mean
> different things. When comparing servers, or comparing one server over time,
> check that the rubric versions match before reading anything into the delta.
> Re-run the older subject under the current rubric instead of adjusting the old
> number by hand.

---

## `rubric-v1.1`

Motivated by a 31-server fleet run under `rubric-v1`, which produced a grade
distribution of A 13 · B 9 · C 6 · D 0 · F 3 and exposed two defects. Both were
scoring bugs, not measurement bugs: the underlying observations were correct in
every case, and no *finding* changed. Only the arithmetic that turns findings
into a number changed.

### 1. Per-dimension penalties are now size-relative

**The defect.** Schema hygiene and description quality grade *per-item* defects
(a parameter without a description, a tool without a title) but summed their
per-item penalties without regard to how many items the server exposed. The
deduction was therefore a function of **tool-surface size**, not quality. A
90-tool server was mathematically guaranteed to hit 0 — a handful of undescribed
parameters per tool saturates a 100-point budget almost immediately — while a
5-tool server with the *same proportion* of defects scored in the 90s.

At weight 20, that single manufactured zero drove every F in the fleet run. One
server measured protocol 100, description 90, robustness 100, schema **0**,
context 11 and composited to F 56. Calling that server an F is not a defensible
reading of the evidence; it is an artifact of the denominator.

**The fix.** Both dimensions now score the **rate** of defects. For each defect
class *c* with per-item weight `p_c`:

```text
rate_c    = defective items in class c / total items in class c
deduction = SCALE * Σ_c ( p_c * rate_c )
score     = clamp(100 - deduction, 15, 100)
```

The denominator is class-appropriate: tool-level classes divide by the tool
count, parameter-level classes by the total parameter count across all tools.
The existing per-item penalty constants are unchanged in value — they now set
each defect class's *relative* weight rather than an absolute deduction. `SCALE`
is chosen per dimension so a 100%-defective server lands exactly on the floor.

**The floor is 15, not 0.** A server that completed a handshake and enumerated a
tool list has demonstrably produced *some* structure. Grading it identically to
one with no structure at all is what manufactured the F grades in the first
place. 0 is now reserved for genuinely absent structure — a dimension that is
not applicable is excluded from the composite entirely, which is a different and
more honest statement than "scored zero".

**This cuts both ways.** Small servers with a high *proportion* of defects now
score lower than they did under `rubric-v1`, where absolute penalties
under-punished them. That symmetry is the point: the rubric now measures the
same thing at every surface size.

### 2. Catastrophic context cost caps the composite

**The defect.** Under `rubric-v1` the heaviest server measured — 89 tools,
42,288 tokens, the 100th percentile of the census and roughly 25× the median —
graded **C 73**, *above* the F 56 of a server costing less than half as much.
Strong schema and description scores simply outweighed a context sub-score of 5.

A rubric that claims context discipline matters cannot let the most expensive
server in the ecosystem outrank a lighter one on the strength of schema polish.

**The fix.** Context cost is a *cost*, not a quality, and a catastrophic one now
**bounds** the composite regardless of the other four dimensions:

| Context sub-score | Composite capped at | Grade ceiling |
|:------------------|--------------------:|:--------------|
| `< 20` (beyond ~p95) | 65 | D |
| `< 10`               | 55 | F |

The cap is never applied silently. It emits an explicit finding and a visible
line in every rendering — human report, `--json` (`contextCap`), and the HTML
report card — naming the token count and how far above the census median it
sits, plus what the server would have scored uncapped:

```
composite capped at 55 by context cost: 42,288 tokens is 25× the census median
```

A cap that would not actually lower the score is not reported at all, so a
`contextCap` in the output always means the number really moved.

### 3. The grade-band gap is closed

`rubric-v1` documented bands `A >= 90 · B 80–89 · C 70–79 · D 60–69 · F < 40`,
leaving scores of 40–59 in a gap: they rendered as F, but no band claimed them.

`rubric-v1.1` defines **F as everything below the D band** — `F < 60`. No new
letter was introduced; the D band is unchanged.

The shields.io badge colors were independently banded under `rubric-v1` (green
ran to 75, orange covered 40–59), which let a C and a B share a color while two
F scores differed. Badge colors are now the grade bands, one color per letter:

| Grade | Score | Badge color |
|:------|:------|:------------|
| A | `>= 90` | `brightgreen` |
| B | `80–89` | `green` |
| C | `70–79` | `yellowgreen` |
| D | `60–69` | `yellow` |
| F | `< 60`  | `red` |

### What did *not* change

- **Findings.** Every defect still produces exactly one finding carrying its fix
  text. The set of findings for a given server is byte-identical to
  `rubric-v1`'s. Only each finding's `points` — its share of the dimension
  deduction, used to rank "Top fixes" — reflects the new arithmetic.
- **Dimension weights.** Still 25 / 25 / 20 / 15 / 15.
- **Measurement.** The context metric is still gpt-4o exact tokens over the
  canonical tool rendering; the percentile census is unchanged; protocol and
  robustness scoring are untouched.
- **Not-applicable handling.** A dimension that does not apply is still excluded
  from the composite and its weight dropped, never assumed to be 100.

---

## `rubric-v1`

The initial rubric: five weighted dimensions (protocol compliance 25, context
cost 25, schema hygiene 20, description quality 15, robustness 15), each scored
`0..=100` by subtracting documented per-defect penalties from 100, composited by
weight over the applicable dimensions.
