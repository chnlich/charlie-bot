const assert = require('node:assert/strict');
const test = require('node:test');

const {loadWebsocketContext, CONNECT_DEADLINE_MS} = require('./websocket_context_stub');

const BANNER_TEXT = 'This page is out of date. Reload to continue.';
const RELOAD_GUARD_KEY = 'charliebot-stale-page-reload-at';
const RESTART_RETRY_MS = 500;
const RESTART_WINDOW_MS = 60000;

// One build with an injected clock (context.Date.now), a sessionStorage stub, a reload-recording location and a timer
// list. clock.now moves the fake time; document.hidden is the tab's visibility. The reload helpers stand in for
// offline-shell.js. reloadPastStoredShell (a visible page) navigates at once, and records the cause the page dispatched.
// reloadAfterRefreshingShell (a hidden page) waits in `pendingRefreshes` until the test settles it, as the shell refresh
// does. Each record names its path: 'visible' or 'refresh'. A build may share another build's sessionStorage, which is
// how a page a reload opened inherits the guard.
function buildHarness(clock, {hidden = false, sessionStorage: storage = null} = {}) {
  const reloaded = [];
  const pendingRefreshes = [];
  const timers = [];
  const bodyChildren = [];
  const documentListeners = new Map();
  let lastCause = null;
  const sessionStorage = storage ?? {
    getItem: (key) => (sessionStorage.store.has(key) ? sessionStorage.store.get(key) : null),
    setItem: (key, value) => sessionStorage.store.set(key, value),
    store: new Map(),
  };
  const document = {
    hidden,
    get visibilityState() { return this.hidden ? 'hidden' : 'visible'; },
    getElementById: () => null,
    addEventListener: (type, listener) => {
      const listeners = documentListeners.get(type) || [];
      listeners.push(listener);
      documentListeners.set(type, listeners);
    },
    dispatchEvent: (event) => {
      for (const listener of documentListeners.get(event.type) || []) listener(event);
      return true;
    },
    createElement: () => ({
      style: {cssText: ''},
      textContent: '',
      children: [],
      appendChild(child) { this.children.push(child); },
    }),
    body: {appendChild(child) { bodyChildren.push(child); }},
  };
  const reloadPastStoredShell = () => reloaded.push({cause: lastCause, at: clock.now, path: 'visible'});
  const reloadAfterRefreshingShell = (cause, beforeNavigate = () => true) => {
    pendingRefreshes.push(() => {
      if (beforeNavigate()) reloaded.push({cause, at: clock.now, path: 'refresh'});
    });
    return Promise.resolve();
  };
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    STATIC_ASSET_VERSION: 'v1/x y',
    WEB_PROTOCOL: 1,
    Date: {now: () => clock.now},
    sessionStorage,
    location: {protocol: 'http:', host: 'localhost:8000', reload: () => reloaded.push({cause: 'direct', at: clock.now})},
    reloadPastStoredShell,
    reloadAfterRefreshingShell,
    setTimeout: (fn, ms) => {
      // The connect deadline has its own tests (websocket_connect_handlers.test.js); this harness records the rest.
      if (ms === CONNECT_DEADLINE_MS) return 0;
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: () => {},
    document,
  });
  document.addEventListener('charliebot:page-load-cause', (event) => { lastCause = event.detail.cause; });
  return {context, FakeWebSocket, reloaded, pendingRefreshes, timers, sessionStorage, bodyChildren, document};
}

// Lets the settled promises of the reload helper run.
function flushPromises() {
  return new Promise((resolve) => setImmediate(resolve));
}

function settleRefreshes(pendingRefreshes) {
  for (const settle of pendingRefreshes.splice(0)) settle();
}

function catchupComplete(socket, {assetVersion = 'v1/x y', uptimeMs = 5000} = {}) {
  socket.emitMessage({
    type: 'catchup_complete',
    session_id: 'session-a',
    subscription: 0,
    event_total: 0,
    asset_version: assetVersion,
    uptime_ms: uptimeMs,
  });
}

test('the handshake URL carries the web protocol, not the asset version', () => {
  const clock = {now: 1000};
  const {context, FakeWebSocket} = buildHarness(clock);

  context.connectWS();

  assert.match(FakeWebSocket.instances[0].url, /protocol=1$/);
  assert.doesNotMatch(FakeWebSocket.instances[0].url, /asset_version/);
});

