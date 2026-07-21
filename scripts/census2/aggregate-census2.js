#!/usr/bin/env node
/*
 * aggregate-census2.js — turn data/census2-raw.json into the calibration
 * dataset: per-dimension score distributions and finding-class frequencies
 * across the fleet.
 *
 * Output (data/census2-calibration.json):
 *   - per dimension: sorted score samples, min/p25/median/p75/p90/max, mean, stddev
 *   - finding classes: how many servers exhibit each, total occurrences, points
 *   - composite and grade distribution
 *   - cap counts and the failure taxonomy for the unreachable tail
 *
 * ---------------------------------------------------------------------------
 * KNOWN WEAKNESS — finding-class keys are derived from message text
 * ---------------------------------------------------------------------------
 * `jig check --json` emits findings as {dimension, message, fix, severity,
 * points}. There is NO stable finding class code in that document. So a class
 * key here is *synthesised* by normalizing the human-readable message:
 * backticked identifiers -> <name>, digit runs -> <n>, lowercased, prefixed
 * with the dimension and severity.
 *
 * That means the finding-class table is only as stable as jig's wording:
 *   - rewording a message splits one class into two across jig versions;
 *   - two genuinely different checks that happen to phrase similarly merge;
 *   - a message that embeds an un-backticked, un-numeric variable (a tool name
 *     in plain text, a path) fragments into one class per server.
 * Treat `findingClasses` as an indicative frequency table, not as an identity
 * key. Cross-version comparison of these keys is NOT valid. The fix is a stable
 * class code in the check JSON; until jig emits one, this is the ceiling.
 *
 * The per-dimension score statistics do not have this problem — they read
 * numeric fields — and are the part of the dataset weight calibration uses.
 *
 * Zero dependencies beyond Node builtins.
 *
 * Usage:
 *   node scripts/census2/aggregate-census2.js [--raw <path>] [--out <path>]
 *   node scripts/census2/aggregate-census2.js [rawPath] [outPath]
 */

'use strict';

const fs = require('fs');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const key = a.slice(2);
      const val = argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[++i] : 'true';
      out[key] = val;
    } else {
      out._.push(a);
    }
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));

if (args.help === 'true' || args.h === 'true') {
  console.error(`aggregate-census2.js — build the census v2 calibration dataset.

  node scripts/census2/aggregate-census2.js [--raw <path>] [--out <path>]

  --raw <path>   run output   [data/census2-raw.json]
  --out <path>   calibration  [data/census2-calibration.json]
  --help         this message

Positional arguments are accepted as <raw> <out> for convenience.`);
  process.exit(0);
}

const RAW = path.resolve(
  args.raw || args._[0] || path.join(REPO_ROOT, 'data', 'census2-raw.json')
);
const OUT = path.resolve(
  args.out || args._[1] || path.join(REPO_ROOT, 'data', 'census2-calibration.json')
);

if (!fs.existsSync(RAW)) {
  console.error(
    `census2-aggregate: raw run output not found at ${RAW}\n` +
      `                   run it first: node scripts/census2/run-census2.js   (or pass --raw <path>)`
  );
  process.exit(1);
}

const raw = JSON.parse(fs.readFileSync(RAW, 'utf8'));
if (!Array.isArray(raw.results)) {
  console.error(`census2-aggregate: ${RAW}: expected a { results: [...] } document`);
  process.exit(1);
}
const checked = raw.results.filter((r) => r && r.checkOk && r.check);

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

function pct(sorted, p) {
  if (!sorted.length) return null;
  const idx = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  return lo === hi ? sorted[lo] : +(sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo)).toFixed(2);
}

function stats(xs) {
  const s = xs.filter(Number.isFinite).sort((a, b) => a - b);
  if (!s.length) return null;
  const mean = s.reduce((a, b) => a + b, 0) / s.length;
  const sd = Math.sqrt(s.reduce((a, b) => a + (b - mean) ** 2, 0) / s.length);
  return {
    n: s.length,
    min: s[0],
    p25: pct(s, 25),
    median: pct(s, 50),
    p75: pct(s, 75),
    p90: pct(s, 90),
    max: s[s.length - 1],
    mean: +mean.toFixed(2),
    stddev: +sd.toFixed(2),
    samples: s,
  };
}

// ---------------------------------------------------------------------------
// Per-dimension scores
// ---------------------------------------------------------------------------
// Shape (rubric-v1.4): dimensions[] = {dimension, label, score, scoreExact,
// weight, applicable, heuristic, summary, findings[]}. Scores from
// non-applicable dimensions are excluded from the calibration samples: a
// dimension that did not apply has no opinion about the server.

