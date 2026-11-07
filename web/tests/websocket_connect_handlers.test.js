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
