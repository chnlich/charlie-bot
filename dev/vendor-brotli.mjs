#!/usr/bin/env node
// Writes a brotli copy beside each file under web/static/vendor whose copy is smaller, so the static
// file route can serve the CDN's wire form for the files the page took from it. Run from the repo
// root after re-vendoring a file; the copies are committed with their originals.
//
//   node dev/vendor-brotli.mjs

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const vendorRoot = path.join(import.meta.dirname, '..', 'web', 'static', 'vendor');
// A copy worth carrying shrinks the file by at least five percent: already-compressed forms (fonts)
// gain nothing and their copies would only rot beside them.
const SHRINKS_BY = 0.95;

for (const entry of fs.readdirSync(vendorRoot, { withFileTypes: true, recursive: true })) {
  if (!entry.isFile() || entry.name.endsWith('.br')) continue;
  const file = path.join(entry.parentpath ?? entry.parentPath ?? vendorRoot, entry.name);
  const original = fs.readFileSync(file);
  const compressed = zlib.brotliCompressSync(original);
  if (compressed.length > original.length * SHRINKS_BY) {
    console.log(`skip ${file}: brotli saves ${original.length - compressed.length}B of ${original.length}B`);
    continue;
  }
  fs.writeFileSync(file + '.br', compressed);
  console.log(`wrote ${file}.br (${original.length}B -> ${compressed.length}B)`);
}
