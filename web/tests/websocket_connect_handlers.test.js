const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

// The connect handlers of websocket-handlers.js clear the network-loss flag when they run, and a handler
// registered after them must still see the reconnect as a network loss's.
test('every connect handler sees the network-loss flag, whichever handler clears it', () => {
  const timers = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    setTimeout: (fn) => { timers.push(fn); return timers.length; },
  });
  const seen = [];
  context.registerWsConnectHandler((connect) => seen.push(connect.afterNetworkLoss));

  context.connectWS();
  FakeWebSocket.instances[0].emitOpen();
  FakeWebSocket.instances[0].emitClose({code: 1006});
  context.connectWS();
  FakeWebSocket.instances[1].emitOpen();

  assert.deepEqual(seen, [false, true]);
});

// The socket replays no task_tree_changed frame, so an open after a close (a session switch closes and reopens the
// page's socket) and an open after a view without a socket both reload the list. A page load's own first socket
// reloads nothing: the page carried the server's snapshot.
test('an open after a close or a session-less view reloads the list', () => {
  const reloads = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    switchSidebarFilter: (filter) => reloads.push(filter),
  });

  context.connectWS();
  FakeWebSocket.instances[0].emitOpen();
  assert.deepEqual(reloads, [], "a page load's first socket reloads nothing");

  context.disconnectWS();
  context.connectWS();
  FakeWebSocket.instances[1].emitOpen();
  assert.deepEqual(reloads, ['all'], 'a reopen reloads the list the closed socket could have changed');

  // A phone backgrounding can kill the socket with no close event; the page's resume opens a new one over it.
  context.connectWS();
  FakeWebSocket.instances[2].emitOpen();
  assert.deepEqual(reloads, ['all', 'all'], 'a socket replaced without a close event reloads the list too');

  context.SESSION_ID = null;
  context.disconnectWS();
  context.connectWS();
  assert.equal(FakeWebSocket.instances.length, 3, 'a view without a session opens no socket');
  context.SESSION_ID = 'session-b';
  context.connectWS();
  FakeWebSocket.instances[3].emitOpen();
  assert.deepEqual(reloads, ['all', 'all', 'all'], 'the first session opens reloads the list the empty socket missed');
});
