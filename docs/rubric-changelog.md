# Rubric changelog

Jig's report card is versioned. Every score Jig emits — human report, `--json`,
`--badge`, the HTML report card — carries the `rubricVersion` that produced it.

> **Scores from different rubric versions are not comparable.** A `rubric-v1`
> 73, a `rubric-v1.1` 73, a `rubric-v1.2` 73, a `rubric-v1.3` 73, a
> `rubric-v1.4` 73 and a `rubric-v1.5` 73 were
> produced by different arithmetic and mean different things. This is a standing property of the
> rubric, not a caveat attached to any one release. When comparing servers, or comparing one server over time,
> check that the rubric versions match before reading anything into the delta.
> Re-run the older subject under the current rubric instead of adjusting the old
> number by hand.

---

## Outside the rubric: judged description quality (`jig check --judge`)

`jig check --judge` asks a model whether each tool description states its
purpose, distinguishes its siblings, and documents its parameters. **That output
is explicitly OUTSIDE `rubric-v1.5`** and outside every rubric version — present
and future — until a changelog entry says otherwise.

It is not a dimension, it has no weight, and it is not an input to the
composite, any dimension score, the grade, the badge, or `--min-score`. It does
not affect `rubricVersion`, which continues to describe the deterministic score
only. Two `rubric-v1.5` reports on the same server are comparable whether one of
them was judged and the other was not — an integration test asserts the
deterministic document is byte-identical either way.

The reason it sits outside is the same reason the rubric is versioned at all: a
score has to mean the same thing twice. A model's answer is not reproducible, is
not pinned to an arithmetic, and would silently change as providers retire and
replace models underneath a fixed model id. So the judged verdict carries its
own provenance instead — `JUDGE_PROMPT_VERSION`, the verbatim prompt, the
temperature, and the model id **as the provider reported it** — and stays out of
the number.

The scored `description_quality` dimension is unchanged and remains
deterministic, heuristic, and labelled as such in every report.

---

## Outside the rubric: a score describes an invocation, not a package

Jig grades the tool surface a server advertises **under the command line it was
given** — in practice the bare `npx -y <package>` default. Many servers ship a
lighter mode behind a flag (`--preset`, `--enabled-tools`, a discovery mode);
nothing in `initialize` or `tools/list` advertises that, so Jig cannot see it by
connecting, which is the only thing it does.

No rubric version has ever claimed otherwise, but the report used to leave it
implicit. It no longer does. Every output surface states the exact invocation
measured — an `invocation` field in `--json`, a `measured:` line in the human
report card header and in the HTML report — and the context-cost dimension
qualifies its token count **as invoked**. Any secret in the invocation (URL
userinfo, a query token, a secret-named flag or `NAME=value` assignment) is
redacted before it is printed.

