"use strict";

// The money test: stand up a localhost "GitHub Releases" mirror hosting a REAL
// jig archive + a REAL SHA256SUMS, point the installer's download+verify+extract
// chain at it via JIG_DOWNLOAD_BASE, then RUN the extracted binary. This proves
// the exact code path that day-one `npx @shodh/jig` will take — no GitHub, no
// real release required.
//
// It exercises the same functions scripts/install.js calls (downloadBuffer,
// verifyChecksum, extractBinary) rather than a mocked stand-in.

const { test } = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const { buildZip } = require("./helpers/zip");
const { detectTarget } = require("../lib/platform");
const {
  archiveName,
  assetUrl,
  parseChecksums,
  expectedDigest,
  sha256,
  verifyChecksum,
} = require("../lib/release");
const { downloadBuffer } = require("../lib/download");
const { extractBinary } = require("../lib/extract");
const { version } = require("../package.json");

// Locate the real binary built into the repo's target/release dir.
const REPO_ROOT = path.join(__dirname, "..", "..");
const REAL_BIN = path.join(
  REPO_ROOT,
  "target",
  "release",
  process.platform === "win32" ? "jig.exe" : "jig"
);

test("localhost release: download -> verify -> extract -> run", { skip: !fs.existsSync(REAL_BIN) ? "no built binary at target/release" : false }, async () => {
  const { target, ext, binName } = detectTarget(process.platform, process.arch);
  const realBytes = fs.readFileSync(REAL_BIN);
  const archive = archiveName(version, target, ext);

  // Build the release archive in the same shape release.yml produces:
  // a staging dir jig-v{version}-{target}/ containing the binary.
  const member = `jig-v${version}-${target}/${binName}`;
  let archiveBytes;
  if (ext === "zip") {
    archiveBytes = buildZip([{ name: member, data: realBytes, method: 8 }]);
  } else {
    // On unix, tar up a staging dir so extractBinary's `tar` path is exercised.
    const staging = fs.mkdtempSync(path.join(os.tmpdir(), "jig-rel-"));
    const dir = path.join(staging, `jig-v${version}-${target}`);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, binName), realBytes);
    const out = path.join(staging, archive);
    execFileSync("tar", ["-czf", out, "-C", staging, `jig-v${version}-${target}`]);
    archiveBytes = fs.readFileSync(out);
  }

  // Generate a REAL SHA256SUMS for the archive.
  const sumsText = `${sha256(archiveBytes)}  ${archive}\n`;

  // Serve /v{version}/{archive} and /v{version}/SHA256SUMS.
  const routes = new Map([
    [`/v${version}/${archive}`, archiveBytes],
    [`/v${version}/SHA256SUMS`, Buffer.from(sumsText)],
  ]);
  const server = http.createServer((req, res) => {
    const body = routes.get(req.url);
    if (!body) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    res.statusCode = 200;
    res.end(body);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const base = `http://127.0.0.1:${port}`;

  try {
    // ---- exactly what install.js does ----
    const [dlArchive, dlSumsBuf] = await Promise.all([
      downloadBuffer(assetUrl(base, version, archive)),
      downloadBuffer(assetUrl(base, version, "SHA256SUMS")),
    ]);
    const checksums = parseChecksums(dlSumsBuf.toString("utf8"));
    const expected = expectedDigest(checksums, archive);
    verifyChecksum(dlArchive, expected, archive); // throws on mismatch
    const binBytes = extractBinary(dlArchive, { ext, version, target, binName });
    // ---------------------------------------

    assert.ok(binBytes.equals(realBytes), "extracted binary matches the original bytes");

    // Write it out, mark executable, and actually RUN it.
    const dest = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "jig-inst-")), binName);
    fs.writeFileSync(dest, binBytes);
    if (process.platform !== "win32") fs.chmodSync(dest, 0o755);
    const stdout = execFileSync(dest, ["--version"], { encoding: "utf8" });
    assert.match(stdout, /jig\s+\d+\.\d+\.\d+/);
  } finally {
    await new Promise((r) => server.close(r));
  }
});

test("localhost release: a corrupted archive is rejected before extraction", async () => {
  // No real binary needed — a bogus archive body must fail the checksum gate.
  const target = "x86_64-pc-windows-msvc";
  const archive = archiveName(version, target, "zip");
  const good = Buffer.from("the genuine archive bytes");
  const tampered = Buffer.from("the genuine archive bytes (tampered!)");
  const sumsText = `${sha256(good)}  ${archive}\n`; // checksum of the GOOD bytes

  const routes = new Map([
    [`/v${version}/${archive}`, tampered], // but we serve tampered bytes
    [`/v${version}/SHA256SUMS`, Buffer.from(sumsText)],
  ]);
  const server = http.createServer((req, res) => {
    const body = routes.get(req.url);
    res.statusCode = body ? 200 : 404;
    res.end(body || "not found");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const base = `http://127.0.0.1:${port}`;

  try {
    const [dlArchive, dlSumsBuf] = await Promise.all([
      downloadBuffer(assetUrl(base, version, archive)),
      downloadBuffer(assetUrl(base, version, "SHA256SUMS")),
    ]);
    const expected = expectedDigest(parseChecksums(dlSumsBuf.toString("utf8")), archive);
    assert.throws(() => verifyChecksum(dlArchive, expected, archive), /checksum mismatch/);
  } finally {
    await new Promise((r) => server.close(r));
  }
});
