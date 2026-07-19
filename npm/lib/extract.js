"use strict";

// Pull the single `jig`/`jig.exe` binary out of a verified release archive held
// in memory, returning its raw bytes.
//
//   * .zip  (Windows) — parsed in-process by lib/unzip.js. No shell, no temp
//     file: the verified bytes never hit disk unverified.
//   * .tar.gz (macOS/Linux) — Node has gzip (zlib) but no tar reader, and a
//     from-scratch tar parser is more surface than warranted when `tar(1)` is
//     universally present on our supported unix trio (musl Linux, both macOS).
//     We write the already-verified archive to a temp file and ask `tar` to
//     stream out just the one member (`-O`), so still only one file is
//     extracted. A pure-JS tar fallback is intentionally NOT implemented
//     (documented in the README).

const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const { extractEntry } = require("./unzip");

/**
 * @param {Buffer} archive - the verified archive bytes.
 * @param {{ext: string, version: string, target: string, binName: string}} meta
 * @returns {Buffer} the binary's bytes.
 */
function extractBinary(archive, meta) {
  const { ext, version, target, binName } = meta;
  // The archive contains a top-level staging dir (see release.yml):
  //   jig-v{version}-{target}/{binName}
  const member = `jig-v${version}-${target}/${binName}`;
  // Match by basename so we are robust to path-separator quirks.
  const matches = (name) => name.split("/").pop() === binName;

  if (ext === "zip") {
    return extractEntry(archive, matches);
  }

  if (ext === "tar.gz") {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "jig-npm-"));
    const tmpArchive = path.join(tmpDir, "jig.tar.gz");
    try {
      fs.writeFileSync(tmpArchive, archive);
      // `-O` streams the member to stdout; capture it as a Buffer. Cap the
      // buffer generously (64 MiB) — the real binary is a few MiB.
      const out = execFileSync("tar", ["-xzOf", tmpArchive, member], {
        maxBuffer: 64 * 1024 * 1024,
      });
      if (!out || out.length === 0) {
        throw new Error(`@shodh/jig: tar produced no bytes for member "${member}"`);
      }
      return out;
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  }

  throw new Error(`@shodh/jig: unknown archive extension "${ext}"`);
}

module.exports = { extractBinary };