This is presentation only. **No score, weight, cap, threshold or finding code
changed**, `rubricVersion` is unchanged, and reports produced before and after
this change are directly comparable. See
[issue #6](https://github.com/Shodh-Labs/jig/issues/6) for the structural
problem this does *not* solve: a server that has fixed its surface behind a flag
still grades identically to one that has not.

---

## `rubric-v1.5`

Three changes, none of them new ideas. Each one was named, argued for, and
**deliberately deferred** by an earlier release for the same stated reason: the
data to do it honestly did not exist. `rubric-v1.2` named the missing dataset
twice. `rubric-v1.4` named it again and declined to rebalance weights without it,
writing that doing so *"would repeat exactly the error `rubric-v1.2` corrected in
`rubric-v1.1`: asserting anchors rather than calibrating them."*

The dataset now exists. **Census v2** ran `jig check` across 127 public MCP
servers, of which **63 were reachable and graded**, and recorded every
per-dimension score — `data/census2-calibration.json`. This release spends it,
and nothing here is asserted that could have been fitted.

### 1. The weights were editorial. They are now fitted.

**The defect** is `rubric-v1.4`'s own recommendation (a), which it analysed and
declined to act on. Its 26-server sample showed three of five dimensions
near-constant, so a 25%-weighted context-cost dimension set essentially the whole
order: Spearman(composite, −tokens) = 0.959. The composite was a token ranking in
a five-dimension costume.

Census v2 confirms the shape at 63 servers, and locates it precisely:

| Dimension | `v1.4` weight | p25 | median | p75 | sd |
|:----------|-------------:|----:|-------:|----:|---:|
| Protocol compliance | 25 | 100 | 100 | 100 | 7.85 |
| Context cost | 25 | 43.1 | 87.1 | 96.6 | 32.40 |
| Schema hygiene | 20 | 95.9 | 98.7 | 100 | 3.60 |
| Description quality | 15 | 95.9 | 98.3 | 99.1 | 2.73 |
| Robustness | 15 | 89.9 | 95.3 | 99.3 | 5.38 |

**Protocol compliance is a constant among servers that answer at all.** Its p25,
median and p75 are all exactly 100; its mean is 98.49. Its non-zero sd is four
servers, three of which are also capped. A quarter of the composite was being
spent on a dimension that cannot distinguish the middle half of the fleet from
itself.

That is *not* an argument that protocol compliance is unimportant — it is the
most important thing the tool measures. It is an argument that its weight was the
wrong instrument for saying so, and the right one was already in place: the
`rubric-v1.3` **protocol ceiling**, which bounds the composite outright when
framing breaks. A broken server is disciplined by the ceiling, not by the mean.
The ceiling is untouched by this release.

**Robustness is the only craft dimension with real spread**, and only because
`rubric-v1.4` fixed its measurement. Its own changelog worried that subtracting
the npm shim had *"replaced one constant with a better-justified one"*. The fleet
says otherwise: p25 89.9 → p75 99.3, sd 5.38, against schema hygiene's 3.60 and
description quality's 2.73. It earned weight; it now has it.

**The fit.** Candidate sets were evaluated offline against all 63 fleet cards on
two measures — the composite's standard deviation (does it still separate
servers?) and Spearman(composite, −context tokens) (is it still just a token
ranking?) — plus the mean absolute grade movement, because churn is a cost paid
by every published score.

| Weights | sd | ρ(composite, −tokens) | mean \|Δ\| | Verdict |
|:--------|---:|----------------------:|-----------:|:--------|
| `{25,25,20,15,15}` — `v1.4` | 11.92 | 0.854 | — | baseline |
| **`{15,25,20,15,25}`** | **11.22** | **0.840** | **0.74** | **chosen** |
| `{15,35,10,10,30}` — variance-proportional | 12.56 | 0.855 | 2.95 | rejected |
| `{17,34,11,10,28}` — sqrt-variance | 12.36 | 0.857 | 2.61 | rejected |

**The two "principled" candidates are the ones that fail.** Weighting each
dimension by its variance is the obvious statistical move, and on this data it is
exactly wrong: context cost has by far the largest variance, so variance-
proportional weighting hands it *more* of the composite (25 → 34–35) and pushes ρ
**up**, from 0.854 to ~0.856. They optimise for spread and buy it by making the
composite more of a token ranking — the precise defect the exercise existed to
reduce. They also churn grades four times as hard.

The chosen set is the only candidate that **reduces** ρ (0.854 → 0.840), and it
does so at the smallest movement of the four (mean |Δ| 0.74). It moves weight
between two dimensions and leaves the other three alone.

**What this does not fix, stated plainly.** ρ = 0.840 is still high. Context cost
still explains most of the ordering, because on this fleet it is still the only
dimension with wide spread — the craft dimensions cluster in the 90s because most
published servers really are clean on them. Rebalancing weights cannot manufacture
variance that the ecosystem does not have. The honest reading is that **the
composite remains substantially a cost ranking**, and the remedy for that is
`rubric-v1.4`'s recommendation (a)(3) — splitting cost from craft — not a further
turn of the weight screw. This release does change 3 above, which makes the
situation visible rather than arguable.

### 2. A single dimension may bound the composite. It may no longer fail it.

**The defect** is `rubric-v1.4`'s recommendation (b), quoted here because it made
the case better than a restatement would: `dataforseo-mcp-server` scores protocol
**100**, schema hygiene **100**, description quality **100** — a perfect card on
every craft dimension — and graded **F 55**, solely because 89 tools cost 42,288
tokens and pinned it to the context-cap floor.

Both halves of that report are individually defensible and together they are
incoherent. A reader who sees three 100s printed above an F does not conclude the
server is bad; they conclude the instrument is broken, which costs Jig exactly the
credibility the context finding needs in order to land.

**The fix.** The applied context cap is floored at **60** — the D/F boundary:

