#!/usr/bin/env node
/*
 * build-candidates.js — assemble the census v2 candidate fleet.
 *
 * Census v2 measures `jig check` scores per dimension across a wide fleet. This
 * script builds the fleet list it runs against:
 *
 *   data/census-servers.json  (the curated v1 50 — kept verbatim, args intact)
 * + npm discovery pools       (raw npm registry search dumps, NOT committed)
 * = data/census2-servers.json
 *
 * Every pool package not already in the v1 list is screened against the npm
 * registry and kept only if:
 *   - it has a `bin` entry on its latest version (i.e. `npx -y <pkg>` runs it)
 *   - that version was published within ~14 months of --asof (actively published)
 *   - name/keywords/description mention MCP (drops generic search hits)
 *   - it does not match obvious non-server patterns (sdk / client library /
 *     framework / inspector / template / starter / generator / create-*)
 *
 * There is deliberately NO reachability screening: a server that refuses to
 * start is census data, not an exclusion.
 *
 * ---------------------------------------------------------------------------
 * The discovery pools (why they are not in the repo, and how to regenerate)
 * ---------------------------------------------------------------------------
 * The Jul-2026 run used two raw npm registry search responses, ~210 KB of JSON
 * that is 95% metadata this script never reads. They are not committed. Both
 * are plain GETs against the public npm search API:
 *
 *   pool A  https://registry.npmjs.org/-/v1/search?text=keywords:mcp%20server&size=100
 *           (captured 2026-07-19; 100 objects of 60,982 matches)
 *   pool B  https://registry.npmjs.org/-/v1/search?text=keywords:modelcontextprotocol&size=80
 *           (captured 2026-07-19; 80 objects of 2,081 matches)
 *
 * `--fetch-pools <dir>` writes exactly those two files for you. npm's search
 * ranking is not stable over time, so a fresh fetch will not reproduce the
 * Jul-2026 pool member-for-member; the committed data/census2-servers.json is
 * the record of what was actually screened.
 *
 * Zero dependencies beyond Node builtins. Cross-platform.
 *
 * Usage:
 *   node scripts/census2/build-candidates.js [--pool-dir <dir>] [--pools <a.json,b.json>]
 *                                            [--v1 <path>] [--out <path>]
 *                                            [--asof <iso8601>] [--conc <n>]
 *   node scripts/census2/build-candidates.js --fetch-pools <dir>
 *   node scripts/census2/build-candidates.js --help
 *
 *   --pool-dir  directory of npm search dumps; every *.json in it is a pool.
 *               Default: $CENSUS2_POOL_DIR, else <repo>/data/pools.
 *   --pools     explicit pool files (comma-separated, or repeat the flag).
 *   --asof      screening clock for the ~14-month recency window (default: now).
 *               Pass 2026-07-19 to re-screen against the original cutoff.
 */

'use strict';

const fs = require('fs');
const path = require('path');
const https = require('https');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) continue;
    const key = a.slice(2);
    const val = argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[++i] : 'true';
    // Repeated flags accumulate so `--pools x --pools y` works.
    if (out[key] === undefined) out[key] = val;
    else out[key] = [].concat(out[key], val);
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));

const HELP = `build-candidates.js — assemble the census v2 candidate fleet.

  node scripts/census2/build-candidates.js [options]

  --pool-dir <dir>     directory of npm search dumps (*.json). Default:
                       $CENSUS2_POOL_DIR, else <repo>/data/pools
  --pools <a,b>        explicit pool files (comma-separated; flag repeatable)
  --v1 <path>          census v1 server list  [data/census-servers.json]
  --out <path>         output fleet list      [data/census2-servers.json]
  --asof <iso8601>     clock for the ~14-month recency screen  [now]
  --conc <n>           registry fetch concurrency  [8]
  --fetch-pools <dir>  fetch the two canonical npm discovery pools into <dir>
                       and exit (see the header for the exact queries)
  --help               this message

The pools are large raw npm registry search dumps and are NOT committed to the
repo. Fetch them with --fetch-pools, or point --pool-dir at your own copies.`;

if (args.help === 'true' || args.h === 'true') {
  console.error(HELP);
  process.exit(0);
}

const V1_LIST = args.v1
  ? path.resolve(args.v1)
  : path.join(REPO_ROOT, 'data', 'census-servers.json');
const OUT = args.out
  ? path.resolve(args.out)
  : path.join(REPO_ROOT, 'data', 'census2-servers.json');
const POOL_DIR = args['pool-dir']
  ? path.resolve(args['pool-dir'])
  : process.env.CENSUS2_POOL_DIR
    ? path.resolve(process.env.CENSUS2_POOL_DIR)
    : path.join(REPO_ROOT, 'data', 'pools');
