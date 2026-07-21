#!/usr/bin/env node
/*
 * run-census2.js — census v2: per-dimension `jig check` scores across the fleet.
 *
 * For every server in data/census2-servers.json:
 *
 *   jig check --stdio "npx -y <pkg> [args]" --json --no-report --timeout 45
 *
 * and records, honestly, what came back: the FULL check document (composite,
 * grade, per-dimension scores and weights, findings, caps) for every server that
 * completes, and a failure record with a reason for every one that does not.
 * Failures are never silently dropped — census v2's 64 failures out of 127 are
 * themselves a finding, and the aggregator keeps them as a taxonomy.
 *
 * This is the dataset rubric weight *fitting* requires: per-dimension spread
 * across a real fleet, not a hand-picked cohort.
 *
 * Notes on the mechanics:
 *   - Concurrency-bounded (default 4). npx cold starts dominate wall time.
 *   - Each server gets one retry; npx/network flake is common at this scale.
 *   - Progress is checkpointed to a JSONL sidecar after every server, and a
 *     re-run skips anything already recorded there. A 20-minute fleet run WILL
 *     get interrupted; resume is what makes the dataset collectible at all.
 *     Delete the sidecar to force a clean run.
 *   - `{TMPDIR}` in a server's args is replaced with a fresh temp directory
 *     (server-filesystem and friends require an allowed path).
 *   - `jig check` exits nonzero for a failing grade too, so "did we get a check
 *     document on stdout" is the reachability signal — not the exit code.
 *
 * Zero dependencies beyond Node builtins. Windows- and Linux-safe: jig is
 * invoked by explicit path and resolves the npx shim itself; the only shell-ish
 * concern (a temp path containing spaces) is handled by quoting.
 *
 * Usage:
 *   node scripts/census2/run-census2.js [--jig <path>] [--list <path>] [--out <path>]
 *                                       [--progress <path>] [--timeout <sec>]
 *                                       [--conc <n>] [--limit <n>] [--no-resume]
 *                                       [--no-report-flag false]
 *   env: JIG_BIN, CENSUS_TIMEOUT
 *
 *   --out defaults to data/census2-raw.json; the checkpoint sidecar defaults to
 *   <out>.progress.jsonl, so pointing --out outside the repo keeps the whole run
 *   outside the repo.
 */

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFile } = require('child_process');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

// ---------------------------------------------------------------------------
// Paths & args
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const key = a.slice(2);
      const val = argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[++i] : 'true';
      out[key] = val;
    }
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));

if (args.help === 'true' || args.h === 'true') {
  console.error(`run-census2.js — run \`jig check --json\` across the census v2 fleet.

  node scripts/census2/run-census2.js [options]

  --jig <path>        jig binary  [JIG_BIN, else target/release, else target/debug]
  --list <path>       fleet list  [data/census2-servers.json]
  --out <path>        raw output  [data/census2-raw.json]
  --progress <path>   checkpoint sidecar  [<out>.progress.jsonl]
  --timeout <sec>     jig's per-request timeout  [CENSUS_TIMEOUT, else 45]
  --conc <n>          concurrent servers  [4]
  --limit <n>         only the first n servers of the list
  --no-resume         ignore an existing checkpoint (does not delete it)
  --no-report-flag false   pass --report instead of --no-report
  --help              this message`);
  process.exit(0);
}

function defaultJigBin() {
  const exe = process.platform === 'win32' ? 'jig.exe' : 'jig';
  const candidates = [
    process.env.JIG_BIN,
    path.join(REPO_ROOT, 'target', 'release', exe),
    path.join(REPO_ROOT, 'target', 'debug', exe),
  ].filter(Boolean);
  for (const c of candidates) {
    if (fs.existsSync(c)) return c;
  }
  // Fall back to the release path even if missing, so the error message is clear.
  return candidates[candidates.length - 1];
}

const JIG_BIN = args.jig ? path.resolve(args.jig) : defaultJigBin();
const LIST_PATH = args.list
  ? path.resolve(args.list)
  : path.join(REPO_ROOT, 'data', 'census2-servers.json');
const OUT = args.out ? path.resolve(args.out) : path.join(REPO_ROOT, 'data', 'census2-raw.json');
const PROGRESS = args.progress
  ? path.resolve(args.progress)
  : OUT.replace(/\.json$/i, '') + '.progress.jsonl';

