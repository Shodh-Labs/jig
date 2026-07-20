# Rubric changelog

Jig's report card is versioned. Every score Jig emits — human report, `--json`,
`--badge`, the HTML report card — carries the `rubricVersion` that produced it.

> **Scores from different rubric versions are not comparable.** A `rubric-v1`
> 73, a `rubric-v1.1` 73 and a `rubric-v1.2` 73 were produced by different
> arithmetic and mean different things. This is a standing property of the
> rubric, not a caveat attached to any one release. When comparing servers, or comparing one server over time,
> check that the rubric versions match before reading anything into the delta.
> Re-run the older subject under the current rubric instead of adjusting the old
> number by hand.

---

## `rubric-v1.2`

`rubric-v1.1` shipped with five defects, flagged by its own author in the
handover. All five are closed here. Every one is an *arithmetic* defect — the
observations, the findings and their fix text are unchanged, as is the set of
things Jig measures. Four of the five make the rubric **less** punitive; that
direction is deliberate, because this rubric grades named companies' servers in
public, and a defensible grade is worth more than a harsh one.

### 1. The cap was itself a cliff

**The defect.** `rubric-v1.1` existed largely to remove a scoring cliff, then
rebuilt one at the cap. Its ceiling was a two-step function — context sub-score
`< 20` capped the composite at 65, `< 10` at 55 — so a sub-score of 20.1 kept a
composite of 76 while 19.9 was forced to 65. An **11-point discontinuity across
a hair of measurement difference**, in the release whose stated purpose was to
delete exactly that shape. A step function is also not monotone once combined
with the other dimensions: a server could gain grade by getting worse.

**The fix.** A continuous ramp:

```text
ceiling(sub) = clamp(55 + (sub - 5) / (22 - 5) * 45, 55, 100)
```

There is no discontinuity anywhere on it. It is monotone non-decreasing in the
context sub-score, so worsening context cost can never raise the ceiling — now
asserted by a property test over a dense sweep and its full cross-product, and
separately over the *reported composite* (`min(uncapped, ceiling)`) across a
range of sibling-dimension quality.

At the old boundary, 19.9 and 20.1 now differ by 0.3 points of ceiling instead
of 11 points of grade.

The reported line states the applied cap **and the sub-score that produced it**.
With a continuous ramp the ceiling is no longer one of two memorable constants,
so stating the input is what makes the output checkable:

```
composite capped at 55 by context cost (context sub-score 5): 42,288 tokens is 25x the census median
```

### 2. Cap thresholds were asserted, not calibrated

**The defect.** `rubric-v1.1` documented its sub-score-20 threshold as "roughly
the census p95 — a server heavier than 95% of the ecosystem". Its own percentile
mapping says otherwise. Percentile scoring assigns `score = 90 - (pct - 50) *
1.7` above the median, which inverts exactly:

| Sub-score | Actual percentile | `v1.1` claimed |
|:----------|:------------------|:---------------|
| 20 | **p91.2** | "~p95" |
| 10 | **p97.1** | "extreme tail" |

The severe cap was firing on roughly the heaviest **9%** of the ecosystem, not
the heaviest 5%. The documented intent and the arithmetic disagreed.

**The fix.** The ramp's anchors are derived from that mapping rather than chosen
as round numbers, and the percentiles they implement are stated in the module
docs and here:

| Sub-score | Census percentile | `v1.2` ceiling | `v1.1` ceiling |
|:----------|:------------------|---------------:|---------------:|
| 22 and above | **p90** | 100 — inert | none |
| 16.7 | p93 | 86.0 | 65 |
| 13.5 | **p95** | 77.5 | 65 |
| 9.9 | p97 | 68.0 | 55 |
| 5 and below | **p100** | 55 | 55 |

The upper anchor is exactly p90 — below it the cap does nothing at all. The
lower anchor is exactly p100, which is also the lowest sub-score percentile
scoring can express: the single heaviest server in the measured ecosystem.
"Only genuinely extreme context cost bounds a grade" now describes the
arithmetic instead of contradicting it.

Against the real census (n=29) the same three servers are capped as under
`v1.1`, but far less harshly where the evidence is weaker: the p93 server's
ceiling moves from 65 to 86.0, the p97 server's from 65 to 70.5, and only the
p100 server is still held at 55.

### 3. Class weights did not transfer from per-item to per-rate

**The defect.** The class weights were inherited unchanged from the per-item
regime, where they meant "how bad is one instance of this". Under rate scoring
they mean something different — "how much of the dimension does this class
command when violated at rate `r`" — and they do not transfer.

Missing annotations carried a deliberately *minor* per-item weight of 1. But
servers that omit annotations omit them on **every** tool, so the class sat at a
defect rate of ~1.0 and consumed its **entire** share on nearly every server,
while a genuinely serious defect at a 10% rate consumed almost nothing. The
minor class was outweighing the major one.

**The fix.** Re-tuned on the discriminating principle: **a class that is
near-universally violated carries less rate weight** — it separates nobody from
anybody — while **a rare-but-serious class carries more**.

