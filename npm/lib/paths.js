"use strict";

// Where the installed binary and its version stamp live inside the package.
// Shared by the installer (writes) and the bin shim (reads) so the location is
// defined in exactly one place.

const path = require("path");

// Package root = the directory containing package.json (one level up from lib/).
const PACKAGE_ROOT = path.join(__dirname, "..");
const VENDOR_DIR = path.join(PACKAGE_ROOT, "vendor");

/** Absolute path to the installed binary for this host. */
function binaryPath(platform = process.platform) {
  const name = platform === "win32" ? "jig.exe" : "jig";
  return path.join(VENDOR_DIR, name);
}

/** Absolute path to the version stamp written after a successful install. */
function stampPath() {
  return path.join(VENDOR_DIR, ".jig-version");
}

module.exports = { PACKAGE_ROOT, VENDOR_DIR, binaryPath, stampPath };
