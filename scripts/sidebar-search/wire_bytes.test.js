// The byte accounting of wire_bytes.js on synthetic CDP event streams: a finished request whose body a window cuts, an
// aborted request with bytes before its abort, an encoded aborted request, and a headers-only request.
'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('events');
const { recordNetwork, wireBytesIn, wireTotal, wireChunks } = require('./wire_bytes');

const categorize = () => 'test';

function startRequest(cdp, id) {
  cdp.emit('Network.requestWillBeSent', { requestId: id, request: { url: `http://127.0.0.1/${id}` } });
}

test('a finished request splits Chrome total over its chunks, and a window cutting the body reads its share', (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: 0 });
  const cdp = new EventEmitter();
  const list = recordNetwork(cdp, categorize);
  startRequest(cdp, 'a');
  t.mock.timers.tick(100);
  cdp.emit('Network.responseReceived', { requestId: 'a', response: { headers: { 'Content-Encoding': 'gzip' } } });
  // Chrome's per-chunk encodedDataLength is partial here, so the split must not read it.
  cdp.emit('Network.dataReceived', { requestId: 'a', dataLength: 1000, encodedDataLength: 0 });
  t.mock.timers.tick(100);
  cdp.emit('Network.dataReceived', { requestId: 'a', dataLength: 3000, encodedDataLength: 999 });
  t.mock.timers.tick(50);
  cdp.emit('Network.loadingFinished', { requestId: 'a', encodedDataLength: 800 });
  const [record] = list;

  assert.equal(record.outcome, 'finished');
  assert.equal(record.contentEncoding, 'gzip');
  assert.equal(wireTotal(record), 800);
  assert.deepEqual(wireChunks(record), [{ wall: 100, bytes: 200 }, { wall: 200, bytes: 600 }]);
  // The window (150, 250] cuts the request: it holds the second chunk only.
  assert.equal(wireBytesIn(record, 150, 250), 600);
  assert.equal(wireBytesIn(record, -Infinity, 150) + wireBytesIn(record, 150, 250) + wireBytesIn(record, 250), 800);
});

test('an aborted request counts the decoded bytes it received before the abort', (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: 0 });
  const cdp = new EventEmitter();
  const list = recordNetwork(cdp, categorize);
  startRequest(cdp, 'b');
  t.mock.timers.tick(120);
  cdp.emit('Network.responseReceived', { requestId: 'b', response: { headers: {} } });
  cdp.emit('Network.dataReceived', { requestId: 'b', dataLength: 40, encodedDataLength: 0 });
  t.mock.timers.tick(10);
  cdp.emit('Network.dataReceived', { requestId: 'b', dataLength: 10, encodedDataLength: 0 });
  cdp.emit('Network.loadingFailed', { requestId: 'b', canceled: true });
  const [record] = list;

  assert.equal(record.outcome, 'aborted');
  assert.equal(record.chromeTotal, null);
  assert.equal(record.contentEncoding, null);
  assert.equal(wireTotal(record), 50);
  assert.equal(wireBytesIn(record, -Infinity, 125), 40);
  assert.equal(wireBytesIn(record, 125), 10);
});

test('an aborted encoded request keeps its content-encoding, so its decoded count reads as an upper bound', (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: 0 });
  const cdp = new EventEmitter();
  const list = recordNetwork(cdp, categorize);
  startRequest(cdp, 'c');
  cdp.emit('Network.responseReceived', { requestId: 'c', response: { headers: { 'content-encoding': 'gzip' } } });
  t.mock.timers.tick(30);
  cdp.emit('Network.dataReceived', { requestId: 'c', dataLength: 500, encodedDataLength: 0 });
  cdp.emit('Network.loadingFailed', { requestId: 'c', canceled: true });
  const [record] = list;

  assert.equal(record.outcome, 'aborted');
  assert.equal(record.contentEncoding, 'gzip');
  assert.equal(wireTotal(record), 500);
});

test('a finished request with no body puts its whole total at its end, and a pending one counts what it has', (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: 0 });
  const cdp = new EventEmitter();
  const list = recordNetwork(cdp, categorize);
  startRequest(cdp, 'd');
  startRequest(cdp, 'e');
  t.mock.timers.tick(40);
  cdp.emit('Network.loadingFinished', { requestId: 'd', encodedDataLength: 150 });
  cdp.emit('Network.dataReceived', { requestId: 'e', dataLength: 70, encodedDataLength: 0 });
  const [empty, pending] = list;

  assert.equal(empty.outcome, 'finished');
  assert.equal(wireBytesIn(empty, 0, 39), 0);
  assert.equal(wireBytesIn(empty, 39, 40), 150);
  assert.equal(pending.outcome, 'pending');
  assert.equal(wireTotal(pending), 70);
});