const CONC = Number(args.conc || 8);
const ASOF = args.asof && args.asof !== 'true' ? Date.parse(args.asof) : Date.now();
if (!Number.isFinite(ASOF)) {
  console.error(`build-candidates: --asof: not a parseable date: ${args.asof}`);
  process.exit(1);
}

// Screening constants. Changing either of these changes the published dataset's
// selection criteria — census v2 was built with exactly these.
const NON_SERVER_RE =
  /\b(sdk|client library|framework for building|inspector|boilerplate|template|starter|scaffold|create-|generator)\b/i;
const RECENCY_MS = 14 * 30 * 24 * 3600 * 1000;
const CUTOFF_MS = ASOF - RECENCY_MS;

const POOL_QUERIES = [
  { file: 'npm-mcp-search.json', text: 'keywords:mcp server', size: 100 },
  { file: 'npm-mcp2.json', text: 'keywords:modelcontextprotocol', size: 80 },
];

// ---------------------------------------------------------------------------
// npm registry
// ---------------------------------------------------------------------------

function get(url) {
  return new Promise((resolve) => {
    https
      .get(url, { headers: { accept: 'application/json' } }, (res) => {
        let body = '';
        res.on('data', (c) => (body += c));
        res.on('end', () => resolve({ status: res.statusCode, body }));
      })
      .on('error', (e) => resolve({ status: 0, body: String((e && e.message) || e) }));
  });
}

async function registryDoc(pkg) {
  // Scoped names must keep their leading @ but have the / encoded.
  const { status, body } = await get(
    `https://registry.npmjs.org/${encodeURIComponent(pkg).replace('%40', '@')}`
  );
  if (status !== 200) return null;
  try {
    return JSON.parse(body);
  } catch {
    return null;
  }
}

async function fetchPools(dir) {
  fs.mkdirSync(dir, { recursive: true });
  for (const q of POOL_QUERIES) {
    const url = `https://registry.npmjs.org/-/v1/search?text=${encodeURIComponent(q.text)}&size=${q.size}`;
    const { status, body } = await get(url);
    if (status !== 200) {
      console.error(`build-candidates: pool fetch failed (${status}) for ${url}`);
      process.exit(1);
    }
    let doc;
    try {
      doc = JSON.parse(body);
    } catch {
      console.error(`build-candidates: pool fetch returned non-JSON for ${url}`);
      process.exit(1);
    }
    const dest = path.join(dir, q.file);
    fs.writeFileSync(dest, JSON.stringify(doc, null, 2) + '\n');
    console.error(
      `pool ${q.file}: ${(doc.objects || []).length} objects of ${doc.total} matches -> ${dest}`
    );
  }
  console.error(
    'npm search ranking drifts; a fresh fetch will not reproduce the Jul-2026 pool exactly.'
  );
}

// ---------------------------------------------------------------------------
// Pool loading
// ---------------------------------------------------------------------------

function resolvePoolFiles() {
  if (args.pools) {
    return []
      .concat(args.pools)
      .flatMap((s) => String(s).split(','))
      .map((s) => s.trim())
      .filter(Boolean)
      .map((p) => path.resolve(p));
  }
  if (!fs.existsSync(POOL_DIR)) return [];
  return fs
    .readdirSync(POOL_DIR)
    .filter((f) => f.toLowerCase().endsWith('.json'))
    .sort()
    .map((f) => path.join(POOL_DIR, f));
}