```text
cap = max(context_cap_ceiling(sub), 60)
```

The **ramp is untouched**. Every anchor, its slope and its census calibration are
byte-identical to `rubric-v1.2`; `context_cap_ceiling(5)` still returns 55. Only
the ramp's *output* is floored. Re-sloping the ramp from a base of 60 was the
obvious alternative and was rejected: it would have silently moved every
intermediate ceiling (the p93 server's from 86.0 to 87.6, and so on down the
table) and invalidated a calibration this release has no evidence to revise. The
distinction is between "the cap cannot say worse than D" and "the cap means
something different at every percentile".

Only the harshest stretch of the ramp is affected — sub-scores below ~6.9, where
the raw ramp reads under 60. Everything above is unchanged.

**The cap's original purpose is intact, and it is checked rather than asserted.**
The cap exists to stop a heavy server *outranking* a light one on schema polish
(`rubric-v1.1`, defect 2). In census v2 **no uncapped server scores below 63**, so
a heavyweight held at 60 still ranks below every well-proportioned server in the
fleet. That is the property the floor had to preserve, and it is now a regression
test rather than a paragraph.

**The protocol ceiling is deliberately not floored**, and this is the substantive
judgement in the change. The two ceilings shared a constant (55) and now do not,
because they never meant the same thing. Every trigger of the protocol ceiling —
polluted stdout, an unanswered `*/list`, an accepted unknown method — is a server
that **breaks its own contract**. That is what F is for. A large-but-correct
server is not in that class, and putting it there devalues the letter for the
servers that earn it. Flooring both would have deleted the very distinction the
floor was introduced to protect.

A server can still reach F on context cost. It now has to get there by
*combining* catastrophic context cost with genuine defects elsewhere — a
statement the rest of the card supports.

**The cap line says so when the floor binds**, on the same discipline
`rubric-v1.2` applied when it made the ramp state its own input:

```
composite capped at 60 by context cost (context sub-score 5): 42,288 tokens is 24× the census median (D floor: a single dimension bounds the composite but cannot reach F alone)
```

The clause is conditional. It appears only where the floor actually set the
ceiling, never as boilerplate on every cap.

### 3. The fleet spread is now printed beside every score

This is `rubric-v1.4`'s recommendation (a) **option 2** — *"report the spread…
cheap, purely additive, and it makes the defect visible instead of arguable"*,
marked there as *"Recommended as the next step"* — implemented as written.

Each dimension line and each `--json` dimension object now carries the census-v2
**p25 · median · p75** for that dimension:

```
  ✓  Protocol compliance  100  [100·100·100]  clean handshake, no stdout pollution, spec-valid capabilities
  ✓  Context cost          99  [43·87·97]     183 tokens (no ecosystem data — absolute bands)
  ✓  Schema hygiene        96  [96·99·100]    `make_reservation`: parameter `party` missing a description (+1 more)
  ✓  Description quality   99  [96·98·99]     heuristic · 3 tool(s) have no human-facing title
  ✓  Robustness           100  [90·95·99]     list 12ms, clean shutdown
```

A reader can now see, without taking the weights on trust, that a protocol 100 is
the fleet's *median* and separates the server from nobody, while a context 99 is
genuinely distinguishing. The argument in change 1 is legible from the output of
any single run.

`--json` gains an additive `fleetSpread` object per dimension, carrying the exact
decimals; the human line rounds to whole numbers because it is orientation, not
arithmetic. A legend in the footer says what the bracket is and that it is **not
scored**.

**It is a new file, not an extension of `data/percentiles.json`.** The new dataset
is `data/dimension-spread.json`, bundled with `include_str!` alongside the census.
`data/percentiles.json` is deliberately untouched: it is a *scoring* input, its
anchors are the curated `v1` cohort, and folding an unvetted 63-server fleet into
it would move published grades under cover of a reporting change. This file enters
no score, no finding and no ranking.

### Grade impact on the fleet

Re-scoring all 63 census-v2 cards under the new weights and the floor together:

| | A | B | C | D | F |
|:--|--:|--:|--:|--:|--:|
| `rubric-v1.4` | 42 | 9 | 8 | **0** | **4** |
| `rubric-v1.5` | 41 | 9 | 9 | **3** | **1** |