| Class | Old | New | Why |
|:------|----:|----:|:----|
| Schema · missing tool description | 8 | **10** | Uncommon and severe — a model cannot select an undescribed tool. Discriminates well. |
| Schema · parameter missing type | 5 | **8** | Rare, and directly breaks argument generation and validation. The archetypal rare-but-serious class. |
| Schema · parameter missing description | 3 | 3 | Unchanged — the reference point the others were tuned against. |
| Schema · missing annotations | 1 | **0.5** | Near-universally violated, all-or-nothing per server. Carries almost no information about quality. |
| Description · whitespace in name | 15 | 15 | Unchanged — vanishingly rare, categorically fatal. Should dominate when it fires. |
| Description · terse/missing description | 6 | **8** | Determines whether a model can pick the right tool, and far from universal. |
| Description · naming inconsistency | 5 | **4** | Cosmetic, and by construction can only fire on a minority of a server's tools. |
| Description · verbose description | 4 | **3** | Already priced directly, and far more precisely, by the context-cost dimension. Was double-charged. |
| Description · missing title | 1 | **0.5** | Same profile as missing annotations: optional, recently standardized, omitted on every tool or none. |

The sum-to-floor math is intact. `SCALE` is still `(100 - floor) / Σ p_c` over
the worst simultaneously-attainable class set, now over sums of 21.5 (schema,
was 17) and 23.5 (description, was 22).

**A limitation, stated plainly.** These are judgement weights informed by the
census's *shape*, not fitted to measured defect rates. `data/census-raw.json`
records `toolCount`, `contextCostTokens`, `capabilities`,
`stdoutPollutionLines` and similar, but **no per-class schema or description
defect counts at all** — the census never captured the fields these two
dimensions grade, so no such fit is currently possible. Extending the census to
record per-class defect counts is the prerequisite for calibrating these weights
the way the cap anchors are now calibrated.

### 4. Small surfaces scored jumpily

**The defect.** A raw defect rate is a point estimate whose variance explodes as
the denominator shrinks. A 1-tool server with one flaw sits at a 100% defect
rate and consumes a whole class weight; a 40-tool server needs 40 flaws for the
same score. That is a sample-size artefact, not a quality difference — and it is
not a rare corner: **5 of the 29 census servers expose exactly one tool, and 11
of 29 expose five or fewer**.

**The fix.** Empirical-Bayes confidence shrinkage on every class rate:

```text
adjusted_rate = (defects + k * prior) / (n + k)      k = 2, prior = 0
```

`k = 2` is chosen against the census `tool_count` distribution so the prior is
decisive only where the evidence genuinely is thin, and negligible where it is
not:

| Tools `n` | Census position | Prior weight `k/(n+k)` |
|----------:|:----------------|-----------------------:|
| 1 | p17 | 67% |
| 5 | p38 | 29% |
| 14 | median | 13% |
| 26 | p76 | 7% |
| 89 | p100 | 2% |

Effect on schema hygiene:

| Server | `v1.1` | `v1.2` |
|:-------|-------:|-------:|
| 1 tool, 1 defect (100% rate) | 15.0 | **71.7** |
| 40 tools, 40 defects (100% rate) | 15.0 | **19.0** |
| 90 tools, 1/3 rate | 71.7 | 72.3 |
| 900 tools, 1/3 rate | 71.7 | 71.7 |

Large-surface grading is materially unchanged; the small-`n` end is no longer a
coin flip reported as a verdict. The leniency a surface of size `n` enjoys is
exactly `SPAN * raw_rate * k / (n + k)` — an identity the tests assert directly,
so the property survives any future re-tune of `k`.

**The prior is 0.0, and that is a limitation rather than a choice.** The
principled prior is the census median defect rate per class — the same missing
data as defect 3 above. Shrinking toward 0 means a thin surface is treated as
*probably clean*, so small servers are graded generously. That is the right way
to be wrong when the evidence is thin and the grade is public, but it is a thumb
on the scale and should be replaced with a measured prior once one exists.

**One deliberate consequence.** A 100%-defective server no longer lands
*exactly* on the floor of 15, approaching it from above as the surface grows
(40 tools → 19.0, 900 → 15.2). Confidence that a 100% defect rate is real is
itself a function of how many items were observed. The floor is a clamp bound,
not an asserted equality.

### 5. The cap finding was invisible where it mattered most

**The defect.** The cap finding carries `points: 0.0` so the ceiling is not
double-counted against the context sub-score that already priced those tokens.
But `top_fixes` filtered on `points > 0.0` — so the cap finding was silently
excluded from "Top fixes". For precisely the servers whose grade was *most*
determined by context cost, the single fact determining that grade never
appeared in the ranked to-do list users read first.

**The fix.** Ranking weight and score deduction are separate concerns, and are
now separate fields. `Finding::rank_points` carries the ranking weight
independently of `points`; `top_fixes` ranks on it; and the cap finding is
**pinned**, like the stdout-pollution finding, so it can never be crowded out.

The cap finding still contributes exactly 0 to the composite. Its ranking weight
is the composite points the cap actually cost (`uncapped - cap`), converted into
dimension-local units so `points * weight` stays comparable with every other
finding in the list.

### What did *not* change

- **Findings.** Every defect still produces exactly one finding carrying its fix
  text. No finding was added, removed, or reworded by this release except the
  context-cap line, which gained its sub-score.
- **Dimension weights.** Still 25 / 25 / 20 / 15 / 15.
- **Measurement.** The context metric is still gpt-4o exact tokens over the
  canonical tool rendering; the percentile census is unchanged; protocol and
  robustness scoring are untouched.
- **Grade bands and badge colors.** Unchanged from `rubric-v1.1`.
- **The floor.** Still 15, still reserving 0 for genuinely absent structure —
  though it is now approached rather than landed on exactly (defect 4 above).

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