test('the first 4409 reloads a visible tab at once with the stale-asset cause, and schedules no reconnect', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 5000, path: 'visible'}]);
  assert.equal(sessionStorage.getItem(RELOAD_GUARD_KEY), '5000');
  assert.equal(timers.length, 0);
});

test('a 4409 on a hidden tab refreshes the shell and reloads at once when the guard allows', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers} = buildHarness(clock, {hidden: true});

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 5000, path: 'refresh'}]);
  assert.equal(timers.length, 0);
});

test('a second 4409 within 60 seconds shows the banner on a visible tab and neither reloads nor reconnects', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers, bodyChildren} = buildHarness(clock);

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);
  clock.now = 30000;
  socket.emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 5000, path: 'visible'}],
      'the guarded close must not reload again');
  assert.equal(timers.length, 0, 'the guarded close must schedule no reconnect');
  assert.equal(bodyChildren.length, 1);
  assert.equal(bodyChildren[0].textContent, BANNER_TEXT);
  assert.equal(bodyChildren[0].children.length, 1);
  assert.equal(bodyChildren[0].children[0].textContent, 'Reload');
  // The Reload button reloads past the stored shell, outside the guard, and names the stale-asset cause.
  bodyChildren[0].children[0].onclick();
  assert.deepEqual(reloaded.map((entry) => entry.at), [5000, 30000]);
  assert.deepEqual(reloaded.map((entry) => entry.cause), ['stale_asset', 'stale_asset']);
});

test('a second 4409 on the page a reload opened shows the banner and does not navigate again', () => {
  const clock = {now: 5000};
  const first = buildHarness(clock);
  first.context.connectWS();
  first.FakeWebSocket.instances[0].emitClose({code: 4409});
  assert.deepEqual(first.reloaded.map((entry) => entry.at), [5000]);

  clock.now = 9000;
  const reloadedPage = buildHarness(clock, {sessionStorage: first.sessionStorage});
  reloadedPage.context.connectWS();
  reloadedPage.FakeWebSocket.instances[0].emitClose({code: 4409});
  assert.deepEqual(reloadedPage.reloaded, [], 'the guard holds the reloaded page to the banner');
  assert.equal(reloadedPage.bodyChildren.length, 1);
  assert.equal(reloadedPage.bodyChildren[0].textContent, BANNER_TEXT);
});

test('a hidden tab the guard holds reloads when the window ends, if it is still hidden', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers, document} = buildHarness(clock, {hidden: true});

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);
  await flushPromises();
  clock.now = 30000;
  FakeWebSocket.instances[0].emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);

  assert.deepEqual(reloaded.map((entry) => entry.at), [5000], 'the guard holds the second reload');
  const wait = timers[timers.length - 1];
  assert.equal(wait.ms, RESTART_WINDOW_MS - 25000, 'the wait runs to the end of the window');
  clock.now = 65000;
  wait.fn();
  settleRefreshes(pendingRefreshes);
  assert.deepEqual(reloaded.map((entry) => entry.at), [5000, 65000]);
  assert.equal(document.hidden, true);
});

test('a failed shell refresh offers a retry without reloading the current page', () => {
  const clock = {now: 5000};
  const {context, reloaded, bodyChildren, document} = buildHarness(clock);

  document.dispatchEvent(new context.CustomEvent('charliebot:offline-shell-refresh-failed'));

  assert.deepEqual(reloaded, []);
  assert.equal(bodyChildren.length, 1);
  assert.equal(bodyChildren[0].children[0].textContent, 'Reload');
});

test('a 4409 at least 60 seconds after the stored time reloads again', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);
  clock.now = 65000;
  socket.emitClose({code: 4409});
  settleRefreshes(pendingRefreshes);

  assert.deepEqual(reloaded.map((entry) => entry.at), [5000, 65000]);
  assert.equal(sessionStorage.getItem(RELOAD_GUARD_KEY), '65000');
  assert.equal(timers.length, 0);
});

test('a close with another code schedules the reconnect as before', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, timers, sessionStorage} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1006});

  assert.deepEqual(timers.map((timer) => timer.ms), [1000, 250], 'the reconnect backoff, then the quick retry');
  assert.deepEqual(reloaded, []);
  assert.equal(sessionStorage.store.size, 0, 'a plain close stores no reload time');
});