The D band was empty and is now populated, which is the floor doing exactly what
it was built to do: three servers that read F on size alone now read D on size
alone. Mean |Δ| across the fleet is **0.74** points.

**The one remaining F is `@agentdeskai/browser-tools-mcp`, and it is
protocol-capped** — it pollutes stdout. It is broken, not big. That is the
sentence the change was for: after this release, an F in a Jig report means the
server does not work, and there are no exceptions on this fleet.

### Monotonicity

Both changes preserve the standing guarantee — **no server can score worse by
improving any dimension** — and both arguments are short enough to check.

**Weights.** They are positive constants (15, 25, 20, 15, 25) and the composite is
`Σ(score·weight) / Σ(weight)` over applicable dimensions. A weighted mean with
positive constant weights is strictly increasing in every input. Changing which
positive constants they are cannot introduce non-monotonicity; it changes the
gradient, not its sign. Asserted by a test that pins each weight and the exact
composite arithmetic.

**The floor.** `max(ramp(sub), 60)` is the pointwise maximum of a monotone
non-decreasing function and a constant, which is monotone non-decreasing. So
worsening context cost still can never *raise* the ceiling, and the existing
dense-sweep property test over the reported composite (`min(uncapped, ceiling)`)
continues to pass unchanged.

The floor also cannot inflate a score. It raises a **ceiling**, and a ceiling is
applied with `min`. A server whose uncapped composite is already below 60 has no
cap reported at all and keeps the number its dimensions produced — the floor never
rescues a server that its own dimensions failed.

### Comparability

**`rubric-v1.5` composites are not comparable to `rubric-v1.4` composites** for:

- any server whose protocol and robustness scores differ from each other (the
  reweighting moves it — upward if robustness leads, downward if protocol does);
- any context-capped server (the floor moves it, by up to 5 points).

A server scoring 100 on both protocol and robustness is unmoved by the
reweighting, because shifting weight between two equal values changes nothing —
which is why mean |Δ| is under a point despite a 10-point weight transfer.

**Per-dimension scores are unchanged.** Every dimension is scored by exactly the
arithmetic `rubric-v1.4` used; no finding was added, removed or reworded except
the context-cap line, which gained its conditional floor clause. A `v1.4`
robustness 95.3 and a `v1.5` robustness 95.3 mean the same thing. Only the
composite that combines them changed.

### The dataset, and what is wrong with it

The fit is only as good as census v2, so its limits are stated rather than
buried — the same disclosure `rubric-v1.2` made about its own missing data.

- **n = 63**, from 127 attempted. The other 64 never became reachable.
- **One machine, one run.** No repetition, no second host, no error bars.
  Robustness in particular is timing-derived and therefore the dimension most
  exposed to this — and it is the dimension that gained weight.
- **Selection skew.** The fleet extends the curated `v1` cohort with unvetted
  additions pulled from the npm pool, but the *reachable* subset skews back toward
  the curated cohort, because curated servers are likelier to start. So the fleet
  is plausibly **cleaner than the ecosystem**, which would compress the very
  spreads these weights are fitted to.
- **The spreads describe a sample, not a population.** They are labelled that way
  in `data/dimension-spread.json` and should be read that way.

None of this makes the fit worse than the editorial weights it replaces, which
rested on no dataset at all. It does mean the weights should be **re-fitted, not
defended**, when a second fleet run exists.

### What did *not* change

- **Grade bands and badge colors.** Still `A >= 90 · B 80–89 · C 70–79 · D 60–69 · F < 60`.
- **The context-cap ramp.** Every anchor and its census calibration are untouched;
  only the output is floored. `context_cap_ceiling(5)` still returns 55.
- **The protocol ceiling.** The `rubric-v1.3` ramp, its slope of 1.0 and its floor
  of 55 are all untouched — including, deliberately, the fact that it can reach F.
- **Every per-dimension scoring rule.** Rate-based scoring, shrinkage, class
  weights, the floor of 15, the robustness anchor table and the launcher-floor
  subtraction are all exactly as `rubric-v1.4` left them.
- **`data/percentiles.json`.** Untouched, still the curated `v1` cohort, still the
  only dataset that reaches a score.
- **Injection and advisor posture.** Still reported, never scored, always pinned.
- **Findings.** Same set, same fix text, one line changed (the context cap).
- **No LLM.** Nothing added here is non-deterministic.

---

## `rubric-v1.4`

