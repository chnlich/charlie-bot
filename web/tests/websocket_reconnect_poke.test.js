// ---------------------------------------------------------------------------
// The reconnect poke: after a socket disconnect, the first successful request
// of any kind reconnects the socket at once and resets its backoff, instead of
// leaving it to wait the backoff delay out (websocket.js).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

function buildContext() {
  const timers = [];
  const cleared = [];
  let fetchResponse = {ok: true, status: 200};
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: (id) => { cleared.push(id); },
    fetch: () => Promise.resolve(fetchResponse),
  });
  return {context, FakeWebSocket, timers, cleared, setFetchResponse: (value) => { fetchResponse = value; }};
}

function connectedThenClosed({context, FakeWebSocket, timers}) {
  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  socket.emitClose({code: 1006});
  assert.equal(timers.length, 1, 'the close schedules its reconnect backoff');
  return socket;
}

test('the first successful request after a disconnect reconnects the socket at once', () => {
  const h = buildContext();
  connectedThenClosed(h);

  return h.context.fetch('/api/sessions/status?ids=').then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 2, 'the poke reconnected without waiting the delay out');
    assert.equal(h.context.reconnectDelay, 1000, 'the backoff reset');
    assert.deepEqual(h.cleared, [1], 'the pending backoff timer was cancelled');
  });
});

test('a failed request does not poke the reconnect', () => {
  const h = buildContext();
  h.setFetchResponse(Promise.reject(new Error('link down')));
  connectedThenClosed(h);

  return h.context.fetch('/api/sessions/status?ids=').catch(() => {}).then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 1, 'the backoff timer still owns the reconnect');
    assert.deepEqual(h.cleared, []);
  });
});

test('a refused response does not poke the reconnect', () => {
  const h = buildContext();
  h.setFetchResponse({ok: false, status: 503});
  connectedThenClosed(h);

  return h.context.fetch('/api/sessions/status?ids=').then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 1, 'the backoff timer still owns the reconnect');
    assert.deepEqual(h.cleared, []);
  });
});

test('a success with no pending reconnect does nothing', () => {
  const h = buildContext();
  connectedThenClosed(h);
  h.timers[0].fn();  // the backoff fires: connectWS runs on its own
  assert.equal(h.FakeWebSocket.instances.length, 2);

  return h.context.fetch('/api/sessions/status?ids=').then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 2, 'no second socket without a pending timer');
  });
});
