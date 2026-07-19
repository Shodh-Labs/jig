#!/usr/bin/env node
/*
 * run-census.js — the MCP server census.
 *
 * For every server in data/census-servers.json this script shells out to the
 * `jig` binary twice over stdio (npx):
 *
 *   jig inspect --stdio "npx -y <pkg> [args]" --json --timeout 45
 *   jig budget  --stdio "npx -y <pkg> [args]" --json --model gpt-4o --timeout 45
 *
 * and records, honestly, what came back: handshake ok?, protocol version,
 * tool/resource/prompt counts, total gpt-4o context tokens, the single most
 * expensive tool, advertised capabilities (and which of them are advertised but
 * empty), and any stdout-pollution jig warned about on its stderr. Failures are
 * recorded with a reason and EXCLUDED from the percentile samples but REPORTED
 * in the raw file — a census that silently drops failures lies about the
 * ecosystem.
 *
 * Zero dependencies beyond Node builtins. Windows- and Linux-safe: it calls the
 * jig binary by explicit path and lets jig resolve the npx shim itself; the only
 * shell-ish concern (a temp path containing spaces) is handled by quoting.
 *
 * Usage:
 *   node scripts/census/run-census.js [--jig <path>] [--list <path>]
 *                                     [--out-raw <path>] [--out-pct <path>]
 *                                     [--timeout <sec>] [--limit <n>]
 *   env: JIG_BIN, CENSUS_TIMEOUT
 */

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

// ---------------------------------------------------------------------------
// Paths & args
// ---------------------------------------------------------------------------

const REPO_ROOT = path.resolve(__dirname, '..', '..');

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

function defaultJigBin() {
  const exe = process.platform === 'win32' ? 'jig.exe' : 'jig';
  const candidates = [
    process.env.JIG_BIN,
    path.join(REPO_ROOT, 'target', 'debug', exe),
    path.join(REPO_ROOT, 'target', 'release', exe),
  ].filter(Boolean);
  for (const c of candidates) {
    if (fs.existsSync(c)) return c;
  }
  // Fall back to the debug path even if missing, so the error message is clear.
  return candidates[1] || candidates[0];
}

const JIG_BIN = args.jig ? path.resolve(args.jig) : defaultJigBin();
const LIST_PATH = args.list
  ? path.resolve(args.list)
  : path.join(REPO_ROOT, 'data', 'census-servers.json');
const OUT_RAW = args['out-raw']
  ? path.resolve(args['out-raw'])
  : path.join(REPO_ROOT, 'data', 'census-raw.json');
const OUT_PCT = args['out-pct']
  ? path.resolve(args['out-pct'])
  : path.join(REPO_ROOT, 'data', 'percentiles.json');

// jig's own per-request timeout (seconds).
const JIG_TIMEOUT = Number(args.timeout || process.env.CENSUS_TIMEOUT || 45);
// Hard wall for a single jig invocation (ms). Larger than JIG_TIMEOUT to allow
// for npx cold-start install + jig's own timeout to fire cleanly first.
const CMD_WALL_MS = (JIG_TIMEOUT + 45) * 1000;
// The gpt-4o model is the canonical context-cost tokenizer for the census.
const BUDGET_MODEL = 'gpt-4o';
// Attempts per server (the whole server is retried once if the first fails).
const MAX_ATTEMPTS = 2;
const LIMIT = args.limit ? Number(args.limit) : Infinity;

if (!fs.existsSync(JIG_BIN)) {
  console.error(
    `census: jig binary not found at ${JIG_BIN}\n` +
      `        build it first: cargo build -p jig-cli   (or pass --jig <path> / JIG_BIN)`
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
  return (rawArgs || []).map((a) => {
    if (a === '{TMPDIR}' || a.includes('{TMPDIR}')) {
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'jig-census-'));
      return a.replace('{TMPDIR}', dir);
    }
    return a;
  });
}