Motivated by a **50-server fleet run under `rubric-v1.3`**, which exposed three
defects. All three are defects in Jig's *own* measurement, not in the servers:
the first fires on documentation quality, the second grades a cold npm cache as a
server defect, and the third presents a one-dimensional ranking as a
five-dimensional grade. None of the three could have been found without running
the rubric at fleet scale, which is the argument for doing so before every
release.

Two of the three are precision fixes and one is a resolution fix. Where
`rubric-v1.3` added measurements, this release makes the existing ones mean what
they claim.

### 1. The injection lint's precision was 0/6

**The defect.** The `rubric-v1.3` name/behaviour-mismatch detector matched a
mutation verb **anywhere** in a tool's description. Across 50 servers it produced
six findings, and **every one of the six was a false positive**:

| Tool | Text that fired | What the text actually is |
|:-----|:----------------|:--------------------------|
| `read_file` | "Prefer this over `execute_command`" | a **comparative clause naming another tool** |
| `get_config` | `fileWriteLineLimit` | a verb inside a **config field name** |
| `get_prompts` | "Create organized knowledge base" | a **menu label** in a bulleted list |
| drawio `get_shape_catalog` | "…to create new vertex cells" | **caller guidance** in a purpose clause |
| drawio `get_graph` | "The response removes circular dependencies" | **response-sanitization prose** |
| firecrawl (exfiltration shape) | `https://example.com` in a JSON usage example | a **documentation placeholder** beside a documented `webhookUrl` feature |

The `read_file` case is the one that settles it. That description is steering the
model *away* from shelling out and *toward* a narrower, safer tool — the exact
practice a security lint exists to encourage — and `rubric-v1.3` penalized it for
saying so. A lint that fires on documentation quality is worse than no lint,
because it teaches authors that the way to a clean report is to document less.

**The fix.** The detector is scoped to the tool's **action clause** — the
sentence whose head predicate is *this tool* — by four filters, each of which
kills at least one of the six:

1. **Non-prose is masked.** Fenced code blocks, inline code spans, JSON object
   literals, and identifier tokens (`camelCase`, `snake_case`, `dotted.path`) are
   blanked before anything is read. Masking replaces characters with spaces
   rather than deleting them, so line and sentence structure survive exactly.
2. **List items are dropped.** A bulleted or numbered line is an enumeration, not
   a predication. `get_prompts` naming a prompt is not a claim that it creates
   one.
3. **Comparative clauses are dropped.** A sentence containing `prefer`, `unlike`,
   `rather than`, `instead of`, `as opposed to` (and their siblings) is about a
   *different* tool.
4. **The verb must be the clause's head predicate.** Only a **tool-referring
   subject** may precede it — `this`, `it`, `the tool`, a conjunction, or a
   read-shaped verb it is conjoined to. So `Deletes stale rows`, `This tool
   deletes rows` and `Reads and deletes rows` all match, while `Use this format
   to create cells` (a purpose clause addressed to the caller) and `The response
   removes cycles` (subject: the *response*) do not.

The exfiltration detector gets the masking plus a fifth rule: **reserved
documentation hosts are not destinations.** `example.com` and friends are
RFC 2606 placeholders; `localhost`, `your-domain`, and the rest are the same
thing by convention. A description that uses one is showing the caller a shape.

The `rubric-v1.3` negation window is replaced by a clause-level negation check,
which is both simpler and strictly more accurate now that clauses are delimited:
a fixed 32-character window can stop mid-clause and miss a negation a reader
plainly sees.

**The result, measured against the six real strings.**

| | `rubric-v1.3` | `rubric-v1.4` |
|:--|:--|:--|
| Findings on the six fleet cases | 6 | **0** |
| Precision on the fleet | **0/6** | no findings to be wrong about |
| True-positive cases still caught | 6/6 | **6/6** |

The six descriptions are pinned verbatim as `FLEET_FALSE_POSITIVES` in
`injection.rs` and are the regression suite: any future widening of the detector
has to keep them clean. A companion test asserts each of the four filters is
load-bearing in isolation, so none can be quietly deleted, and a third asserts
the real mismatch signal — `get_report` saying "Deletes stale rows",
`sync_state` with a false `readOnlyHint` — still fires through every filter.