function poolsMissing(reason) {
  console.error(
    `build-candidates: ${reason}\n` +
      `\n` +
      `  The npm discovery pools are raw npm registry search dumps and are not\n` +
      `  committed to this repo (they are large and 95% metadata we never read).\n` +
      `\n` +
      `  Fetch them:   node scripts/census2/build-candidates.js --fetch-pools ${path.join('data', 'pools')}\n` +
      `  Or point at your own copies:\n` +
      `                node scripts/census2/build-candidates.js --pool-dir <dir>\n` +
      `                node scripts/census2/build-candidates.js --pools a.json,b.json\n` +
      `\n` +
      `  A pool file is the verbatim JSON body of:\n` +
      POOL_QUERIES.map(
        (q) =>
          `    https://registry.npmjs.org/-/v1/search?text=${encodeURIComponent(q.text)}&size=${q.size}\n` +
          `      -> ${q.file}`
      ).join('\n') +
      `\n\n  See scripts/census2/README.md.`
  );
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  if (args['fetch-pools']) {
    const dir = args['fetch-pools'] === 'true' ? POOL_DIR : path.resolve(args['fetch-pools']);
    await fetchPools(dir);
    return;
  }

  if (!fs.existsSync(V1_LIST)) {
    console.error(`build-candidates: census v1 list not found at ${V1_LIST} (pass --v1 <path>)`);
    process.exit(1);
  }

  const poolFiles = resolvePoolFiles();
  if (poolFiles.length === 0) {
    poolsMissing(`no discovery pool files found (looked in ${POOL_DIR})`);
  }
  const unreadable = poolFiles.filter((f) => !fs.existsSync(f));
  if (unreadable.length) {
    poolsMissing(`pool file(s) not found: ${unreadable.join(', ')}`);
  }

  const v1 = JSON.parse(fs.readFileSync(V1_LIST, 'utf8'));
  const v1Servers = Array.isArray(v1) ? v1 : v1.servers;
  const v1Names = new Set(v1Servers.map((s) => s.package));

  const pool = new Map(); // name -> { desc, keywords, source }
  for (const file of poolFiles) {
    let doc;
    try {
      doc = JSON.parse(fs.readFileSync(file, 'utf8'));
    } catch (e) {
      console.error(`build-candidates: ${file}: not valid JSON (${(e && e.message) || e})`);
      process.exit(1);
    }
    if (!Array.isArray(doc.objects)) {
      console.error(
        `build-candidates: ${file}: expected an npm search response with an "objects" array`
      );
      process.exit(1);
    }
    for (const o of doc.objects) {
      const p = o.package || {};
      if (!p.name || pool.has(p.name) || v1Names.has(p.name)) continue;
      pool.set(p.name, {
        desc: p.description || '',
        keywords: (p.keywords || []).join(' '),
        source: `npm-search:${path.basename(file)}`,
      });
    }
  }

  console.error(
    `pool: ${pool.size} unique non-v1 candidates from ${poolFiles.length} file(s); screening against the registry...`
  );

  const accepted = [];
  const rejected = { noBin: 0, stale: 0, notMcp: 0, nonServer: 0, fetchFail: 0 };
  const names = [...pool.keys()];
  let idx = 0;
  async function worker() {
    while (idx < names.length) {
      const name = names[idx++];
      const meta = pool.get(name);
      const blob = `${name} ${meta.desc} ${meta.keywords}`;
      if (!/mcp|model.?context.?protocol/i.test(blob)) {
        rejected.notMcp++;
        continue;
      }
      if (NON_SERVER_RE.test(blob)) {
        rejected.nonServer++;
        continue;
      }
      const doc = await registryDoc(name);
      if (!doc) {
        rejected.fetchFail++;
        continue;
      }
      const latest = doc['dist-tags'] && doc['dist-tags'].latest;
      const ver = latest && doc.versions && doc.versions[latest];
      if (!ver || !ver.bin || Object.keys(ver.bin).length === 0) {
        rejected.noBin++;
        continue;
      }
      const pubTime = doc.time && doc.time[latest] ? Date.parse(doc.time[latest]) : 0;
      if (pubTime < CUTOFF_MS) {
        rejected.stale++;
        continue;
      }
      accepted.push({
        package: name,
        args: [],
        source: meta.source,
        note: meta.desc.slice(0, 140),
      });
    }
  }
  await Promise.all(Array.from({ length: Math.max(1, CONC) }, worker));

  const servers = [
    ...v1Servers.map((s) => ({ ...s, source: s.source || 'census-v1' })),
    ...accepted.sort((a, b) => a.package.localeCompare(b.package)),
  ];

  const out = {
    _selection: {
      purpose:
        'Census v2 fleet: the curated v1 50 plus every actively-published, npx-runnable, MCP-flavored package from the Jul-2026 npm discovery pools. Purpose: per-dimension `jig check` scores across a wide fleet, the dataset rubric weight calibration requires.',
      screened: {
        poolCandidates: pool.size,
        accepted: accepted.length,
        rejected,
      },
      criteria: [
        'v1 servers kept verbatim with their curated args ({TMPDIR} placeholders intact).',
        'Pool additions: registry doc has a bin entry (npx-runnable), latest publish within ~14 months, MCP-related name/keywords/description, and not matching obvious non-server patterns (sdk/inspector/template/etc.).',
        'No reachability screening: startup failures are census data, not exclusions.',
      ],
      asof: new Date(ASOF).toISOString(),
      builtFrom: [path.relative(REPO_ROOT, V1_LIST).replace(/\\/g, '/')].concat(
        poolFiles.map((f) => path.basename(f))
      ),
    },
    servers,
  };
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(out, null, 2) + '\n');

  console.error('\n================ CANDIDATE SCREENING ================');
  console.error(`pool candidates: ${pool.size}`);
  console.error(`accepted:        ${accepted.length}`);
  console.error(
    `rejected:        ${Object.entries(rejected)
      .map(([k, v]) => `${k}=${v}`)
      .join(' ')}`
  );
  console.error(`v1 carried over: ${v1Servers.length}`);
  console.error(`fleet total:     ${servers.length}`);
  console.error(`out -> ${OUT}`);
  console.error('=====================================================');
}

main().catch((e) => {
  console.error(`build-candidates: ${(e && e.stack) || e}`);
  process.exit(1);
});