test('after a 1000 close a visible tab retries every 500 ms for 60 seconds, then falls back to the quick retry', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, timers} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1000});
  assert.deepEqual(timers.map((timer) => timer.ms), [1000, RESTART_RETRY_MS]);

  // Runs the pending retry: it opens the next socket, and the timers it leaves are the next close's.
  const runPendingRetry = (ms) => {
    const retry = timers.find((timer) => timer.ms === ms);
    timers.length = 0;
    retry.fn();
  };
  runPendingRetry(RESTART_RETRY_MS);
  clock.now = 5500;
  FakeWebSocket.instances[1].emitClose({code: 1006});
  assert.deepEqual(timers.map((timer) => timer.ms), [2000, RESTART_RETRY_MS], 'the fixed retry keeps running');

  runPendingRetry(RESTART_RETRY_MS);
  clock.now = 5000 + RESTART_WINDOW_MS;
  FakeWebSocket.instances[2].emitClose({code: 1006});
  // The harness drops the 4000 ms backoff, which is the connect deadline's own length; the quick retry is what follows.
  assert.deepEqual(timers.map((timer) => timer.ms), [250], 'after the window only the quick retry follows');

  runPendingRetry(250);
  FakeWebSocket.instances[3].emitClose({code: 1006});
  assert.deepEqual(timers.map((timer) => timer.ms), [8000, 250], 'the backoff keeps doubling after the window');
});

test('a restart that the page saw from view swaps the code when a hidden catch-up names another release', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, document} = buildHarness(clock);

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1000});
  document.hidden = true;
  document.dispatchEvent({type: 'visibilitychange'});
  clock.now = 6000;
  context.connectWS();
  const socket = FakeWebSocket.instances[1];
  socket.emitOpen();
  catchupComplete(socket, {assetVersion: 'v2'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 6000, path: 'refresh'}]);
});

test('a 1000 close on a hidden tab keeps the backoff and schedules no fixed retry', () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, timers} = buildHarness(clock, {hidden: true});

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1000});

  assert.deepEqual(timers.map((timer) => timer.ms), [1000]);
});

test('a catch-up naming another release swaps a hidden tab once the shell refresh answers', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes} = buildHarness(clock, {hidden: true});

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  catchupComplete(socket, {assetVersion: 'v2'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 5000, path: 'refresh'}]);
});

test('a catch-up after a 1006 close swaps a hidden tab the same way', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes} = buildHarness(clock, {hidden: true});

  context.connectWS();
  FakeWebSocket.instances[0].emitClose({code: 1006});
  clock.now = 9000;
  // The reconnect's own backoff timer would fire here; the test opens the socket directly.
  context.connectWS();
  const socket = FakeWebSocket.instances[1];
  socket.emitOpen();
  catchupComplete(socket, {assetVersion: 'v2'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 9000, path: 'refresh'}]);
});

test('a visible tab whose catch-up names another release keeps its code', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes} = buildHarness(clock);

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  catchupComplete(socket, {assetVersion: 'v2'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, []);
});

test('a catch-up naming the page own release reloads nothing', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes} = buildHarness(clock, {hidden: true});

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  catchupComplete(socket);
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, []);
});

test('a return before the swap navigates cancels it, and the next hide tries again', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, document} = buildHarness(clock, {hidden: true});

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  catchupComplete(socket, {assetVersion: 'v2'});
  // The tab comes back while the shell refreshes: the navigation must not start.
  document.hidden = false;
  document.dispatchEvent({type: 'visibilitychange'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();
  assert.deepEqual(reloaded, [], 'a visible tab keeps its code');

  // The next hide runs the swap again.
  document.hidden = true;
  document.dispatchEvent({type: 'visibilitychange'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();
  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 5000, path: 'refresh'}]);
});

test('a swap the guard holds waits for the window to end while the tab stays hidden', async () => {
  const clock = {now: 5000};
  const {context, FakeWebSocket, reloaded, pendingRefreshes, timers, sessionStorage} =
    buildHarness(clock, {hidden: true});
  sessionStorage.setItem(RELOAD_GUARD_KEY, '5000');

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  socket.emitOpen();
  clock.now = 20000;
  catchupComplete(socket, {assetVersion: 'v2'});
  settleRefreshes(pendingRefreshes);
  await flushPromises();

  assert.deepEqual(reloaded, [], 'the guard window holds the swap');
  const wait = timers.find((timer) => timer.ms === RESTART_WINDOW_MS - 15000);
  assert.ok(wait, 'the wait runs to the end of the window');
  clock.now = 65000;
  wait.fn();
  settleRefreshes(pendingRefreshes);
  await flushPromises();
  assert.deepEqual(reloaded, [{cause: 'stale_asset', at: 65000, path: 'refresh'}]);
});