// Build the single --stdio command string jig expects. jig's own splitter
// understands double-quoted segments, so we quote any token containing
// whitespace (e.g. a Windows temp path with spaces).
function buildStdioCommand(pkg, resolvedArgs) {
  const quote = (t) => (/\s/.test(t) ? `"${t}"` : t);
  return ['npx', '-y', pkg, ...resolvedArgs].map(quote).join(' ');
}

// ---------------------------------------------------------------------------
// Running jig
// ---------------------------------------------------------------------------

function runJig(subcommand, stdioCmd, extraArgs) {
  const argv = [subcommand, '--stdio', stdioCmd, '--json', '--timeout', String(JIG_TIMEOUT), ...extraArgs];
  const res = spawnSync(JIG_BIN, argv, {
    encoding: 'utf8',
    timeout: CMD_WALL_MS,
    maxBuffer: 96 * 1024 * 1024,
    windowsHide: true,
  });
  return {
    status: res.status,
    signal: res.signal,
    stdout: res.stdout || '',
    stderr: res.stderr || '',
    // spawnSync sets .error on spawn failure or timeout kill.
    spawnError: res.error ? String(res.error.message || res.error) : null,
    killedByWall: res.signal === 'SIGTERM' || (res.error && res.error.code === 'ETIMEDOUT'),
  };
}

// jig prints a specific stderr warning when a server pollutes stdout with
// non-JSON-RPC lines. Extract the count if present.
function pollutionFromStderr(stderr) {
  const m = /wrote (\d+) non-protocol line\(s\) to stdout/.exec(stderr);
  return m ? Number(m[1]) : 0;
}

function firstStderrLine(stderr) {
  const line = (stderr || '')
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean)
    .find((s) => s.startsWith('jig: error') || s.startsWith('jig:')) ||
    (stderr || '').split(/\r?\n/).map((s) => s.trim()).find(Boolean);
  return line || '';
}

// ---------------------------------------------------------------------------
// Per-server census
// ---------------------------------------------------------------------------

function censusOne(entry) {
  const resolvedArgs = materializeArgs(entry.args);
  const stdioCmd = buildStdioCommand(entry.package, resolvedArgs);

  const base = {
    package: entry.package,
    args: resolvedArgs,
    note: entry.note || '',
    source: entry.source || '',
    stdioCommand: stdioCmd,
  };

  let lastResult = null;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    const started = Date.now();
    const inspect = runJig('inspect', stdioCmd, []);
    const rec = interpretInspect(inspect);
    rec.attempt = attempt;

    if (rec.handshakeOk) {
      // Only price a server we could actually inspect.
      const budget = runJig('budget', stdioCmd, ['--model', BUDGET_MODEL]);
      Object.assign(rec, interpretBudget(budget));
      rec.durationMs = Date.now() - started;
      return Object.assign(base, rec);
    }

    rec.durationMs = Date.now() - started;
    lastResult = Object.assign(base, rec);
    if (attempt < MAX_ATTEMPTS) {
      // brief backoff before the retry (npx/network flake)
      const until = Date.now() + 1500;
      while (Date.now() < until) {
        /* busy-wait keeps the script single-threaded & dependency-free */
      }
    }
  }
  return lastResult;
}