**Why this stayed a scored… and did not.** The brief allowed making the lint
advisory-only if precision could not be raised without losing signal. It could,
so it was not needed — and in any case injection findings were **already
reported and never scored** under `rubric-v1.3`, and remain so. Nothing here
touches the composite. What changed is whether a user is told something false.

**Monotonicity.** Unaffected: no injection finding has ever carried `points`, so
the scoring shape is unchanged and there is nothing to argue. The change is
strictly *subtractive* in findings — every description flagged under
`rubric-v1.4` was also flagged under `rubric-v1.3`.

### 2. The credential probe timed the npm shim, not the server

**The defect.** The credential-UX probe (`rubric-v1.3`, SOP 26) gave a server
4 seconds from **spawn** to exit or answer, and called anything slower `Hung` —
the harshest verdict the rubric can reach: High severity, robustness **0**,
rendered as *"never exited and never answered"*.

But `rubric-v1.3`'s own SOP 25 work measured the `npx` shim at **~2.6s before
the server's code runs**. So the window the server actually got was under 1.4s.

| Server | Behaviour | Time from spawn | `v1.3` verdict | Correct verdict |
|:-------|:----------|----------------:|:---------------|:----------------|
| `server-slack` | exit 1, named `SLACK_BOT_TOKEN` | 3.83s / 3.86s | **Hung** | PASS |
| `server-gitlab` | exit 1, named the variable | 5.19s / 7.84s | **Hung** | PASS |

Both are the rubric's **own PASS shape** — fail fast, name the variable — and
both were recorded as the worst thing it can say about a server. **13 of the 24
fleet failures were affected.**

It was also non-deterministic. `server-gitlab` flipped to PASS on a warm npm
cache, so the same server graded differently on the same machine depending on
whether someone had run it that week. A grade that moves with the state of a
package cache is not a grade.

**The fix.** The probe window is measured from the **child's first byte of
output**, not from spawn. Whatever the launcher spent resolving and spawning is,
by construction, over by the time the server's own process writes anything, so
the launcher cost is subtracted without having to be modelled. A second bound —
`PROBE_HARD_CAP`, 30s from spawn — catches a child that never writes at all,
which is the only case where "never answered" is literally true. It is set well
above the worst launcher cost the fleet measured (7.84s) plus a full window, so a
cold cache alone can never consume it.

The implementation is a sliding deadline in `tokio::select!`: before first output
the bound is the hard cap; after it, `PROBE_TIMEOUT` from that instant. The exit
branch is `biased` so a process that exits in the same tick as the deadline is
read as an exit — **a server that exited is never a hang.**

**Monotonicity.** The verdict lattice is unchanged (`NamedVariable` →
`UnnamedVariable` → `Hung`/`ExitedZero`) and so are its sub-scores. The change is
strictly *lenient*: widening the window can only move a server from `Hung`
toward a better verdict, never the reverse, because `Hung` is what the timeout
produces and the timeout now fires later or not at all. No server can score worse
under `rubric-v1.4` than it did under `rubric-v1.3` on this rule.

### 3. Robustness was a constant wearing a dimension's clothes

**The defect.** Across the 26 graded servers the fleet run measured:

| Dimension | Weight | Spread across 26 servers |
|:----------|-------:|:-------------------------|
| Robustness | 15 | **exactly 80 for all 26** — zero |
| Protocol compliance | 25 | 100 for 25 of 26 |
| Schema hygiene | 20 | rank correlation with composite **0.148** |
| Description quality | 15 | some |
| **Context cost** | **25** | **Spearman(composite, −tokens) = 0.959** |

Three of five dimensions are near-constant, so the 25%-weighted context-cost
dimension sets essentially the whole order. The composite was a token-count
ranking in a five-dimension costume — which is the `rubric-v1.2` complaint
("cap thresholds were asserted, not calibrated") wearing new clothes.

Robustness's exact-80 is arithmetic, not coincidence. Its three sub-scores were
`latency 100`, `boot 40`, `shutdown 100`, and their mean is 80. Every `npx`
server tripped the same boot penalty, for two compounding reasons:

**(a) The boot number was mostly npm.** `rubric-v1.3`'s changelog says so
outright: of ~2.9s reported boot for `server-everything`, roughly **2.6s was the
npx shim and 0.3s was the server**. It declined to subtract the 2.6s, and its
stated reason was specific and testable — *"measuring it would require timing a
null server through the same path on every run."*

