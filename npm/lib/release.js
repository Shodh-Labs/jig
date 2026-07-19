"use strict";

// URL / archive-name construction and SHA256SUMS parsing + verification.
// All pure functions (crypto is a Node builtin) so the security-critical bits
// are unit-testable in isolation (see test/release.test.js).

const crypto = require("crypto");

// The canonical GitHub Releases download base. Overridable at install time via
// the JIG_DOWNLOAD_BASE env var (used by the localhost download simulation test
// and by air-gapped mirrors).
const DEFAULT_BASE = "https://github.com/Shodh-Labs/jig/releases/download";

/**
 * The archive file name for a given version/target/ext, matching the naming in
 * release.yml: `jig-v{VERSION}-{TARGET}.{ext}`.
 */
function archiveName(version, target, ext) {
  return `jig-v${version}-${target}.${ext}`;
}

/**
 * Full download URL for a release asset (an archive, or "SHA256SUMS").
 * Layout: `{base}/v{version}/{fileName}`.
 */
function assetUrl(base, version, fileName) {
  const trimmed = base.replace(/\/+$/, "");
  return `${trimmed}/v${version}/${fileName}`;
}

/**
 * Parse a SHA256SUMS file (the `sha256sum`/`shasum -a 256` format:
 * `<hex>  <filename>`, two spaces, or `<hex> *<filename>` in binary mode).
 * @returns {Map<string, string>} filename -> lowercase hex digest.
 */
function parseChecksums(text) {
  const map = new Map();
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    // First whitespace run separates the digest from the filename; the filename
    // may carry a leading "*" (binary-mode marker) which we strip.
    const m = line.match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (!m) continue;
    map.set(m[2].trim(), m[1].toLowerCase());
  }
  return map;
}

/**
 * Look up the expected digest for a file, throwing if absent (a missing entry
 * means we cannot verify — never install unverified bytes).
 */
function expectedDigest(checksums, fileName) {
  const digest = checksums.get(fileName);
  if (!digest) {
    throw new Error(
      `@shodh/jig: "${fileName}" not found in SHA256SUMS — cannot verify download integrity. ` +
        `Refusing to install unverified bytes.`
    );
  }
  return digest;
}

/** SHA-256 of a buffer as lowercase hex. */
function sha256(buffer) {
  return crypto.createHash("sha256").update(buffer).digest("hex");
}

/**
 * Verify a downloaded archive against its expected digest. Throws loudly on
 * mismatch — the caller must NOT extract on a thrown error.
 */
function verifyChecksum(buffer, expectedHex, fileName) {
  const actual = sha256(buffer);
  if (actual !== expectedHex.toLowerCase()) {
    throw new Error(
      `@shodh/jig: checksum mismatch for "${fileName}".\n` +
        `  expected: ${expectedHex.toLowerCase()}\n` +
        `  actual:   ${actual}\n` +
        `The download is corrupt or tampered with. Aborting install.`
    );
  }
  return true;
}

module.exports = {
  DEFAULT_BASE,
  archiveName,
  assetUrl,
  parseChecksums,
  expectedDigest,
  sha256,
  verifyChecksum,
};
