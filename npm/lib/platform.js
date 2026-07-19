"use strict";

// Maps a Node.js (process.platform, process.arch) pair to the Rust release
// target, the archive extension, and the binary file name shipped inside the
// archive. These four release targets mirror the matrix in
// `.github/workflows/release.yml` exactly — keep them in sync.
//
// Pure and dependency-free so it can be unit-tested against every supported
// pair without touching the real host (see test/platform.test.js).

// platform -> arch -> descriptor
const TARGETS = {
  win32: {
    x64: { target: "x86_64-pc-windows-msvc", ext: "zip", binName: "jig.exe" },
  },
  darwin: {
    x64: { target: "x86_64-apple-darwin", ext: "tar.gz", binName: "jig" },
    arm64: { target: "aarch64-apple-darwin", ext: "tar.gz", binName: "jig" },
  },
  linux: {
    x64: { target: "x86_64-unknown-linux-musl", ext: "tar.gz", binName: "jig" },
  },
};

// A human-readable list of what we DO support, for the error message.
const SUPPORTED = [
  "Windows x64 (win32/x64)",
  "macOS Intel (darwin/x64)",
  "macOS Apple Silicon (darwin/arm64)",
  "Linux x64 (linux/x64)",
];

/**
 * Resolve a release descriptor for the given platform/arch.
 * @param {string} platform - e.g. process.platform ("win32" | "darwin" | "linux")
 * @param {string} arch - e.g. process.arch ("x64" | "arm64")
 * @returns {{target: string, ext: string, binName: string}}
 * @throws {Error} with a clear, actionable message on an unsupported pair.
 */
function detectTarget(platform, arch) {
  const byArch = TARGETS[platform];
  const descriptor = byArch && byArch[arch];
  if (!descriptor) {
    throw new Error(
      `@shodh/jig: unsupported platform "${platform}/${arch}".\n` +
        `Prebuilt binaries are published for:\n` +
        SUPPORTED.map((s) => `  - ${s}`).join("\n") +
        `\n\nIf you have a Rust toolchain you can build from source instead:\n` +
        `  cargo install --git https://github.com/Shodh-Labs/jig jig-cli\n`
    );
  }
  return descriptor;
}

module.exports = { detectTarget, TARGETS, SUPPORTED };
