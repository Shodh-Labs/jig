# Rubric changelog

Jig's report card is versioned. Every score Jig emits — human report, `--json`,
`--badge`, the HTML report card — carries the `rubricVersion` that produced it.

> **Scores from different rubric versions are not comparable.** A `rubric-v1`
> 73, a `rubric-v1.1` 73, a `rubric-v1.2` 73 and a `rubric-v1.3` 73 were
> produced by different arithmetic and mean different things. This is a standing property of the
> rubric, not a caveat attached to any one release. When comparing servers, or comparing one server over time,
> check that the rubric versions match before reading anything into the delta.
> Re-run the older subject under the current rubric instead of adjusting the old
> number by hand.

---

## `rubric-v1.3`

Where `rubric-v1.1` and `rubric-v1.2` were arithmetic releases — same
observations, better maths — this one is mostly the opposite. Three of its four
changes make Jig **measure things it previously could not**, closing SOPs 12, 25
and 26, each of which carried an honest *"not machine-checkable"* line in the
SOP guide. The fourth is an arithmetic defect found by the director: a server
that breaks its own protocol framing could still read "A".

Two of the new measurements are **reported and never scored**. That is
deliberate, and it is the same posture the tool-set advisor has held since it
shipped: a detector earns its way into the composite by first being watched in
the wild, not by being switched on the day it is written.

### 1. A server that breaks its own framing could still score "A"

**The defect.** A fixture carrying stdout pollution, an off-spec capability and
missing tool descriptions scored **A 91**. Weighted averaging is why: protocol
compliance is a quarter of the composite, so a single 15-point framing break
moves the total by under four points, and four clean dimensions absorbed it.

But a server that pollutes stdout does not have a small problem in one of five
areas. It has **broken its own framing**, and the four clean dimensions are
describing a server no client can talk to. An A on that is not a slightly
generous score; it is a false statement, and it is the kind of false statement
that destroys a grading instrument's credibility the first time a user tries the
server.

**The fix.** Protocol compliance gets the treatment context cost has had since
`rubric-v1.1`: a heavy enough defect **bounds** the composite rather than merely
nudging it.

```text
ceiling(high_points) = clamp(100 - high_points, 55, 100)
```

`high_points` is the total deduction carried by **HIGH-severity** protocol
findings. The ramp is continuous, monotone non-increasing, and inert at
`high_points == 0` — where the overwhelming majority of servers sit.

| High protocol defect | Deduction | Ceiling | Grade |
|:---------------------|----------:|--------:|:------|
| one malformed tool name | 8 | 92 | A− |
| one polluting stdout line | 15 | 85 | B |
| unknown method accepted | 20 | 80 | B− |
| two polluting stdout lines | 30 | 70 | C |
| a `*/list` that never answered | 40 | 60 | D |
| 45 and above | — | 55 | F |

**Why the ramp reads the deduction, not a count or the sub-score.** Three inputs
were available and the choice matters.

A **count of HIGH findings** — one → 85, two → 75 — is a step function, which is
precisely the discontinuity `rubric-v1.2` spent a release removing from the
context cap. It would also rank a server with one catastrophic defect above one
with two trivial ones.

The **protocol sub-score** is continuous but wrong, because it also moves on
MEDIUM defects. An off-spec capability is a real finding and not a framing
break; letting it drag the ceiling would cap servers that never violated the
contract this rule exists to enforce. In the fixture above, the off-spec
capability contributes to the 75 sub-score and contributes **nothing** to the
ceiling — correctly.

The **total HIGH-severity deduction** is continuous *and* selective: it moves
only on defects that stop clients working, and it moves smoothly with how many
there are and how bad each one is.

**The slope is 1.0, and that is a refusal rather than a tuning.** The `PROTOCOL_*`
penalty table already encodes how bad each protocol defect is, in points. The
ceiling reuses that judgement one-for-one instead of asserting a fresh slope
that would need its own justification and its own maintenance. The rule reads in
one line: *a High protocol finding costs the composite ceiling exactly what it
cost the protocol dimension.*

The handover proposed ~85 for one finding and ~75 for two. One lands exactly;
two lands at 70 rather than 75, because two independent breaks of the framing
contract is a materially worse server than one and the penalty table already
says so. Choosing a 0.83 slope to hit a round 75 would have bought five points
of agreement with a number nobody could derive.

**The effect on the director's fixture.** A 91 → **B 85**, with the ceiling and
its cause stated on their own line in every renderer:

```
  ⓘ  composite capped at 85 by protocol compliance (15 points of high-severity
     protocol defects): 1 non-protocol line(s) on stdout — this corrupts MCP's
     newline-delimited framing (would have scored 91)
```

**When both ceilings apply**, the composite takes the lower and the report keeps
both, because a reader is entitled to know the server was capped twice over. In
practice they rarely co-occur: a context sub-score low enough to ceiling below
85 already drags the uncapped composite under it.