function interpretInspect(r) {
  const pollution = pollutionFromStderr(r.stderr);
  if (r.killedByWall) {
    return {
      handshakeOk: false,
      failureStage: 'inspect',
      failureReason: `timed out (no handshake within ${CMD_WALL_MS / 1000}s wall)`,
      stdoutPollutionLines: pollution,
    };
  }
  let doc = null;
  if (r.stdout.trim()) {
    try {
      doc = JSON.parse(r.stdout);
    } catch (_e) {
      doc = null;
    }
  }
  if (!doc) {
    return {
      handshakeOk: false,
      failureStage: 'inspect',
      failureReason:
        firstStderrLine(r.stderr) ||
        r.spawnError ||
        `jig inspect exited ${r.status} with no JSON on stdout`,
      exitStatus: r.status,
      stdoutPollutionLines: pollution,
    };
  }

  const tools = Array.isArray(doc.tools) ? doc.tools : [];
  const resources = Array.isArray(doc.resources) ? doc.resources : [];
  const prompts = Array.isArray(doc.prompts) ? doc.prompts : [];
  const capabilities = doc.capabilities || {};
  const capKeys = Object.keys(capabilities).sort();

  // A capability advertised in the handshake but backed by an empty list is a
  // small honesty gap we want to quantify across the ecosystem.
  const advertisedButEmpty = [];
  if (capabilities.tools && tools.length === 0) advertisedButEmpty.push('tools');
  if (capabilities.resources && resources.length === 0) advertisedButEmpty.push('resources');
  if (capabilities.prompts && prompts.length === 0) advertisedButEmpty.push('prompts');

  return {
    handshakeOk: true,
    protocolVersion: doc.protocolVersion || null,
    serverInfo: doc.serverInfo || null,
    toolCount: tools.length,
    resourceCount: resources.length,
    promptCount: prompts.length,
    capabilities: capKeys,
    capabilitiesAdvertisedButEmpty: advertisedButEmpty,
    hasInstructions: typeof doc.instructions === 'string' && doc.instructions.length > 0,
    stdoutPollutionLines: pollution,
  };
}

function interpretBudget(r) {
  const pollution = pollutionFromStderr(r.stderr);
  const out = { budgetOk: false, budgetPollutionLines: pollution };
  if (r.killedByWall) {
    out.budgetFailureReason = 'budget timed out';
    return out;
  }
  let doc = null;
  if (r.stdout.trim()) {
    try {
      doc = JSON.parse(r.stdout);
    } catch (_e) {
      doc = null;
    }
  }
  if (!doc || !Array.isArray(doc.models)) {
    out.budgetFailureReason = firstStderrLine(r.stderr) || `budget exited ${r.status}`;
    return out;
  }
  const model = doc.models.find((m) => m.model === BUDGET_MODEL) || doc.models[0];
  if (!model) {
    out.budgetFailureReason = 'no model column in budget output';
    return out;
  }
  const toolTokens = (model.tools || []).map((t) => t.tokens || 0);
  out.budgetOk = true;
  out.contextCostTokens = model.total; // total gpt-4o context cost of the tool surface
  out.instructionsTokens = model.instructionsTokens || 0;
  out.maxToolTokens = toolTokens.length ? Math.max(...toolTokens) : 0;
  out.tokenExact = !!(model.totalExactness && model.totalExactness.exact);
  return out;
}

// ---------------------------------------------------------------------------
// Percentiles
// ---------------------------------------------------------------------------

function sortedInts(xs) {
  return xs.filter((x) => Number.isFinite(x)).map((x) => Math.round(x)).sort((a, b) => a - b);
}

function buildPercentiles(reachable, collectedIso, allResults) {
  const contextSamples = sortedInts(
    reachable.filter((r) => r.budgetOk).map((r) => r.contextCostTokens)
  );
  const toolSamples = sortedInts(reachable.map((r) => r.toolCount));
  const maxToolSamples = sortedInts(
    reachable.filter((r) => r.budgetOk).map((r) => r.maxToolTokens)
  );
  // Ecosystem startup-failure rate: the fraction of attempted servers that
  // failed at startup / during the handshake (never became reachable). `jig
  // check` surfaces this as one line of cohort context when a checked server
  // fails to start. A fraction in [0,1]; null when nothing was attempted.
  const results = Array.isArray(allResults) ? allResults : [];
  const attempted = results.length;
  const failedAtStartup = results.filter((r) => !r.handshakeOk).length;
  const startupFailureRate =
    attempted > 0 ? Number((failedAtStartup / attempted).toFixed(4)) : null;
  return {
    _schema:
      'jig census percentiles v0. Sorted integer sample arrays the `jig check` command interpolates ' +
      'percentiles from. Only servers that completed the handshake (and, for token metrics, that jig ' +
      'could price) are included; failures are excluded here and reported in census-raw.json. ' +
      '`startup_failure_rate` is the fraction of attempted servers that failed at startup/handshake.',
    _note_for_director:
      'docs/percentiles-schema.md did not exist on this branch base at authoring time. Field names ' +
      '(context_cost_tokens / tool_count / max_tool_tokens) are provisional — reconcile with the ' +
      '`jig check` author at merge. Values are gpt-4o token counts and raw tool counts.',
    collected: collectedIso,
    n: contextSamples.length,
    model: BUDGET_MODEL,
    startup_failure_rate: startupFailureRate,
    context_cost_tokens: { unit: 'tokens', model: BUDGET_MODEL, samples: contextSamples },
    tool_count: { unit: 'count', samples: toolSamples },
    max_tool_tokens: { unit: 'tokens', model: BUDGET_MODEL, samples: maxToolSamples },
  };
}

