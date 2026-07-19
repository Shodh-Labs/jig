"use strict";

// Cross-platform end-to-end smoke for the @shodh/jig npm installer.
//
// It stands up a localhost "GitHub Releases" mirror hosting a REAL jig release
// archive (built in the release layout release.yml produces) plus a REAL
// SHA256SUMS, points the installer at it via JIG_DOWNLOAD_BASE, `npm pack`s the
// installer package, installs the tarball into a throwaway project (running the
// real postinstall: download -> checksum-verify -> extract), and finally RUNS
// the installed launcher shim:
//
//   * `jig --version`                 — the downloaded binary runs, exit 0
//   * `jig inspect --stdio <mock>`    — a real MCP session end to end, exit 0
//
// This is what makes "works on macOS / Linux / Windows" a tested claim rather
// than an aspiration. It reuses the same lib/ + test/helpers the unit tests use,
// so the code path exercised here is the one day-one `npx @shodh/jig` takes.

const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const assert = require("node:assert");
const { execFileSync, spawn } = require("node:child_process");

const REPO_ROOT = path.join(__dirname, "..", "..");
const NPM_DIR = path.join(REPO_ROOT, "npm");

const { detectTarget } = require(path.join(NPM_DIR, "lib", "platform"));
const { archiveName, sha256 } = require(path.join(NPM_DIR, "lib", "release"));
const { buildZip } = require(path.join(NPM_DIR, "test", "helpers", "zip"));
const { version } = require(path.join(NPM_DIR, "package.json"));

function run(cmd, args, opts = {}) {
  execFileSync(cmd, args, { stdio: "inherit", ...opts });
}

// Run npm asynchronously through the platform shell (npm is a .cmd on Windows
// that Node refuses to spawn directly). It MUST be async: the localhost release
// mirror lives in this process's event loop, so a *blocking* child would
// deadlock the very download the installer performs. The caller quotes any path
// argument that might contain spaces.
function npm(command, opts = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(`npm ${command}`, { stdio: "inherit", shell: true, ...opts });
    child.on("error", reject);
    child.on("close", (code) =>
      code === 0 ? resolve() : reject(new Error(`\`npm ${command}\` exited with ${code}`))
    );
  });
}

async function main() {
  const { target, ext, binName } = detectTarget(process.platform, process.arch);

  const releaseDir = path.join(REPO_ROOT, "target", "release");
  const realBin = path.join(releaseDir, binName);
  const mockBin = path.join(
    releaseDir,
    process.platform === "win32" ? "jig-mock-server.exe" : "jig-mock-server"
  );
  assert.ok(fs.existsSync(realBin), `missing release binary: ${realBin}`);
  assert.ok(fs.existsSync(mockBin), `missing mock server binary: ${mockBin}`);

  const realBytes = fs.readFileSync(realBin);
  const archive = archiveName(version, target, ext);
  // The archive holds a top-level staging dir, exactly as release.yml lays out:
  //   jig-v{version}-{target}/{binName}
  const staging = `jig-v${version}-${target}`;
  const member = `${staging}/${binName}`;

  let archiveBytes;
  if (ext === "zip") {
    archiveBytes = buildZip([{ name: member, data: realBytes, method: 8 }]);
  } else {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "jig-smoke-rel-"));
    const dir = path.join(tmp, staging);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, binName), realBytes);
    const out = path.join(tmp, archive);
    execFileSync("tar", ["-czf", out, "-C", tmp, staging]);
    archiveBytes = fs.readFileSync(out);
  }

  // A REAL SHA256SUMS for the archive (sha256sum's `<hex>  <name>` format).
  const sumsText = `${sha256(archiveBytes)}  ${archive}\n`;

  // Serve the release layout: /v{version}/{archive} and /v{version}/SHA256SUMS.
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
  const base = `http://127.0.0.1:${server.address().port}`;
  console.log(`serving release archive at ${base}`);

  const workDir = fs.mkdtempSync(path.join(os.tmpdir(), "jig-smoke-"));
  const tgz = path.join(NPM_DIR, `shodh-jig-${version}.tgz`);
  try {
    // Pack the installer package into the real .tgz users would receive.
    await npm("pack", { cwd: NPM_DIR });
    assert.ok(fs.existsSync(tgz), `npm pack did not produce ${tgz}`);

    // Install into a throwaway project, pointing the installer at our mirror.
    // The postinstall downloads, verifies the checksum, and extracts for real.
    await npm("init -y", { cwd: workDir });
    await npm(`install "${tgz}"`, {
      cwd: workDir,
      env: { ...process.env, JIG_DOWNLOAD_BASE: base },
    });
  } finally {
    await new Promise((r) => server.close(r));
    fs.rmSync(tgz, { force: true });
  }

  // Run the installed launcher shim through node, cross-platform.
  const shim = path.join(workDir, "node_modules", "@shodh", "jig", "bin", "jig.js");
  assert.ok(fs.existsSync(shim), `installed launcher shim not found at ${shim}`);

  // 1) jig --version — the downloaded binary runs and reports its version.
  const ver = execFileSync(process.execPath, [shim, "--version"], { encoding: "utf8" });
  console.log(`jig --version => ${ver.trim()}`);
  assert.match(ver, /jig\s+\d+\.\d+\.\d+/, "jig --version output looks wrong");

  // 2) jig inspect --stdio <mock> — a full MCP handshake + list, exit 0. Quote
  // the path: jig's --stdio parser splits on whitespace unless quoted.
  run(process.execPath, [shim, "inspect", "--stdio", `"${mockBin}"`], { cwd: workDir });

  console.log("npm install smoke: OK");
}

main().catch((err) => {
  console.error(err && err.stack ? err.stack : String(err));
  process.exit(1);
});