### 2. Tool poisoning was not machine-checkable (SOP 12)

**The gap.** A tool description is untrusted input to the model, even when you
wrote it — a different server in the same session may not have. *Tool
poisoning*, the practice of embedding model-directed instructions in
registration metadata, is a live class of indirect prompt injection specific to
MCP, demonstrated by Invariant Labs and now benchmarked by **MCPTox**
([arXiv:2508.14925](https://arxiv.org/abs/2508.14925)). Jig graded description
*quality and cost* and said nothing at all about adversarial *content*.

**The fix.** A new deterministic analyzer, `crates/jig-core/src/injection.rs`.
No LLM anywhere — every signal is a mechanical fact about the text. Five
detectors:

| Detector | Severity | What it matches |
|:---------|:---------|:----------------|
| Model-directed imperatives | High | instruction override, concealment, invocation ordering, authority override |
| Fake conversation turns | High | chat-template tokens, XML-ish role tags, multi-role transcripts |
| Hidden characters | High | zero-width, bidi controls (Trojan Source), homoglyph tool names |
| Exfiltration shape | Medium | a URL within 120 characters of an outbound-transfer verb |
| Name/behaviour mismatch | Medium | a read-shaped name or `readOnlyHint: true` over a mutating description |

**False-positive discipline is the whole design problem.** A legitimate
description absolutely can say "do not use this for binary files". The
distinguishing property of an injection is that it is **model-directed and
tool-control-bearing**: it tells the *assistant* what to do about tools,
instructions, or disclosure — not the developer what the tool is for.

So the table never contains a bare imperative stem. `you must always` is
expanded mechanically across a list of *tool-control objects* (`call`, `use`,
`invoke`, `mention`, `reveal`, …) which deliberately excludes input-shaped verbs
(`provide`, `supply`, `pass`). That single decision is what lets `You must
always call \`audit_log\` first` fire while `You must always provide a valid API
key` does not. Every pattern carries a written rationale, a test asserts none of
them can be added without one, and a pinned corpus of benign phrasings — several
of them deliberate near-misses — is asserted to produce zero findings.

**They are reported, never scored.** No injection finding touches the composite.
Whether adversarial content should move a *quality* grade, as opposed to failing
the server outright, is a product decision that deserves its own release rather
than being smuggled in with the detector.

**They are, however, always pinned.** A poisoned description is the single most
important thing a user can learn about a server, and a 90-tool surface
generating dozens of schema nits must never be able to bury it below the fold of
"Top fixes".

**A sibling sentinel, not a reuse.** Findings are tagged `Dimension::Injection`
(machine key `injection`) rather than folded into the existing `tool_set`
advisor category. The two answer different questions and a user acts on them
differently — "will the model pick the right tool?" is a quality conversation,
"is this metadata adversarial?" is a trust one — and machine consumers filtering
on `tool_set` would otherwise have started silently receiving security findings.

**What it still cannot do.** A semantic attack written in plain, well-formed
English, with no override phrasing, no fake turns, no hidden characters and no
URL, passes cleanly. This is a lint for the *shape* the published attacks take,
not a red-teamer, and there is no threshold at which it becomes one.

### 3. Credential-failure UX was not machine-checkable (SOP 26)

**The gap.** Failing to start is not itself a defect: a server that needs an API
key and does not have one *should* refuse. What varies — and what the user
actually experiences — is the **shape** of the refusal. The census measured 29
servers over stdio; **14 died on a missing credential and 2 hung until the
timeout fired**. Those populations were indistinguishable in a report that only
recorded "did not start", and they are not remotely the same product.

**The fix.** When a stdio server fails to connect, Jig re-launches it once under
observation and grades how it failed, parsing the child's retained stderr for an
environment-variable name.

| Observed failure | Verdict | Severity | Robustness sub-score |
|:-----------------|:--------|:---------|---------------------:|
| Exits nonzero **and** names the variable | Pass | Info | — (no sub-score) |
| Exits nonzero without naming it | *fail fast is right; say which variable* | Medium | 60 |
| Hangs until timeout | never hang on a missing credential | High | 0 |
| Exits **zero** on a failed start | a client cannot distinguish this from success | High | 0 |

A hang and a zero-exit both score 0, for different reasons. The hang gives the
client no signal at all, so the user waits out a timeout and blames the client.
The zero-exit is worse in kind if not in degree: it is an affirmative lie, and a
supervisor that reads it as success will not restart.

**The Pass case earns nothing.** Naming a variable in stderr is not proof the
server documents it, and this rule cannot distinguish a genuine credential
failure from any other non-zero exit that happens to mention a capitalized
identifier. So it only ever *penalizes* the three shapes that are unambiguously
worse for the user, and never rewards the good one with points it cannot
justify.

**The guard that keeps it honest.** The probe sends a well-formed `initialize`
and watches stdout. A server that **answers** did not fail to start, whatever
went wrong afterwards, and is graded `NotObserved` rather than given a verdict
this rule is not entitled to reach. The probe also holds stdin open for its
whole window: closing it would send EOF, and a correct server exits 0 on EOF —
which would then have to be read as "exited zero after a failed start".

`jig check` and `jig info --probe` print the same verdict line from the same
core function, so the two commands cannot disagree about the same server.

### 4. "Cold start" conflated npm download with server boot (SOP 25)

**The defect.** Jig's own README advertised an **8-second `npx` cold start** for
`@modelcontextprotocol/server-everything`, and SOP 25 cited it as evidence that
authors should budget their cold start. That number is two numbers glued
together: npm resolving and downloading a package tree, and the server process
actually booting and answering `initialize`.

Only the second is a property of the server. The first belongs to the registry,
the network, and whether the user has run this package before — and it is paid
**once**, not per session. Grading them as one figure told authors to optimize
something most of them do not control, and let a genuinely slow boot hide inside
a big download.

Worse, the 8s figure traces to a caption on a **design-prototype screenshot**,
not to a recorded measurement. It has been removed from the README rather than
restated.

**The fix.** For `npx`-shaped commands Jig runs a pre-warm pass first:

```text
npx --yes --package <pkg> -- node -e ""
```

This installs the package into the `_npx` cache and then runs a trivial `node`
program *instead of* the package's own binary, so the cache is populated without
the server ever starting. That pass is timed as **install**; the real launch is
then timed from spawn to the `initialize` response and reported as **boot**.

```
install 12.5s · boot 8.8s
```

**Only boot is scored.** Install is reported and never graded. Non-`npx`
commands report install as `n/a`; `--no-prewarm` skips the pass for offline,
air-gapped, or known-warm runs and reports install as `skipped` — a state
deliberately distinct from `n/a`, so "we did not look" is never rendered as
"there was nothing to look at".

**The measured split.**

Measured on Windows 11 / Node 22.16 / warm network, against
`@modelcontextprotocol/server-everything`, using `jig check` itself:

| Run | install | boot |
|:----|--------:|-----:|
| cold cache (`_npx` deleted) | **12.5s** | 8.8s |
| warm cache | 2.0s | 3.1s |
| warm cache, repeat | 2.0s | 2.8s |

The headline: the old single figure was **dominated by install**, and neither
half of it was stable. On a cold cache the download alone is 12.5s — larger than
the whole 8s Jig used to quote — while the part the author actually controls is
a fraction of it.

Two further facts the split exposes, both of which the conflated number hid:

**Boot is not constant across runs of the same server.** 8.8s cold versus
~2.9s warm, for a package that is by then fully downloaded in both cases. The
residual is npm's own resolution work on first use of a cache entry, charged to
a number that is supposed to describe the server. This is why `--no-prewarm`
reports `install skipped` rather than `n/a`: a warm-cache run and a
never-looked run produce very different boot figures and must not be conflated
in turn.

**Most of even the warm boot is not the server.** Timing the cached entrypoint
directly — `node <cache>/dist/index.js`, bypassing the `npx` shim — the same
server answers `initialize` in **0.30s** (0.303 / 0.300 / 0.289 over three
runs). So of the ~2.9s Jig reports as boot, roughly 2.6s is npm shim overhead
and 0.3s is `server-everything` actually starting.

Jig does **not** subtract that 2.6s, and the honest reason is that it cannot do
so defensibly from one session: the correction is not a constant, and measuring
it would require timing a null server through the same path on every run.
Reported boot is therefore an **upper bound** on server boot — the safe
direction for a grade, and now a documented one rather than an unexamined one.

**Honesty about what boot still contains.** Even after the split, boot for an
`npx` command includes npm's own shim resolution and process launch — Jig times
the launch, not the server's first instruction. Subtracting it would require
timing a null server through the same path and asserting the difference is
constant, which it is not. The number therefore slightly **over-estimates**
server boot, which is the safe direction for a grade.

### What did *not* change

- **Dimension weights.** Still 25 / 25 / 20 / 15 / 15.
- **Grade bands and badge colors.** Still `A >= 90 · B 80–89 · C 70–79 · D 60–69 · F < 60`.
- **The context-cost cap.** The `rubric-v1.2` ramp and its census anchors are untouched.
- **Rate-based scoring.** Shrinkage, class weights and the floor of 15 are untouched.
- **Measurement.** Context cost is still gpt-4o exact tokens over the canonical rendering.
- **Findings.** Every pre-existing defect still produces the same finding with the same fix text.
- **Not-applicable handling.** A dimension with no observations is still excluded from the composite, never assumed to be 100.
- **No LLM.** Every detector added here is deterministic, and the only heuristic dimension is still description quality.

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
