"use strict";

const { test } = require("node:test");
const assert = require("node:assert");
const { detectTarget } = require("../lib/platform");

test("platform -> target mapping covers all four release targets", () => {
  assert.deepStrictEqual(detectTarget("win32", "x64"), {
    target: "x86_64-pc-windows-msvc",
    ext: "zip",
    binName: "jig.exe",
  });
  assert.deepStrictEqual(detectTarget("darwin", "x64"), {
    target: "x86_64-apple-darwin",
    ext: "tar.gz",
    binName: "jig",
  });
  assert.deepStrictEqual(detectTarget("darwin", "arm64"), {
    target: "aarch64-apple-darwin",
    ext: "tar.gz",
    binName: "jig",
  });
  assert.deepStrictEqual(detectTarget("linux", "x64"), {
    target: "x86_64-unknown-linux-musl",
    ext: "tar.gz",
    binName: "jig",
  });
});

test("unsupported arch throws a clear, actionable error", () => {
  assert.throws(() => detectTarget("linux", "arm64"), (e) => {
    assert.match(e.message, /unsupported platform "linux\/arm64"/);
    assert.match(e.message, /cargo install/);
    assert.match(e.message, /Windows x64/); // lists the supported set
    return true;
  });
});

test("unsupported platform throws", () => {
  assert.throws(() => detectTarget("freebsd", "x64"), /unsupported platform "freebsd\/x64"/);
  assert.throws(() => detectTarget("win32", "arm64"), /unsupported platform "win32\/arm64"/);
});
