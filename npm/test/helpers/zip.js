"use strict";

// A tiny, dependency-free ZIP *writer* used only by the tests to build fixture
// archives in memory (the installer never writes zips — it only reads them).
// Supports stored (0) and deflate (8) so both code paths in lib/unzip.js are
// exercised. Uses zlib.crc32 (Node >= 22).

const zlib = require("zlib");

/**
 * Build a zip Buffer from entries.
 * @param {Array<{name: string, data: Buffer, method?: 0|8}>} entries
 * @returns {Buffer}
 */
function buildZip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;

  for (const e of entries) {
    const method = e.method === undefined ? 8 : e.method;
    const data = Buffer.isBuffer(e.data) ? e.data : Buffer.from(e.data);
    const crc = zlib.crc32(data) >>> 0;
    const compressed = method === 8 ? zlib.deflateRawSync(data) : data;
    const nameBuf = Buffer.from(e.name, "utf8");

    const lfh = Buffer.alloc(30);
    lfh.writeUInt32LE(0x04034b50, 0);
    lfh.writeUInt16LE(20, 4);
    lfh.writeUInt16LE(0, 6);
    lfh.writeUInt16LE(method, 8);
    lfh.writeUInt16LE(0, 10);
    lfh.writeUInt16LE(0, 12);
    lfh.writeUInt32LE(crc, 14);
    lfh.writeUInt32LE(compressed.length, 18);
    lfh.writeUInt32LE(data.length, 22);
    lfh.writeUInt16LE(nameBuf.length, 26);
    lfh.writeUInt16LE(0, 28);

    const localRecord = Buffer.concat([lfh, nameBuf, compressed]);
    locals.push(localRecord);

    const cdh = Buffer.alloc(46);
    cdh.writeUInt32LE(0x02014b50, 0);
    cdh.writeUInt16LE(20, 4);
    cdh.writeUInt16LE(20, 6);
    cdh.writeUInt16LE(0, 8);
    cdh.writeUInt16LE(method, 10);
    cdh.writeUInt16LE(0, 12);
    cdh.writeUInt16LE(0, 14);
    cdh.writeUInt32LE(crc, 16);
    cdh.writeUInt32LE(compressed.length, 20);
    cdh.writeUInt32LE(data.length, 24);
    cdh.writeUInt16LE(nameBuf.length, 28);
    cdh.writeUInt16LE(0, 30);
    cdh.writeUInt16LE(0, 32);
    cdh.writeUInt16LE(0, 34);
    cdh.writeUInt16LE(0, 36);
    cdh.writeUInt32LE(0, 38);
    cdh.writeUInt32LE(offset, 42);
    centrals.push(Buffer.concat([cdh, nameBuf]));

    offset += localRecord.length;
  }

  const centralDir = Buffer.concat(centrals);
  const localsBuf = Buffer.concat(locals);

  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(0, 4);
  eocd.writeUInt16LE(0, 6);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(centralDir.length, 12);
  eocd.writeUInt32LE(localsBuf.length, 16);
  eocd.writeUInt16LE(0, 20);

  return Buffer.concat([localsBuf, centralDir, eocd]);
}

module.exports = { buildZip };