// jig's own per-request timeout (seconds).
const JIG_TIMEOUT = Number(args.timeout || process.env.CENSUS_TIMEOUT || 45);
// Hard wall for a single jig invocation (ms). Larger than JIG_TIMEOUT so npx
// cold-start install plus jig's own timeout can fire cleanly first.
const WALL_MS = (JIG_TIMEOUT + 60) * 1000;
const CONC = Math.max(1, Number(args.conc || 4));
const LIMIT = args.limit ? Number(args.limit) : Infinity;
const USE_NO_REPORT = args['no-report-flag'] !== 'false';
const RESUME = args['no-resume'] !== 'true';
// Attempts per server (the whole server is retried once if the first fails).
const MAX_ATTEMPTS = 2;

if (!fs.existsSync(JIG_BIN)) {
  console.error(
    `census2: jig binary not found at ${JIG_BIN}\n` +
      `         build it first: cargo build -p jig-cli --release   (or pass --jig <path> / JIG_BIN)`
  );
  process.exit(1);
}
if (!fs.existsSync(LIST_PATH)) {
  console.error(
    `census2: fleet list not found at ${LIST_PATH}\n` +
      `         build it first: node scripts/census2/build-candidates.js   (or pass --list <path>)`
  );
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Server list
// ---------------------------------------------------------------------------

function loadServers() {
  const doc = JSON.parse(fs.readFileSync(LIST_PATH, 'utf8'));
  const list = Array.isArray(doc) ? doc : doc.servers;
  if (!Array.isArray(list)) {
    throw new Error(`${LIST_PATH}: expected an array or a { servers: [...] } object`);
  }
  return list;
}

// Substitute the {TMPDIR} placeholder with a freshly-created temp directory.
function materializeArgs(rawArgs) {
  return (rawArgs || []).map((a) =>
    a.includes('{TMPDIR}')
      ? a.replace('{TMPDIR}', fs.mkdtempSync(path.join(os.tmpdir(), 'jig-census2-')))
      : a
  );
}

// Build the single --stdio command string jig expects. jig's own splitter
// understands double-quoted segments, so quote any token containing whitespace.
function buildStdioCommand(pkg, resolvedArgs) {
  const quote = (t) => (/\s/.test(t) ? `"${t}"` : t);
  return ['npx', '-y', pkg, ...resolvedArgs].map(quote).join(' ');
}

// ---------------------------------------------------------------------------
// Running jig
// ---------------------------------------------------------------------------

function runCheck(stdioCmd) {
  return new Promise((resolve) => {
    const argv = ['check', '--stdio', stdioCmd, '--json', '--timeout', String(JIG_TIMEOUT)];
    if (USE_NO_REPORT) argv.push('--no-report');
    execFile(
      JIG_BIN,
      argv,
      { encoding: 'utf8', timeout: WALL_MS, maxBuffer: 96 * 1024 * 1024, windowsHide: true },
      (err, stdout, stderr) => {
        resolve({
          status: err && typeof err.code === 'number' ? err.code : err ? -1 : 0,
          killedByWall: !!(err && (err.killed || err.signal)),
          stdout: stdout || '',
          stderr: stderr || '',
        });
      }
    );
  });
}

function firstStderrLine(stderr) {
  const lines = (stderr || '')
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean);
  return lines.find((s) => s.startsWith('jig:')) || lines[0] || '';
}

async function checkOne(entry, tag) {
  const resolvedArgs = materializeArgs(entry.args);
  const stdioCmd = buildStdioCommand(entry.package, resolvedArgs);
  const base = {
    package: entry.package,
    args: resolvedArgs,
    note: entry.note || '',
    source: entry.source || '',
    stdioCommand: stdioCmd,
  };

  let last = null;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    const started = Date.now();
    const r = await runCheck(stdioCmd);
    const durationMs = Date.now() - started;

    if (r.killedByWall) {
      last = {
        ...base,
        checkOk: false,
        attempt,
        durationMs,
        failureReason: `wall timeout ${WALL_MS / 1000}s`,
      };
      continue;
    }

    let doc = null;
    if (r.stdout.trim()) {
      try {
        doc = JSON.parse(r.stdout);
      } catch {
        doc = null;
      }
    }
    if (doc && (doc.composite !== undefined || doc.dimensions !== undefined)) {
      console.error(
        `${tag} ok  composite=${doc.composite} grade=${doc.grade || '?'} (${(durationMs / 1000).toFixed(0)}s)`
      );
      return { ...base, checkOk: true, attempt, durationMs, exitStatus: r.status, check: doc };
    }

    last = {
      ...base,
      checkOk: false,
      attempt,
      durationMs,
      exitStatus: r.status,
      failureReason: firstStderrLine(r.stderr) || `check exited ${r.status} with no JSON`,
    };
  }
  console.error(`${tag} FAIL ${last.failureReason}`);
  return last;
}