const dimScores = {}; // name -> [score]
const dimWeights = {}; // name -> weight as reported by the rubric that ran
for (const r of checked) {
  for (const d of r.check.dimensions || []) {
    const name = d.dimension || d.name || d.id;
    if (!name || d.applicable === false) continue;
    (dimScores[name] = dimScores[name] || []).push(d.scoreExact ?? d.score);
    if (d.weight !== undefined) dimWeights[name] = d.weight;
  }
}
const dimensions = {};
for (const [name, scores] of Object.entries(dimScores)) {
  dimensions[name] = { weight: dimWeights[name] ?? null, ...stats(scores) };
}

// ---------------------------------------------------------------------------
// Finding classes (see the KNOWN WEAKNESS note in the header)
// ---------------------------------------------------------------------------

function findingClassKey(dim, f) {
  const norm = String(f.message || '')
    .replace(/`[^`]*`/g, '<name>')
    .replace(/\d[\d,.]*/g, '<n>')
    .toLowerCase()
    .trim();
  return `${dim}: [${f.severity || '?'}] ${norm}`;
}

const findingServers = {}; // key -> Set(package)
const findingCounts = {}; // key -> total occurrences
const findingPoints = {}; // key -> total points deducted
for (const r of checked) {
  for (const d of r.check.dimensions || []) {
    for (const f of d.findings || []) {
      const key = findingClassKey(d.dimension, f);
      findingCounts[key] = (findingCounts[key] || 0) + 1;
      findingPoints[key] = (findingPoints[key] || 0) + (Number(f.points) || 0);
      (findingServers[key] = findingServers[key] || new Set()).add(r.package);
    }
  }
}
const findingClasses = Object.fromEntries(
  Object.entries(findingServers)
    .map(([k, set]) => [
      k,
      {
        servers: set.size,
        occurrences: findingCounts[k],
        serverShare: checked.length ? +(set.size / checked.length).toFixed(4) : null,
        totalPoints: +findingPoints[k].toFixed(2),
      },
    ])
    .sort((a, b) => b[1].servers - a[1].servers)
);

// ---------------------------------------------------------------------------
// Composite, grades, caps, failures
// ---------------------------------------------------------------------------

const composites = stats(checked.map((r) => r.check.composite));
const grades = {};
for (const r of checked) {
  const g = r.check.grade || '?';
  grades[g] = (grades[g] || 0) + 1;
}

const contextCapped = checked.filter((r) => r.check.contextCap).length;
const protocolCapped = checked.filter((r) => r.check.protocolCap).length;

const failures = {};
for (const r of raw.results.filter((x) => x && !x.checkOk)) {
  const reason = (r.failureReason || 'unknown').slice(0, 120);
  failures[reason] = (failures[reason] || 0) + 1;
}

const out = {
  _schema:
    'jig census2 calibration v0. Per-dimension score spreads, finding-class frequencies, ' +
    'composite/grade distribution across the checked fleet, and the failure taxonomy for the ' +
    'unreachable tail.',
  _findingClassCaveat:
    'Finding-class keys are synthesised by normalizing message text (backticked identifiers -> ' +
    '<name>, digit runs -> <n>) because `jig check --json` carries no stable finding class code. ' +
    'They are indicative frequencies only, and are NOT comparable across jig versions.',
  collected: raw.collected,
  jigBinary: raw.jigBinary,
  rubricVersion: checked.length ? checked[0].check.rubricVersion || null : null,
  counts: { ...raw.counts, contextCapped, protocolCapped },
  gradeDistribution: grades,
  composite: composites,
  dimensions,
  findingClasses,
  failureTaxonomy: Object.fromEntries(Object.entries(failures).sort((a, b) => b[1] - a[1])),
};

fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, JSON.stringify(out, null, 2) + '\n');

console.error('\n================ CENSUS2 CALIBRATION ================');
console.error(`checked servers:  ${checked.length} of ${raw.results.length} attempted`);
console.error(`rubric:           ${out.rubricVersion || 'unknown'}`);
console.error(`dimensions:       ${Object.keys(dimensions).length}`);
console.error(`finding classes:  ${Object.keys(findingClasses).length} (message-derived — see header)`);
console.error(`failure classes:  ${Object.keys(failures).length}`);
console.error(`out -> ${OUT}`);
console.error('====================================================');
