#!/usr/bin/env node
/*
 * build-leaderboard.js — the ecosystem leaderboard, from N repeated fleet runs.
 *
 * Publishing a per-vendor grade off ONE run is not defensible: robustness is
 * measured live (server boot time) and drifts with machine load, by a median of
 * ~8 points across the fleet. This script therefore takes several complete runs
 * and publishes, per server:
 *
 *   grade   — from the MEDIAN composite across runs
 *   range   — min..max composite observed, printed whenever it is not zero
 *   runs    — how many runs the server was reachable in
 *
 * A server that is not reachable in every run is reported as such rather than
 * quietly averaged over its good days.
 *
 * Usage: node build-leaderboard.js --runs a.json b.json c.json --out index.html
 */
'use strict';
const fs = require('fs');

function parseArgs(argv) {
  const out = { runs: [] };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--runs') {
      while (argv[i + 1] && !argv[i + 1].startsWith('--')) out.runs.push(argv[++i]);
    } else if (argv[i].startsWith('--')) {
      out[argv[i].slice(2)] = argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[++i] : 'true';
    }
  }
  return out;
}
const args = parseArgs(process.argv.slice(2));
if (!args.runs.length) {
  console.error('usage: build-leaderboard.js --runs <raw.json...> --out <index.html>');
  process.exit(1);
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
const grade = (x) => {
  const r = Math.round(x);
  return r >= 90 ? 'A' : r >= 80 ? 'B' : r >= 70 ? 'C' : r >= 60 ? 'D' : 'F';
};
const esc = (s) =>
  String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

// --- load runs --------------------------------------------------------------
const runs = args.runs.map((p) => JSON.parse(fs.readFileSync(p, 'utf8')));
const runCount = runs.length;
const byPkg = new Map();
let attempted = 0;
for (const run of runs) {
  attempted = Math.max(attempted, run.counts.attempted);
  for (const r of run.results) {
    if (!r || !r.package) continue;
    const e = byPkg.get(r.package) || { pkg: r.package, composites: [], dims: null, fails: 0, reason: '' };
    if (r.checkOk && r.check) {
      e.composites.push(r.check.composite);
      e.dims = r.check.dimensions;
      e.tools = r.check.toolCount;
      e.rubric = r.check.rubricVersion;
      e.capped = !!(r.check.contextCap || r.check.protocolCap);
    } else {
      e.fails++;
      e.reason = e.reason || (r.failureReason || '').replace(/^jig: (error|warning): /, '');
    }
    byPkg.set(r.package, e);
  }
}

const graded = [];
const unreachable = [];
for (const e of byPkg.values()) {
  if (e.composites.length === 0) {
    unreachable.push(e);
    continue;
  }
  const med = median(e.composites);
  graded.push({
    ...e,
    median: med,
    grade: grade(med),
    min: Math.min(...e.composites),
    max: Math.max(...e.composites),
    runsOk: e.composites.length,
  });
}
graded.sort((a, b) => b.median - a.median || a.pkg.localeCompare(b.pkg));

const dimScore = (e, name) => {
  const d = (e.dims || []).find((x) => x.dimension === name);
  return d && d.applicable !== false ? Math.round(d.score) : null;
};

const gradeCounts = {};
for (const g of graded) gradeCounts[g.grade] = (gradeCounts[g.grade] || 0) + 1;
const spreads = graded.map((g) => g.max - g.min);
const worstSpread = Math.max(...spreads);
const medSpread = median(spreads);
const collected = runs[runs.length - 1].collected;
const rubric = graded[0] ? graded[0].rubric : 'rubric-v1.5';

const rows = graded
  .map((g, i) => {
    const range =
      g.max - g.min > 0
        ? `<span class="rng">${g.min}–${g.max}</span>`
        : `<span class="rng stable">exact</span>`;
    const partial = g.runsOk < runCount ? `<span class="warn" title="reachable in only ${g.runsOk} of ${runCount} runs">${g.runsOk}/${runCount}</span>` : '';
    return `<tr>
  <td class="rank">${i + 1}</td>
  <td class="pkg"><code>${esc(g.pkg)}</code> ${partial}</td>
  <td class="grade g${g.grade}">${g.grade}</td>
  <td class="score">${g.median}${range}</td>
  <td class="d">${dimScore(g, 'protocol') ?? '—'}</td>
  <td class="d">${dimScore(g, 'context_cost') ?? '—'}</td>
  <td class="d">${dimScore(g, 'schema_hygiene') ?? '—'}</td>
  <td class="d">${dimScore(g, 'description_quality') ?? '—'}</td>
  <td class="d rob">${dimScore(g, 'robustness') ?? '—'}</td>
  <td class="t">${g.tools ?? '—'}</td>
</tr>`;
  })
  .join('\n');

const failRows = unreachable
  .sort((a, b) => a.pkg.localeCompare(b.pkg))
  .map((e) => `<tr><td class="pkg"><code>${esc(e.pkg)}</code></td><td class="why">${esc(e.reason || 'did not complete the handshake')}</td></tr>`)
  .join('\n');

const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>MCP server leaderboard — jig</title>
<meta name="description" content="${graded.length} public MCP servers graded on protocol, context cost, schema hygiene, description quality and robustness. Median of ${runCount} runs, ${rubric}.">
<style>
:root{--bg:#0b0c0e;--fg:#e8e6e3;--dim:#8b8b8b;--line:#23262b;--acc:#7dd3a0;--warn:#e0b050;--bad:#e07070}
@media (prefers-color-scheme:light){:root{--bg:#fbfbfa;--fg:#16181d;--dim:#666;--line:#e2e2e0;--acc:#1a7f4e;--warn:#8a6100;--bad:#a33}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.6 ui-sans-serif,-apple-system,'Segoe UI',system-ui,sans-serif;-webkit-font-smoothing:antialiased}
.wrap{max-width:1080px;margin:0 auto;padding:48px 20px 96px}
h1{font-size:clamp(28px,5vw,42px);letter-spacing:-.02em;margin:0 0 6px}
.sub{color:var(--dim);margin:0 0 28px;max-width:70ch}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.92em}
a{color:inherit}
.meta{display:flex;flex-wrap:wrap;gap:10px 22px;padding:14px 16px;border:1px solid var(--line);border-radius:8px;margin:0 0 26px;font-size:13.5px;color:var(--dim)}
.meta b{color:var(--fg);font-weight:600}
.method{border-left:3px solid var(--acc);padding:2px 0 2px 16px;margin:0 0 30px;max-width:78ch}
.method h2{font-size:16px;margin:0 0 8px}
.method p{margin:0 0 10px;color:var(--dim);font-size:14px}
.tablewrap{overflow-x:auto;border:1px solid var(--line);border-radius:8px}
table{border-collapse:collapse;width:100%;font-size:14px;min-width:820px}
th,td{padding:9px 10px;text-align:left;border-bottom:1px solid var(--line);white-space:nowrap}
th{font-size:11.5px;text-transform:uppercase;letter-spacing:.06em;color:var(--dim);font-weight:600;position:sticky;top:0;background:var(--bg)}
tbody tr:last-child td{border-bottom:0}
.rank{color:var(--dim);width:36px;text-align:right}
.pkg{white-space:normal}
.grade{font-weight:700;width:34px;text-align:center}
.gA{color:var(--acc)}.gB{color:var(--fg)}.gC{color:var(--warn)}.gD{color:var(--warn)}.gF{color:var(--bad)}
.score{font-variant-numeric:tabular-nums;width:110px}
.rng{color:var(--dim);font-size:12px;margin-left:7px}
.rng.stable{opacity:.45}
.d,.t{font-variant-numeric:tabular-nums;color:var(--dim);width:56px;text-align:right}
.rob{font-style:italic}
.warn{color:var(--warn);font-size:11.5px;border:1px solid currentColor;border-radius:4px;padding:0 4px;margin-left:6px}
h2.sec{font-size:19px;margin:44px 0 10px}
.note{color:var(--dim);font-size:13.5px;max-width:78ch}
footer{margin-top:52px;padding-top:18px;border-top:1px solid var(--line);color:var(--dim);font-size:13px}
</style>
</head>
<body>
<div class="wrap">
<h1>MCP server leaderboard</h1>
<p class="sub">${graded.length} publicly installable Model Context Protocol servers, graded by <a href="https://github.com/Shodh-Labs/jig"><code>jig check</code></a> on five dimensions. No server was contacted for permission and none was asked to opt in — these are public packages, measured with a public rubric, and every number here is reproducible from the repository.</p>

<div class="meta">
  <span><b>${attempted}</b> servers attempted</span>
  <span><b>${graded.length}</b> graded</span>
  <span><b>${unreachable.length}</b> never started</span>
  <span><b>${runCount}</b> full runs, median reported</span>
  <span><b>${rubric}</b></span>
  <span>collected <b>${esc((collected || '').slice(0, 10))}</b></span>
</div>

<div class="method">
<h2>Read this before you read the table</h2>
<p><b>The grade is a median of ${runCount} complete runs, not a single measurement.</b> Four of the five dimensions are computed from a server's tool surface and return identical scores every time. Robustness is not: it times how long a server takes to answer <code>initialize</code>, so it registers machine load as if it were a property of the server. Across this fleet a single run can move a robustness score by 8 points and a letter grade by one step.</p>
<p><b>The range column is the honest part.</b> It shows the lowest and highest composite we actually observed for that server. Median spread across the fleet was <b>${medSpread}</b> points; the widest was <b>${worstSpread}</b>. A server whose range straddles a grade boundary should be read as sitting on that boundary, not as owning the better letter. Robustness is printed in italics for the same reason.</p>
<p><b>Context cost is what a server advertises, not a bill every user pays.</b> It is the token size of the tool surface the server offers on connect. A client that sends the whole surface pays it on every turn; clients that defer tool schemas and load them on demand &mdash; Claude Code and Codex both do this now &mdash; do not. Read it as the advertised size and the worst case. It still matters on non-deferring clients, and tool-selection accuracy degrades as the menu grows either way. Note also that jig grades the <em>default</em> invocation: if a server offers presets, an enabled-tools filter or a discovery mode, this number does not credit it &mdash; a gap in jig, not in the server.</p>
<p><b>An F means the server does not work</b> — it pollutes stdout, or never answers a required call. Since <code>${rubric}</code>, size alone cannot produce an F: a large-but-correct server is floored at D. That distinction is deliberate and is documented in the <a href="https://github.com/Shodh-Labs/jig/blob/main/docs/rubric-changelog.md">rubric changelog</a>.</p>
<p><b>If you maintain one of these servers and think the number is wrong, it is falsifiable in one command:</b> <code>npx @shodh/jig check --stdio "npx -y &lt;your-package&gt;"</code>. If it disagrees with this table, that is a bug worth filing against jig, and corrections get published as loudly as findings.</p>
</div>

<div class="tablewrap">
<table>
<thead><tr>
<th></th><th>Server</th><th>Grade</th><th>Composite</th>
<th title="Protocol compliance">Prot</th><th title="Context cost">Ctx</th><th title="Schema hygiene">Schema</th><th title="Description quality">Desc</th><th title="Robustness — live-measured, varies between runs">Robust</th><th title="Tool count">Tools</th>
</tr></thead>
<tbody>
${rows}
</tbody>
</table>
</div>

<h2 class="sec">Never started (${unreachable.length})</h2>
<p class="note">Installable is not the same as runnable. These packages install from npm without complaint and then fail to complete an MCP handshake — most of them because they exit demanding a credential. They are listed because a census that silently drops its failures lies about the ecosystem, not to shame anyone: refusing to start without a key is a defensible design, it is just one a user discovers the hard way.</p>
<div class="tablewrap">
<table>
<thead><tr><th>Server</th><th>What happened</th></tr></thead>
<tbody>
${failRows}
</tbody>
</table>
</div>

<footer>
Generated by <code>scripts/leaderboard/build-leaderboard.js</code> from ${runCount} full fleet runs on one machine.
Raw per-server documents, the fleet list, and the method note are in the
<a href="https://github.com/Shodh-Labs/jig">jig repository</a>.
This table describes the servers as measured on ${esc((collected || '').slice(0, 10))}; packages change, and it will age.
</footer>
</div>
</body>
</html>
`;

const out = args.out || 'index.html';
fs.writeFileSync(out, html);
console.error(
  `leaderboard: ${graded.length} graded (${Object.entries(gradeCounts).sort().map(([g, n]) => g + n).join(' ')}), ` +
    `${unreachable.length} unreachable, ${runCount} runs, median spread ${medSpread}, worst ${worstSpread} -> ${out}`
);