// ---------------------------------------------------------------------------
// Checkpointing
// ---------------------------------------------------------------------------

function loadCheckpoint() {
  const done = new Map();
  if (!RESUME || !fs.existsSync(PROGRESS)) return done;
  for (const line of fs.readFileSync(PROGRESS, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    try {
      const rec = JSON.parse(line);
      if (rec.package) done.set(rec.package, rec);
    } catch {
      /* torn tail line from a killed run — ignore */
    }
  }
  return done;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const servers = loadServers().slice(0, LIMIT);
  const startedIso = new Date().toISOString();
  const wallStart = Date.now();
  const done = loadCheckpoint();

  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.mkdirSync(path.dirname(PROGRESS), { recursive: true });

  console.error(
    `census2: ${servers.length} servers (${done.size} already checkpointed), conc=${CONC}, ` +
      `jig=${JIG_BIN}, timeout=${JIG_TIMEOUT}s`
  );

  const results = new Array(servers.length);
  let next = 0;
  async function worker() {
    while (next < servers.length) {
      const i = next++;
      const entry = servers[i];
      const tag = `[${i + 1}/${servers.length}] ${entry.package}`;
      if (done.has(entry.package)) {
        results[i] = done.get(entry.package);
        continue;
      }
      try {
        results[i] = await checkOne(entry, tag);
      } catch (e) {
        results[i] = {
          package: entry.package,
          args: entry.args || [],
          source: entry.source || '',
          checkOk: false,
          failureReason: `census2 harness error: ${String((e && e.message) || e)}`,
        };
        console.error(`${tag} HARNESS ERROR ${results[i].failureReason}`);
      }
      fs.appendFileSync(PROGRESS, JSON.stringify(results[i]) + '\n');
    }
  }
  await Promise.all(Array.from({ length: CONC }, worker));

  const wallSeconds = Math.round((Date.now() - wallStart) / 1000);
  const ok = results.filter((r) => r && r.checkOk);
  const failed = results.filter((r) => r && !r.checkOk);

  const raw = {
    _schema:
      'jig census2 raw v0. Full `jig check --json` documents per reachable server; honest failure ' +
      'records otherwise. Purpose: per-dimension spread for rubric weight calibration.',
    collected: new Date().toISOString(),
    started: startedIso,
    wallSeconds,
    jigBinary: path.basename(JIG_BIN),
    platform: `${process.platform} ${process.arch}`,
    node: process.version,
    jigTimeoutSec: JIG_TIMEOUT,
    concurrency: CONC,
    counts: { attempted: results.length, checked: ok.length, failed: failed.length },
    results,
  };
  fs.writeFileSync(OUT, JSON.stringify(raw, null, 2) + '\n');

  // Console summary (stderr so stdout stays clean if ever piped).
  console.error('\n=================== CENSUS2 SUMMARY ===================');
  console.error(`attempted:   ${results.length}`);
  console.error(`checked:     ${ok.length}`);
  console.error(`failed:      ${failed.length}`);
  console.error(`wall time:   ${wallSeconds}s (${(wallSeconds / 60).toFixed(1)} min)`);
  if (failed.length) {
    const byReason = {};
    for (const r of failed) {
      const key = (r.failureReason || 'unknown').slice(0, 60);
      byReason[key] = (byReason[key] || 0) + 1;
    }
    const top = Object.entries(byReason)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 3);
    for (const [reason, n] of top) console.error(`  x${n}  ${reason}`);
  }
  console.error(`raw       -> ${OUT}`);
  console.error(`checkpoint-> ${PROGRESS}`);
  console.error('=======================================================');
}

main().catch((e) => {
  console.error(`census2: ${(e && e.stack) || e}`);
  process.exit(1);
});
