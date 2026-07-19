"use strict";

// A minimal, dependency-free ZIP reader that extracts a SINGLE entry from a zip
// held entirely in memory.
//
// Why parse the zip ourselves instead of shelling out?
//   * Node ships `zlib` (deflate) but has no zip reader — a zip needs its
//     central-directory / local-header framing parsed by hand.
//   * Shelling out to `tar -xf` (bsdtar on Win10+) or `Expand-Archive` works,
//     but which one is on PATH varies by machine (this repo's dev box runs
//     GNU tar via Git Bash, which does NOT read zips), and spawning a shell
//     during postinstall is a portability/robustness risk we can avoid.
//   * Our archives are tiny (a single binary + LICENSE + README), so reading
//     the whole zip into memory and pulling out one member is cheap and — most
//     importantly — the SAME verified bytes we checksummed never touch disk
//     unverified. This is the money property for a security-sensitive install.
//
// Supports the only two compression methods a release archive can use:
//   0 = stored (no compression), 8 = deflate (zlib.inflateRawSync).

const zlib = require("zlib");

const EOCD_SIG = 0x06054b50; // End Of Central Directory
const CDH_SIG = 0x02014b50; // Central Directory Header
const LFH_SIG = 0x04034b50; // Local File Header

/** Locate the End Of Central Directory record, scanning backward. */
function findEocd(buf) {
  // EOCD is >= 22 bytes; the trailing comment (usually empty) can be up to
  // 65535 bytes, so scan the last (22 + 65535) bytes at most.
  const minStart = Math.max(0, buf.length - (22 + 0xffff));
  for (let i = buf.length - 22; i >= minStart; i--) {
    if (buf.readUInt32LE(i) === EOCD_SIG) return i;
  }
  throw new Error("@shodh/jig: not a valid zip archive (no EOCD record found)");
}

/**
 * Extract the first entry whose name satisfies `predicate(name)` from a zip
 * buffer. Returns the raw uncompressed bytes of that entry.
 * @param {Buffer} buf - the entire zip file.
 * @param {(name: string) => boolean} predicate
 * @returns {Buffer}
 */
function extractEntry(buf, predicate) {
  const eocd = findEocd(buf);
  const entryCount = buf.readUInt16LE(eocd + 10);
  let ptr = buf.readUInt32LE(eocd + 16); // start of central directory

  for (let n = 0; n < entryCount; n++) {
    if (buf.readUInt32LE(ptr) !== CDH_SIG) {
      throw new Error("@shodh/jig: corrupt zip (bad central-directory signature)");
    }
    const method = buf.readUInt16LE(ptr + 10);
    const compSize = buf.readUInt32LE(ptr + 20);
    const uncompSize = buf.readUInt32LE(ptr + 24);
    const nameLen = buf.readUInt16LE(ptr + 28);
    const extraLen = buf.readUInt16LE(ptr + 30);
    const commentLen = buf.readUInt16LE(ptr + 32);
    const localOffset = buf.readUInt32LE(ptr + 42);
    const name = buf.toString("utf8", ptr + 46, ptr + 46 + nameLen);

    if (predicate(name)) {
      return readLocalEntry(buf, localOffset, method, compSize, uncompSize, name);
    }
    ptr += 46 + nameLen + extraLen + commentLen;
  }
  throw new Error("@shodh/jig: expected binary entry not found inside the zip archive");
}

/** Read + decompress a single entry given its central-directory metadata. */
function readLocalEntry(buf, localOffset, method, compSize, uncompSize, name) {
  if (buf.readUInt32LE(localOffset) !== LFH_SIG) {
    throw new Error(`@shodh/jig: corrupt zip (bad local header for "${name}")`);
  }
  // The local header repeats name/extra lengths, which can differ from the
  // central directory's, so read them here to find where the data begins.
  const lfhNameLen = buf.readUInt16LE(localOffset + 26);
  const lfhExtraLen = buf.readUInt16LE(localOffset + 28);
  const dataStart = localOffset + 30 + lfhNameLen + lfhExtraLen;
  const compressed = buf.subarray(dataStart, dataStart + compSize);

  let out;
  if (method === 0) {
    out = Buffer.from(compressed); // stored
  } else if (method === 8) {
    out = zlib.inflateRawSync(compressed); // deflate
  } else {
    throw new Error(`@shodh/jig: unsupported zip compression method ${method} for "${name}"`);
  }

  if (out.length !== uncompSize) {
    throw new Error(
      `@shodh/jig: zip entry "${name}" decompressed to ${out.length} bytes, expected ${uncompSize}`
    );
  }
  return out;
}

module.exports = { extractEntry };
