// Tests of the improve-ux WebSocket write guard. The guard's source runs in a fresh VM context whose WebSocket is a
// subclass of a fake socket, so each case patches its own prototype and sees only its own frames. A test file's name
// ends in .test.mjs, so the bench-lock naming check in dev/poor-network/lock.integration.test.mjs reads it as a test.

import assert from 'node:assert/strict';
import vm from 'node:vm';
import { test } from 'node:test';

import { installWriteGuard, writeGuard } from './ux-frame-guard.mjs';

// A socket that records each frame the page sends to the server, and runs the page's handlers for the server's frames.
class FakeSocket {
  constructor(url) {
    this.url = url;
    this.wire = [];
    this.onmessage = null;
  }

  send(data) {
    this.wire.push(data);
  }

  // The server's frame, delivered as the browser delivers it: to onmessage.
  deliver(data) {
    this.onmessage?.({ data });
  }
}

// The guard's records are arrays and objects of the page's realm, so a case compares their plain copies.
function plain(value) {
  return JSON.parse(JSON.stringify(value));
}

// A page with the guard installed, as the driver installs it before the first navigation. The page's WebSocket is a
// subclass, so the guard's send replaces the subclass's method and no other case's.
function openPage() {
  const WebSocket = class extends FakeSocket {};
  const context = vm.createContext({ WebSocket });
  vm.runInContext(`(${writeGuard})();`, context);
  return { context, WebSocket };
}

test('the guard drops terminal input and resize frames and records them', () => {
  const { context, WebSocket } = openPage();
  const socket = new WebSocket('ws://127.0.0.1:18650/ws/terminal?cols=120&rows=40');
  socket.send(JSON.stringify({ type: 'pty_input', data: 'bHMK' }));
  socket.send(JSON.stringify({ type: 'pty_resize', cols: 100, rows: 30 }));
  assert.deepEqual(socket.wire, [], 'no terminal frame reaches the server');
  assert.deepEqual(plain(context.__uxWriteGuard.dropped), [
    { path: '/ws/terminal', type: 'pty_input' },
    { path: '/ws/terminal', type: 'pty_resize' },
  ]);
});

test('the guard drops chat input on a session socket and passes the subscribe frame', () => {
  const { context, WebSocket } = openPage();
  const socket = new WebSocket('wss://host.example/ws/sessions/2a49cdcf?cursor=0&asset_version=1');
  socket.send('{"type":"subscribe","session_id":"2a49cdcf","cursor":0,"subscription":"chat"}');
  socket.send('{"type":"message","content":"a chat line"}');
  assert.deepEqual(socket.wire, ['{"type":"subscribe","session_id":"2a49cdcf","cursor":0,"subscription":"chat"}']);
  assert.deepEqual(plain(context.__uxWriteGuard.dropped), [{ path: '/ws/sessions/2a49cdcf', type: 'message' }]);
});

test('the guard drops voice frames, text and binary alike', () => {
  const { context, WebSocket } = openPage();
  const socket = new WebSocket('ws://127.0.0.1:18650/ws/voice/transcribe');
  socket.send(new Uint8Array([1, 2, 3]));
  socket.send(JSON.stringify({ type: 'end', devices: [] }));
  assert.deepEqual(socket.wire, []);
  assert.deepEqual(plain(context.__uxWriteGuard.dropped), [
    { path: '/ws/voice/transcribe', type: 'binary' },
    { path: '/ws/voice/transcribe', type: 'end' },
  ]);
});

test('the guard drops a frame on a socket it does not name, a subscribe-looking frame included', () => {
  const { context, WebSocket } = openPage();
  const socket = new WebSocket('ws://127.0.0.1:18650/ws/other');
  socket.send('{"type":"subscribe"}');
  assert.deepEqual(socket.wire, []);
  assert.equal(context.__uxWriteGuard.dropped.length, 1);
});

test('reads keep flowing on a socket whose writes are dropped', () => {
  const { WebSocket } = openPage();
  const terminal = new WebSocket('ws://127.0.0.1:18650/ws/terminal');
  const received = [];
  terminal.onmessage = (event) => received.push(event.data);
  terminal.send(JSON.stringify({ type: 'pty_input', data: 'bHMK' }));
  terminal.deliver('{"type":"pty_output","data":"b3V0cHV0"}');
  terminal.deliver('{"type":"pty_exit"}');
  assert.deepEqual(received, ['{"type":"pty_output","data":"b3V0cHV0"}', '{"type":"pty_exit"}']);
});

test('the guard keeps the socket class and its read handler, and only replaces send', () => {
  const { WebSocket } = openPage();
  const session = new WebSocket('ws://127.0.0.1:18650/ws/sessions/s1');
  assert.ok(session instanceof WebSocket, 'the socket keeps its class');
  assert.equal(session.onmessage, null, 'the guard installs no read handler');
  assert.equal(Object.keys(WebSocket.prototype).join(), 'send', 'the guard defines one method on the class');
});

test('the driver installs the guard as a page script before the first navigation', async () => {
  const calls = [];
  await installWriteGuard(async (method, params) => {
    calls.push({ method, params });
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, 'Page.addScriptToEvaluateOnNewDocument');
  assert.match(calls[0].params.source, /WebSocket\.prototype\.send/);
});