**(b) The ramp was a three-step bucket.** `<= 1s` → 100, `<= 3s` → 70, else 40.
Two servers differing by 6 seconds scored identically; 2.999s and 3.001s differed
by 30 points. That is precisely the discontinuity `rubric-v1.2` spent a release
deleting from the context cap.

**The fix, part one: measure the launcher floor.** Jig now does exactly the thing
`rubric-v1.3` described as the prerequisite. The pre-warm pass runs **twice**.
The first populates the `_npx` cache and is timed as *install*, unchanged. The
second runs the identical command against the now-warm cache — `npx --yes
--package <pkg> -- node -e ""`, a **null program through the identical path** —
and is timed as the **launcher floor**.

```text
server_boot = max(boot − launcher_floor, 0)
```

The correction is measured per run rather than asserted as a constant, which was
the whole of `rubric-v1.3`'s objection to subtracting it. Only `server_boot` is
scored. It costs one extra warm-cache spawn, on `npx` targets only.

The subtraction is **never silent** — the same discipline `rubric-v1.2` applied
to the context cap, which states the sub-score that produced it:

```text
install 12.5s · boot 0.3s (2.9s launch − 2.6s npx shim)
```

`--json` gains `launcherSeconds` and `serverBootSeconds`; `bootSeconds` is
retained unchanged so a consumer can still see the raw launch, and `scored` moves
from `"boot"` to `"serverBoot"`. Saturating at zero is deliberate: launcher cost
is noisy, and a server that beat the null program is at the floor of measurement,
not below zero. Where no floor could be measured — a non-`npx` command, or a
failed pass — nothing is subtracted, which is the `rubric-v1.3` behaviour and the
safe direction.

**The fix, part two: a continuous ramp.** The boot and latency sub-scores now
interpolate a shared anchor table instead of bucketing:

| Milliseconds | Sub-score | Provenance |
|-------------:|----------:|:-----------|
| 0 | 100 | instant |
| 1,000 | 100 | the `rubric-v1.3` "fast" edge, **preserved exactly** |
| 3,000 | 70 | the `rubric-v1.3` "sluggish" edge, **preserved exactly** |
| 10,000 | 40 | new — beyond the old cliff |
| 30,000 | 15 | new — the dimension floor `rubric-v1.1` established |

Passing through the old bucket edges is the point: this changes the
**resolution** of the dimension without moving the judgement it encoded, so no
server's score jumps because the shape changed. What is new is the tail —
`rubric-v1.3` floored at 40 the moment a server crossed 3s, so a 3.1s boot and a
60s boot were indistinguishable.

**Monotonicity argument.** The anchor table is ascending in time and
non-increasing in score, and linear interpolation between adjacent anchors
preserves both properties. `timing_subscore` is therefore monotone non-increasing
in milliseconds across its whole domain, and clamped to `[15, 100]`: **a server
can never raise its robustness score by getting slower.** Asserted by a dense
sweep from 0 to 60,000ms. The subtraction in part one is separately monotone —
a larger floor never yields a larger scored boot, and the scored boot never
exceeds the raw boot — so no server can score *worse* under `rubric-v1.4` than
`rubric-v1.3` on this dimension.

**Does robustness now have spread?** Yes, and for both reasons. Raw launches of
3.1s / 5s / 8.8s / 20s / 45s all scored 40 under `rubric-v1.3` and now produce
five distinct sub-scores in the correct order; and the shim subtraction moves a
typical `npx` server's boot from ~2.9s (sub-score 70, dragging the dimension to
80) to ~0.3s (sub-score 100). A test pins both properties.

**What this does *not* fully fix, stated plainly.** Subtracting the shim moves
most `npx` servers to a boot sub-score of **100**, which replaces one constant
with a better-justified one. The honest reading is that *server boot, correctly
measured, genuinely does not vary much* — nearly every MCP server answers
`initialize` in a fraction of a second, and the variance the fleet saw was the
toolchain's, not the servers'.

That is the same discovery `rubric-v1.2` made about the missing-annotations class
and drew the right conclusion from: **a class that is near-universally satisfied
carries little information and should not command a fixed share of the score.**
The dual of that principle applies here, and it points at a weight change rather
than a measurement change. This release does not make one — see the
recommendations below — because rebalancing weights on the strength of a single
fleet run, without a per-dimension spread census to fit against, would repeat
exactly the error `rubric-v1.2` corrected in `rubric-v1.1`: asserting anchors
rather than calibrating them.

