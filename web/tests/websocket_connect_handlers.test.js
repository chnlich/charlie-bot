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

// The socket replays no task_tree_changed frame, so an open after a close (only a network close closes the page's
// socket now) and an open after a view without a socket both reload the list. A page load's own first socket
// reloads nothing: the page carried the server's snapshot.
test('an open after a close or a session-less view reloads the list', () => {
  const reloads = [];
  const timers = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    setTimeout: (fn) => { timers.push(fn); return timers.length; },
    switchSidebarFilter: (filter) => reloads.push(filter),
  });

  context.connectWS();
  FakeWebSocket.instances[0].emitOpen();
  assert.deepEqual(reloads, [], "a page load's first socket reloads nothing");

  FakeWebSocket.instances[0].emitClose({code: 1006});
  timers[0]();
  FakeWebSocket.instances[1].emitOpen();
  assert.deepEqual(reloads, ['all'], 'a reopen reloads the list the closed socket could have changed');

  // A phone backgrounding can kill the socket with no close event; the page's resume opens a new one over it.
  context.connectWS();
  FakeWebSocket.instances[2].emitOpen();
  assert.deepEqual(reloads, ['all', 'all'], 'a socket replaced without a close event reloads the list too');

  context.SESSION_ID = null;
  context.connectWS();
  assert.equal(FakeWebSocket.instances.length, 3, 'a view without a session opens no socket');
  context.SESSION_ID = 'session-b';
  context.connectWS();
  FakeWebSocket.instances[3].emitOpen();
  assert.deepEqual(reloads, ['all', 'all', 'all'], 'the first session opens reloads the list the empty socket missed');
});

// A connect attempt the page has not opened within four seconds is replaced by a fresh one, whose first SYN goes out at
// once. The deadline belongs to its attempt: the attempt's open, close and teardown clear it.
function deadlineContext(documentOverride) {
  const timers = [];
  const cleared = [];
  const loaded = loadWebsocketContext({
    SESSION_ID: 'session-a',
    setTimeout: (fn, ms) => {
      const id = timers.length + 1;
      timers.push({fn, ms, id});
      return id;
    },
    clearTimeout: (id) => cleared.push(id),
    ...(documentOverride ? {document: documentOverride} : {}),
  });
  return {...loaded, timers, cleared, deadlineOf: () => timers.find((timer) => timer.ms === 4000)};
}

test('an attempt still connecting at four seconds is replaced by a fresh attempt', () => {
  const {context, FakeWebSocket, deadlineOf} = deadlineContext();
  context.connectWS();
  deadlineOf().fn();

  assert.equal(FakeWebSocket.instances[0].closed, true, 'the stalled attempt is closed');
  assert.equal(FakeWebSocket.instances.length, 2, 'a fresh attempt takes its place at once');
});

test('an open or a close clears the attempt deadline', () => {
  const opened = deadlineContext();
  opened.context.connectWS();
  const openedDeadline = opened.deadlineOf();
  opened.FakeWebSocket.instances[0].emitOpen();
  assert.ok(opened.cleared.includes(openedDeadline.id), 'an open clears the deadline');

  const closed = deadlineContext();
  closed.context.connectWS();
  const closedDeadline = closed.deadlineOf();
  closed.FakeWebSocket.instances[0].emitClose({code: 1006});
  assert.ok(closed.cleared.includes(closedDeadline.id), 'a close clears the deadline');
});

test('closing the session socket clears the attempt deadline', () => {
  const {context, cleared, deadlineOf} = deadlineContext();
  context.connectWS();
  const deadline = deadlineOf();
  context.closeSessionSocket();
  assert.ok(cleared.includes(deadline.id), 'a torn-down attempt keeps no deadline');
});

test('a hidden page sets no attempt deadline', () => {
  const {context, deadlineOf} = deadlineContext({
    visibilityState: 'hidden',
    dispatchEvent: () => true,
    getElementById: () => null,
  });
  context.connectWS();
  assert.equal(deadlineOf(), undefined);
});
