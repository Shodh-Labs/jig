#!/usr/bin/env node
/*
 * summarize.js — render a census run as GitHub-flavored markdown on stdout.
 *
 * Reads data/census-raw.json and data/percentiles.json (or paths passed as
 * argv[2]/argv[3]) and prints a compact summary suitable for appending to
 * $GITHUB_STEP_SUMMARY. Zero dependencies. Safe to run with `if: always()` —
 * if the raw file is missing it says so and exits 0 rather than failing the job.
 */

'use strict';

const fs = require('fs');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..', '..');
const RAW = process.argv[2] || path.join(REPO_ROOT, 'data', 'census-raw.json');
const PCT = process.argv[3] || path.join(REPO_ROOT, 'data', 'percentiles.json');

function percentile(sorted, p) {
  if (!sorted || !sorted.length) return null;
  const idx = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  if (lo === hi) return sorted[lo];
  return Math.round(sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo));
}

function out(s) {
  process.stdout.write(s + '\n');
}

if (!fs.existsSync(RAW)) {
  out('## MCP Server Census');
  out('');
  out('_No `census-raw.json` produced — the census run did not complete. Check the job logs._');
  process.exit(0);
}

const raw = JSON.parse(fs.readFileSync(RAW, 'utf8'));
const pct = fs.existsSync(PCT) ? JSON.parse(fs.readFileSync(PCT, 'utf8')) : null;
const results = raw.results || [];
const reachable = results.filter((r) => r.handshakeOk);
const failed = results.filter((r) => !r.handshakeOk);
const priced = reachable.filter((r) => r.budgetOk);

out('## MCP Server Census');
out('');
out(`Collected \`${raw.collected}\` on \`${raw.platform}\`, Node \`${raw.node}\`, in ${raw.wallSeconds}s.`);
out('');
out('| Metric | Value |');
out('| --- | --- |');
out(`| Attempted | ${results.length} |`);
out(`| Reachable (handshake ok) | ${reachable.length} |`);
out(`| Failed | ${failed.length} |`);
out(`| Priced (${raw.budgetModel}) | ${priced.length} |`);

const ctx = pct && pct.context_cost_tokens ? pct.context_cost_tokens.samples : [];
if (ctx.length) {
  out(`| Context cost min / median / p90 / max | ${ctx[0]} / ${percentile(ctx, 50)} / ${percentile(ctx, 90)} / ${ctx[ctx.length - 1]} tok |`);
}
const withPollution = reachable.filter((r) => (r.stdoutPollutionLines || 0) > 0);
out(`| Servers polluting stdout | ${withPollution.length} |`);
const withEmptyCaps = reachable.filter((r) => (r.capabilitiesAdvertisedButEmpty || []).length > 0);
out(`| Advertising empty capabilities | ${withEmptyCaps.length} |`);
out('');

if (priced.length) {
  const heavy = [...priced].sort((a, b) => b.contextCostTokens - a.contextCostTokens).slice(0, 5);
  out('### Heaviest servers (context cost)');
  out('');
  out('| Server | Tools | Context tokens |');
  out('| --- | ---: | ---: |');
  for (const r of heavy) out(`| \`${r.package}\` | ${r.toolCount} | ${r.contextCostTokens} |`);
  out('');
}

if (failed.length) {
  out('### Failures (excluded from percentiles, reported here)');
  out('');
  out('| Server | Stage | Reason |');
  out('| --- | --- | --- |');
  for (const r of failed) {
    const reason = String(r.failureReason || '').replace(/\|/g, '\\|').slice(0, 160);
    out(`| \`${r.package}\` | ${r.failureStage || '?'} | ${reason} |`);
  }
  out('');
}

out('> Percentiles are **not** auto-committed. Review the uploaded `percentiles.json` and commit by hand.');
