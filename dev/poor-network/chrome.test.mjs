// Tests of the browser teardown: closing a browser fails its unanswered DevTools calls and destroys the pipe streams,
// so a dead browser leaves the bench no handle to wait on.

import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import { test } from 'node:test';

import { CdpConnection } from './chrome.mjs';

test('closing the connection rejects the pending call and destroys both pipe streams', async () => {
  const readable = new PassThrough();
  const writable = new PassThrough();
  const connection = new CdpConnection(writable, readable);
  const pending = connection.send('Browser.getVersion');
  connection.close();
  await assert.rejects(pending, /DevTools pipe was closed/);
  assert.equal(writable.destroyed, true);
  assert.equal(readable.destroyed, true);
});

test('closing the connection stops feeding the listener', async () => {
  const readable = new PassThrough();
  const writable = new PassThrough();
  const connection = new CdpConnection(writable, readable);
  const seen = [];
  connection.onMessage((message) => seen.push(message));
  connection.close();
  readable.write(`${JSON.stringify({ method: 'x' })}\0`);
  assert.deepEqual(seen, []);
});
