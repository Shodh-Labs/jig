#!/usr/bin/env node
"use strict";

// Thin shim: exec the installed native `jig` binary, passing through argv,
// inheriting stdio, and forwarding its exit code faithfully.
//
// jig uses exit codes meaningfully: 0 = success, 1 = jig-level failure,
// 2 = tool reported isError. This shim must not swallow or remap them.

const fs = require("fs");
const { spawnSync } = require("child_process");
const { binaryPath } = require("../lib/paths");

const bin = binaryPath();

if (!fs.existsSync(bin)) {
  console.error(
    `@shodh/jig: the jig binary is not installed at ${bin}.\n` +
      `The postinstall step may have failed. Try reinstalling:\n` +
      `  npm install --force @shodh/jig\n` +
      `or set JIG_BINARY_PATH to a local binary and reinstall.`
  );
  process.exit(1);
}

// `stdio: "inherit"` wires the child straight to our stdin/stdout/stderr so
// interactive/streaming output and TTY detection behave exactly as if jig were
// invoked directly. spawnSync keeps this process alive until the child exits.
const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error(`@shodh/jig: failed to launch jig binary: ${result.error.message}`);
  process.exit(1);
}

// If the child was killed by a signal, surface that; otherwise forward its code.
if (result.signal) {
  process.kill(process.pid, result.signal);
  process.exit(1); // in case the signal did not terminate us
}

process.exit(result.status === null ? 1 : result.status);