### Open recommendations (analysed, deliberately not implemented)

Two questions the fleet run raised that a scoring release should not answer
unilaterally.

**(a) Should the composite still be presented as multi-dimensional?**
Spearman(composite, −context tokens) = 0.959 says it is, today, close to a
token-count ranking. Three options, in ascending order of honesty and of cost:

1. **Rebalance.** Move weight from robustness and protocol toward the dimensions
   that discriminate. Cheap, but it is anchor-asserting without a spread census,
   and it would make `rubric-v1.4` scores incomparable with everything before.
2. **Report the spread.** Publish each dimension's fleet spread beside its score,
   so a reader can see that robustness separated nobody. Cheap, purely additive,
   and it makes the defect visible instead of arguable. **Recommended as the next
   step.**
3. **Stop calling it a quality grade.** Rename the composite to what it measures,
   or split it into a *cost* number and a *craft* number that are never averaged.
   Most honest, largest breaking change.

The prerequisite for (1) is the same missing dataset `rubric-v1.2` named twice:
`data/census-raw.json` records no per-dimension defect counts, so no weight can
be *fitted*. Extending the census to record per-dimension scores across a fleet
is the prerequisite for calibrating weights the way the cap anchors are
calibrated — and this fleet run is the first dataset that could seed it.

**(b) May a single dimension set the letter grade?** `dataforseo-mcp-server`
scores protocol **100**, schema hygiene **100**, description quality **100** — a
perfect card on every craft dimension — and grades **F 55**, solely because 89
tools cost 42,288 tokens and pin it to the context-cap floor.

Both halves of that report are individually defensible and together they are
incoherent. *"89 tools will wreck model selection accuracy"* is true, important,
and worth saying loudly. *"F"* contradicts the card printed directly beneath it,
and a reader who sees three 100s above an F concludes the instrument is broken —
which costs Jig the credibility it needs for the context finding to land at all.

The recommendation is **no: a single dimension should bound the composite but
should not be able to reach F alone.** Three supporting arguments:

- The cap exists to stop a heavy server *outranking* a light one on schema
  polish (`rubric-v1.1`, defect 2). Holding `dataforseo` to a **D 60–65** ceiling
  achieves that completely — it still ranks below every well-proportioned server
  — without the false statement.
- F is qualitatively different from D. Every other route to F in this rubric
  requires the server to be **broken**: stdout pollution, a `*/list` that never
  answers, a handshake that fails. A large-but-correct server is not in that
  class, and putting it there devalues the letter for the servers that earn it.
- The `rubric-v1.1` changelog already made this exact argument in the other
  direction, and was right then: *"Calling that server an F is not a defensible
  reading of the evidence; it is an artifact of the denominator."* It removed a
  manufactured F caused by tool count in the schema dimension, and then
  reintroduced one caused by tool count in the context dimension.

The concrete proposal is to raise the context-cap floor from **55 to 60** — the
D/F boundary — leaving the whole ramp and its census anchors untouched, and to
keep the cap line verbatim so the token count still leads the report. A server
would then reach F only by *combining* catastrophic context cost with genuine
defects elsewhere, which is a statement the card can support. This is a
single-constant change with a real effect on published grades, and it belongs in
its own release with its own monotonicity argument rather than bundled with three
measurement fixes.

### What did *not* change

- **Dimension weights.** Still 25 / 25 / 20 / 15 / 15. See recommendation (a).
- **Grade bands and badge colors.** Still `A >= 90 · B 80–89 · C 70–79 · D 60–69 · F < 60`.
- **The context-cost cap.** The `rubric-v1.2` ramp, its census anchors, and its
  floor of 55 are all untouched. See recommendation (b).
- **The protocol ceiling.** The `rubric-v1.3` ramp is untouched.
- **Injection scoring posture.** Still reported, never scored, always pinned.
- **The credential-UX verdict lattice.** Same four verdicts, same sub-scores.
- **Install timing.** Still reported, still never graded.
- **Measurement.** Context cost is still gpt-4o exact tokens over the canonical
  rendering. Rate-based scoring, shrinkage, class weights and the floor of 15 are
  untouched.
- **No LLM.** Every detector changed here is deterministic.

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
