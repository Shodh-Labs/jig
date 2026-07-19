"use strict";

const { test } = require("node:test");
const assert = require("node:assert");
const { extractEntry } = require("../lib/unzip");
const { buildZip } = require("./helpers/zip");

test("extracts a deflate (method 8) entry, matching by basename", () => {
  const payload = Buffer.from("A".repeat(5000)); // compressible -> exercises inflate
  const zip = buildZip([
    { name: "jig-v0.1.0-x86_64-pc-windows-msvc/LICENSE", data: Buffer.from("license"), method: 8 },
    { name: "jig-v0.1.0-x86_64-pc-windows-msvc/jig.exe", data: payload, method: 8 },
  ]);
  const out = extractEntry(zip, (n) => n.split("/").pop() === "jig.exe");
  assert.ok(out.equals(payload));
});

test("extracts a stored (method 0) entry", () => {
  const payload = Buffer.from([0, 1, 2, 3, 255, 254, 10, 13]);
  const zip = buildZip([{ name: "dir/jig.exe", data: payload, method: 0 }]);
  const out = extractEntry(zip, (n) => n.endsWith("jig.exe"));
  assert.ok(out.equals(payload));
});

test("preserves exact binary bytes for a larger random payload", () => {
  const payload = require("node:crypto").randomBytes(200000);
  const zip = buildZip([{ name: "d/jig.exe", data: payload, method: 8 }]);
  const out = extractEntry(zip, (n) => n.endsWith("jig.exe"));
  assert.strictEqual(out.length, payload.length);
  assert.ok(out.equals(payload));
});

test("throws when the requested entry is absent", () => {
  const zip = buildZip([{ name: "d/README.md", data: Buffer.from("x") }]);
  assert.throws(() => extractEntry(zip, (n) => n.endsWith("jig.exe")), /not found inside the zip/);
});

test("throws on a non-zip buffer", () => {
  assert.throws(() => extractEntry(Buffer.from("not a zip at all"), () => true), /no EOCD/);
});
