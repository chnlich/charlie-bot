const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const TERMINAL_B64_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'terminal_b64.js'),
  'utf8'
);

function loadHelpers() {
  const context = {atob, btoa, TextEncoder};
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(TERMINAL_B64_JS, context, {filename: 'terminal_b64.js'});
  return context;
}

test('encodeBytesB64 encodes a string as UTF-8 bytes then base64', () => {
  const {encodeBytesB64} = loadHelpers();
  assert.equal(encodeBytesB64('ls\n'), 'bHMK');
  assert.equal(encodeBytesB64('héllo → 世界'), Buffer.from('héllo → 世界', 'utf8').toString('base64'));
});

test('encodeBytesB64 accepts a byte array unchanged', () => {
  const {encodeBytesB64} = loadHelpers();
  const bytes = new TextEncoder().encode('héllo → 世界');
  assert.equal(encodeBytesB64(bytes), Buffer.from(bytes).toString('base64'));
});

test('encodeBytesB64 crosses the 0x8000 apply() chunk boundary', () => {
  const {encodeBytesB64} = loadHelpers();
  const bytes = new Uint8Array(0x8000 + 17);
  for (let i = 0; i < bytes.length; i++) bytes[i] = i % 251;
  assert.equal(encodeBytesB64(bytes), Buffer.from(bytes).toString('base64'));
});

test('decodeB64ToBytes round-trips encodeBytesB64 output', () => {
  const {encodeBytesB64, decodeB64ToBytes} = loadHelpers();
  const bytes = new TextEncoder().encode('héllo → 世界\n');
  const decoded = decodeB64ToBytes(encodeBytesB64(bytes));
  // Same-realm instanceof fails on vm contexts (own intrinsics); isView is cross-realm.
  assert.ok(ArrayBuffer.isView(decoded));
  assert.deepEqual([...decoded], [...bytes]);
});
