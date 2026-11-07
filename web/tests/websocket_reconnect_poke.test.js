// ---------------------------------------------------------------------------
// Reconnect pokes and the quick retry (websocket.js). After a socket disconnect,
// the first successful request of any kind reconnects the socket at once and
// resets its backoff. A visible page also retries a closed socket every quick
// retry delay, a capped number of times, instead of waiting the backoff out, so
// an idle page — whose own polls may be seconds away — asks for a new socket a
// round trip after the link returns.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

function buildContext() {
  const timers = [];
  const cleared = [];
  let fetchResponse = {ok: true, status: 200};
  const fetches = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: (id) => { cleared.push(id); },
    fetch: (url) => {
      fetches.push(url);
      return Promise.resolve(fetchResponse);
    },
  });
  return {context, FakeWebSocket, timers, cleared, fetches, setFetchResponse: (value) => { fetchResponse = value; }};
}

function connectedThenClosed({context, FakeWebSocket, timers}) {
  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  socket.emitClose({code: 1006});
  assert.deepEqual(timers.map((timer) => timer.ms), [1000, 250], 'the close schedules its backoff and quick retry');
  return socket;
}

function quickRetryIndex(timers) {
  return timers.findIndex((timer) => timer.ms === 250);
}

test('the first successful request after a disconnect reconnects the socket at once', () => {
  const h = buildContext();
  connectedThenClosed(h);

  return h.context.fetch('/api/sessions/status?ids=').then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 2, 'the poke reconnected without waiting the delay out');
    assert.equal(h.context.reconnectDelay, 1000, 'the backoff reset');
    assert.deepEqual(h.cleared, [1, 2], 'the pending backoff and quick retry timers were cancelled');
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

test('an idle page asks for a new socket after the quick retry, with no poll', () => {
  // A reset blackout surfaces its socket close at the restore; the page has no poll due then.
  const h = buildContext();
  connectedThenClosed(h);
  const quickIndex = quickRetryIndex(h.timers);

  assert.ok(quickIndex >= 0, 'the close scheduled a quick retry');
  assert.equal(h.timers[quickIndex].ms, 250, 'the retry follows the close by a quarter second');
  assert.equal(h.fetches.length, 0, 'no request of the page is in flight');
  h.timers[quickIndex].fn();
  assert.equal(h.FakeWebSocket.instances.length, 2, 'the quick retry connects without waiting the backoff out');
  assert.equal(h.fetches.length, 0, 'the retry issued no poll');
});

test('a failed retry schedules the next one', () => {
  const h = buildContext();
  connectedThenClosed(h);
  h.timers[quickRetryIndex(h.timers)].fn();

  h.FakeWebSocket.instances[1].emitClose({code: 1006});
  assert.equal(h.timers[h.timers.length - 1].ms, 250, 'the retry cadence holds while the link is down');
});

test('the quick retry is capped, and the backoff then owns a refused attempt', () => {
  // A down server refuses every attempt at once: each close schedules the next retry.
  const h = buildContext();
  connectedThenClosed(h);
  for (let attempt = 1; attempt < 5; attempt++) {
    h.timers[h.timers.length - 1].fn();
    h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1].emitClose({code: 1006});
  }
  const scheduledBefore = h.timers.length;
  h.timers[h.timers.length - 1].fn();
  h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1].emitClose({code: 1006});
  assert.deepEqual(h.timers.slice(scheduledBefore).map((timer) => timer.ms), [30000], 'only the backoff is left');
});

test('an open socket restores the quick retry', () => {
  const h = buildContext();
  connectedThenClosed(h);
  for (let attempt = 1; attempt < 6; attempt++) {
    h.timers[h.timers.length - 1].fn();
    h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1].emitClose({code: 1006});
  }
  h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1].emitOpen();
  h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1].emitClose({code: 1006});
  assert.equal(h.timers[h.timers.length - 1].ms, 250, 'the reconnect after an open retries quickly again');
});

test('a hidden page keeps the exponential backoff', () => {
  const h = buildContext();
  connectedThenClosed(h);
  h.context.document.visibilityState = 'hidden';

  h.timers[quickRetryIndex(h.timers)].fn();
  h.FakeWebSocket.instances[1].emitClose({code: 1006});
  assert.equal(h.timers.filter((timer) => timer.ms === 250).length, 1, 'no new quick retry from a hidden tab');
});

test('a poke cancels the pending quick retry', () => {
  const h = buildContext();
  connectedThenClosed(h);
  const quickTimerId = quickRetryIndex(h.timers) + 1;

  return h.context.fetch('/api/sessions/status?ids=').then(() => {
    assert.equal(h.FakeWebSocket.instances.length, 2, 'the poke reconnected first');
    assert.ok(h.cleared.includes(quickTimerId), 'the quick retry timer was cleared');
  });
});

test('a switch teardown clears the quick retry', () => {
  const h = buildContext();
  connectedThenClosed(h);
  const quickTimerId = quickRetryIndex(h.timers) + 1;

  h.context.cancelReconnect();
  assert.ok(h.cleared.includes(quickTimerId), 'the quick retry timer was cleared');
});