function percentile(sorted, p) {
  if (!sorted.length) return null;
  const idx = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  if (lo === hi) return sorted[lo];
  return Math.round(sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo));
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function main() {
  const servers = loadServers().slice(0, LIMIT);
  const startedIso = new Date().toISOString();
  const wallStart = Date.now();

  console.error(`census: ${servers.length} servers, jig=${JIG_BIN}, timeout=${JIG_TIMEOUT}s`);

  const results = [];
  for (let i = 0; i < servers.length; i++) {
    const entry = servers[i];
    process.stderr.write(`[${i + 1}/${servers.length}] ${entry.package} ... `);
    let rec;
    try {
      rec = censusOne(entry);
    } catch (e) {
      rec = {
        package: entry.package,
        args: entry.args || [],
        note: entry.note || '',
        source: entry.source || '',
        handshakeOk: false,
        failureStage: 'harness',
        failureReason: `census harness error: ${String(e && e.message ? e.message : e)}`,
      };
    }
    results.push(rec);
    if (rec.handshakeOk) {
      const cost = rec.budgetOk ? `${rec.contextCostTokens} tok` : 'no budget';
      const poll = rec.stdoutPollutionLines ? ` POLLUTION x${rec.stdoutPollutionLines}` : '';
      console.error(`ok (${rec.toolCount} tools, ${cost})${poll}`);
    } else {
      console.error(`FAIL [${rec.failureStage}] ${rec.failureReason}`);
    }
  }

  const wallSeconds = Math.round((Date.now() - wallStart) / 1000);
  const collectedIso = new Date().toISOString();
  const reachable = results.filter((r) => r.handshakeOk);
  const failed = results.filter((r) => !r.handshakeOk);

  const raw = {
    _schema: 'jig census raw results v0',
    collected: collectedIso,
    started: startedIso,
    wallSeconds,
    jigBinary: path.basename(JIG_BIN),
    platform: `${process.platform} ${process.arch}`,
    node: process.version,
    budgetModel: BUDGET_MODEL,
    jigTimeoutSec: JIG_TIMEOUT,
    counts: {
      attempted: results.length,
      reachable: reachable.length,
      failed: failed.length,
      priced: reachable.filter((r) => r.budgetOk).length,
    },
    results,
  };
  fs.writeFileSync(OUT_RAW, JSON.stringify(raw, null, 2) + '\n');

  const pct = buildPercentiles(reachable, collectedIso, results);
  fs.writeFileSync(OUT_PCT, JSON.stringify(pct, null, 2) + '\n');

  // Console summary (stderr so stdout stays clean if ever piped).
  const ctx = pct.context_cost_tokens.samples;
  console.error('\n==================== CENSUS SUMMARY ====================');
  console.error(`attempted:   ${results.length}`);
  console.error(`reachable:   ${reachable.length}`);
  console.error(`failed:      ${failed.length}`);
  console.error(`priced:      ${pct.n}`);
  console.error(`wall time:   ${wallSeconds}s (${(wallSeconds / 60).toFixed(1)} min)`);
  if (ctx.length) {
    console.error(
      `context cost (gpt-4o tok): min ${ctx[0]}  median ${percentile(ctx, 50)}  ` +
        `p90 ${percentile(ctx, 90)}  max ${ctx[ctx.length - 1]}`
    );
  }
  console.error(`raw   -> ${OUT_RAW}`);
  console.error(`pct   -> ${OUT_PCT}`);
  console.error('========================================================');
}

main();
