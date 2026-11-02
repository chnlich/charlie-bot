const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

const BANNER_TEXT = 'This page is out of date. Reload to continue.';
const RELOAD_GUARD_KEY = 'charliebot-stale-page-reload-at';

// One build with an injected clock (context.Date.now), a sessionStorage stub,
// a reload-recording location and a timer list. clock.now moves the fake time.
function buildHarness(clock) {
  const reloaded = [];
  const timers = [];
  const bodyChildren = [];
  const sessionStorage = {
    getItem: (key) => (sessionStorage.store.has(key) ? sessionStorage.store.get(key) : null),
    setItem: (key, value) => sessionStorage.store.set(key, value),
    store: new Map(),
  };
  const document = {
    getElementById: () => null,
    createElement: () => ({
      style: {cssText: ''},
      textContent: '',
      children: [],
      appendChild(child) { this.children.push(child); },
    }),
    body: {appendChild(child) { bodyChildren.push(child); }},
  };
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    STATIC_ASSET_VERSION: 'v1/x y',
    Date: {now: () => clock.now},
    sessionStorage,
    location: {protocol: 'http:', host: 'localhost:8000', reload: () => reloaded.push(clock.now)},
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: () => {},
    document,
  });
  return {context, FakeWebSocket, reloaded, timers, sessionStorage, bodyChildren};
}

test('the handshake URL carries the encoded asset_version', () => {
  const clock = {now: 1000};
  const {context, FakeWebSocket} = buildHarness(clock);

  context.connectWS();

  assert.match(FakeWebSocket.instances[0].url, /asset_version=v1%2Fx%20y$/);
});

test('the first 4409 reloads once, stores the time, and schedules no reconnect', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 4409});

  assert.deepEqual(reloaded, [5000]);
  assert.equal(sessionStorage.getItem(RELOAD_GUARD_KEY), '5000');
  assert.equal(timers.length, 0);
});

test('a second 4409 within 60 seconds shows the banner and neither reloads nor reconnects', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, timers, bodyChildren} = buildHarness(clock);

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitClose({code: 4409});
  clock.now = 30000;
  socket.emitClose({code: 4409});

  assert.deepEqual(reloaded, [5000], 'the guarded close must not reload again');
  assert.equal(timers.length, 0, 'the guarded close must schedule no reconnect');
  assert.equal(bodyChildren.length, 1);
  assert.equal(bodyChildren[0].textContent, BANNER_TEXT);
  assert.equal(bodyChildren[0].children.length, 1);
  assert.equal(bodyChildren[0].children[0].textContent, 'Reload');
  // The Reload button reloads by hand, outside the guard.
  bodyChildren[0].children[0].onclick();
  assert.deepEqual(reloaded, [5000, 30000]);
});

test('a 4409 at least 60 seconds after the stored time reloads again', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitClose({code: 4409});
  clock.now = 65000;
  socket.emitClose({code: 4409});

  assert.deepEqual(reloaded, [5000, 65000]);
  assert.equal(sessionStorage.getItem(RELOAD_GUARD_KEY), '65000');
  assert.equal(timers.length, 0);
});

test('a close with another code schedules the reconnect as before', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1006});

  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 1000);
  assert.deepEqual(reloaded, []);
  assert.equal(sessionStorage.store.size, 0, 'a plain close stores no reload time');
});
