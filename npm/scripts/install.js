#!/usr/bin/env node
"use strict";

// postinstall: put the right prebuilt `jig` binary in place for this host.
//
// Flow:
//   1. Detect platform/arch -> release target (lib/platform.js).
//   2. If JIG_BINARY_PATH is set, use that local binary and skip all network
//      (CI / air-gapped / testing). Otherwise:
//   3. Download the archive + SHA256SUMS from GitHub Releases (or the base in
//      JIG_DOWNLOAD_BASE), VERIFY the archive's SHA-256 against SHA256SUMS
//      BEFORE extracting, then extract just the binary.
//   4. chmod +x on unix, write a version stamp. Idempotent: a matching stamp +
//      present binary short-circuits.
//
// Zero runtime dependencies — Node builtins only (https, fs, crypto, zlib).

const fs = require("fs");

const { version } = require("../package.json");
const { detectTarget } = require("../lib/platform");
const {
  DEFAULT_BASE,
  archiveName,
  assetUrl,
  parseChecksums,
  expectedDigest,
  verifyChecksum,
} = require("../lib/release");
const { downloadBuffer, warnIfProxyConfigured } = require("../lib/download");
const { extractBinary } = require("../lib/extract");
const { VENDOR_DIR, binaryPath, stampPath } = require("../lib/paths");

function log(msg) {
  console.log(`@shodh/jig: ${msg}`);
}

/** True if a matching binary + version stamp are already in place. */
function alreadyInstalled(dest) {
  try {
    const stamp = fs.readFileSync(stampPath(), "utf8").trim();
    return stamp === version && fs.existsSync(dest);
  } catch {
    return false;
  }
}

/** Write the binary bytes to `dest`, chmod +x on unix, and stamp the version. */
function place(dest, bytes) {
  fs.mkdirSync(VENDOR_DIR, { recursive: true });
  fs.writeFileSync(dest, bytes);
  if (process.platform !== "win32") {
    fs.chmodSync(dest, 0o755);
  }
  fs.writeFileSync(stampPath(), `${version}\n`);
}

async function main() {
  const { target, ext, binName } = detectTarget(process.platform, process.arch);
  const dest = binaryPath(process.platform);

  if (alreadyInstalled(dest)) {
    log(`v${version} already installed for ${target} — nothing to do.`);
    return;
  }

  // (2) Local-binary override: use it verbatim, skip the network entirely.
  const override = process.env.JIG_BINARY_PATH;
  if (override) {
    if (!fs.existsSync(override)) {
      throw new Error(`@shodh/jig: JIG_BINARY_PATH is set but "${override}" does not exist.`);
    }
    log(`using local binary from JIG_BINARY_PATH (${override}); skipping download.`);
    place(dest, fs.readFileSync(override));
    log(`installed jig v${version} -> ${dest}`);
    return;
  }

  // (3) Download + verify + extract.
  const base = process.env.JIG_DOWNLOAD_BASE || DEFAULT_BASE;
  const archive = archiveName(version, target, ext);
  const archiveDownloadUrl = assetUrl(base, version, archive);
  const sumsUrl = assetUrl(base, version, "SHA256SUMS");

  warnIfProxyConfigured();
  log(`downloading ${archiveDownloadUrl}`);
  const [archiveBytes, sumsText] = await Promise.all([
    downloadBuffer(archiveDownloadUrl),
    downloadBuffer(sumsUrl).then((b) => b.toString("utf8")),
  ]);

  // Verify BEFORE extracting — never install unverified bytes.
  const checksums = parseChecksums(sumsText);
  const expected = expectedDigest(checksums, archive);
  verifyChecksum(archiveBytes, expected, archive);
  log(`checksum OK (sha256 ${expected})`);

  const bytes = extractBinary(archiveBytes, { ext, version, target, binName });
  place(dest, bytes);
  log(`installed jig v${version} -> ${dest}`);
}

main().catch((err) => {
  console.error(`\n${err && err.message ? err.message : err}\n`);
  process.exit(1);
});
