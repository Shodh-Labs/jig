"use strict";

const { test } = require("node:test");
const assert = require("node:assert");
const crypto = require("node:crypto");
const {
  archiveName,
  assetUrl,
  parseChecksums,
  expectedDigest,
  sha256,
  verifyChecksum,
  DEFAULT_BASE,
} = require("../lib/release");

test("archiveName matches release.yml naming", () => {
  assert.strictEqual(
    archiveName("0.1.0", "x86_64-pc-windows-msvc", "zip"),
    "jig-v0.1.0-x86_64-pc-windows-msvc.zip"
  );
  assert.strictEqual(
    archiveName("0.1.0", "x86_64-apple-darwin", "tar.gz"),
    "jig-v0.1.0-x86_64-apple-darwin.tar.gz"
  );
});

test("assetUrl builds {base}/v{version}/{file} and trims trailing slashes", () => {
  assert.strictEqual(
    assetUrl(DEFAULT_BASE, "0.1.0", "jig-v0.1.0-x86_64-apple-darwin.tar.gz"),
    "https://github.com/Shodh-Labs/jig/releases/download/v0.1.0/jig-v0.1.0-x86_64-apple-darwin.tar.gz"
  );
  assert.strictEqual(
    assetUrl("http://127.0.0.1:8080/", "0.1.0", "SHA256SUMS"),
    "http://127.0.0.1:8080/v0.1.0/SHA256SUMS"
  );
});

test("parseChecksums handles two-space (text) and star (binary) forms, skips junk", () => {
  const text = [
    "# a comment line",
    "",
    "aa".repeat(32) + "  jig-v0.1.0-x86_64-apple-darwin.tar.gz",
    "bb".repeat(32) + " *jig-v0.1.0-x86_64-pc-windows-msvc.zip",
    "garbage line without a hash",
  ].join("\n");
  const map = parseChecksums(text);
  assert.strictEqual(map.size, 2);
  assert.strictEqual(map.get("jig-v0.1.0-x86_64-apple-darwin.tar.gz"), "aa".repeat(32));
  assert.strictEqual(map.get("jig-v0.1.0-x86_64-pc-windows-msvc.zip"), "bb".repeat(32));
});

test("expectedDigest throws when the file is absent from SHA256SUMS", () => {
  const map = parseChecksums("aa".repeat(32) + "  other.tar.gz");
  assert.throws(() => expectedDigest(map, "missing.zip"), /not found in SHA256SUMS/);
});

test("sha256 matches Node's own crypto digest", () => {
  const buf = Buffer.from("hello jig");
  const expected = crypto.createHash("sha256").update(buf).digest("hex");
  assert.strictEqual(sha256(buf), expected);
});

test("verifyChecksum accepts a match and rejects a mismatch loudly", () => {
  const buf = Buffer.from("the real archive bytes");
  const good = sha256(buf);
  assert.strictEqual(verifyChecksum(buf, good, "archive.zip"), true);
  assert.strictEqual(verifyChecksum(buf, good.toUpperCase(), "archive.zip"), true); // case-insensitive

  assert.throws(
    () => verifyChecksum(buf, "00".repeat(32), "archive.zip"),
    (e) => {
      assert.match(e.message, /checksum mismatch for "archive.zip"/);
      assert.match(e.message, /expected: 00/);
      assert.match(e.message, new RegExp("actual:   " + good));
      return true;
    }
  );
});
