// ---------------------------------------------------------------------------
// One socket per tab across session switches (websocket.js). A switch keeps
// the open socket and rides a `subscribe` frame — session_id, cursor,
// subscription — instead of closing and reconnecting; the server answers
// `subscribed`, replays from the cursor and closes with a fielded
// `catchup_complete`. From the switch's click until the matching `subscribed`
// frame the view drops every session frame.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

function buildContext(sessionId) {
  const messages = [];
  const announced = [];
  const timers = [];
  const subscribeFailures = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: sessionId,
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: () => {},
    appendMessageObject: (msg) => messages.push(msg.content || ''),
    showStreaming: () => {},
    hideStreaming: () => {},
    // session-view.js's failure path; the transport calls it on a refused subscribe.
    sessionSubscribeFailed: (request) => subscribeFailures.push({...request}),
    document: {
      getElementById: () => null,
      dispatchEvent: (event) => {
        announced.push({type: event.type, detail: {...event.detail}});
        return true;
      },
    },
  });
  return {context, FakeWebSocket, messages, announced, timers, subscribeFailures};
}

function lastSocket({FakeWebSocket}) {
  return FakeWebSocket.instances[FakeWebSocket.instances.length - 1];
}

function subscribeFrames(socket) {
  return socket.sent.map((text) => JSON.parse(text)).filter((frame) => frame.type === 'subscribe');
}

function catchups({announced}) {
  return announced.filter((event) => event.type === 'charliebot:client-socket-catchup').map((event) => event.detail);
}

test('a switch keeps the socket open and rides a subscribe frame', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();

  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});

  assert.equal(h.FakeWebSocket.instances.length, 1, 'the switch opened no second socket');
  assert.equal(socket.closed, false, 'the switch closed no socket');
  assert.deepEqual(subscribeFrames(socket), [
    {type: 'subscribe', session_id: 'session-b', cursor: 6, subscription: 1},
  ]);
});

test('the left session’s late frames drop until subscribed, then the target’s render', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();

  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});
  socket.emitMessage({type: 'stream', message: {role: 'assistant', content: 'left session draft', event_index: 9}});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'left session commit', event_index: 9}});
  assert.deepEqual(h.messages, [], 'the switch window drops the session it left');

  socket.emitMessage({type: 'subscribed', session_id: 'session-b', subscription: 1});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'target commit', event_index: 6}});
  assert.deepEqual(h.messages, ['target commit'], 'the subscribed session’s frames render');
});

test('a superseded subscribe’s answer and replay drop with the frames they carry', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();

  // Two switches: the first subscribe is unanswered when the second bootstrap completes.
  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});
  h.context.holdSessionFrames('session-c');
  h.context.SESSION_ID = 'session-c';
  h.context.requestSessionSubscription('session-c', 9, {from_session: 'session-b', generation: 2});
  assert.deepEqual(subscribeFrames(socket).map((frame) => frame.subscription), [1, 2],
      'both bootstrap completions requested their subscription');

  socket.emitMessage({type: 'subscribed', session_id: 'session-b', subscription: 1});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'stale replay', event_index: 6}});
  socket.emitMessage({type: 'catchup_complete', session_id: 'session-b', subscription: 1, event_total: 6});
  assert.deepEqual(h.messages, [], 'the superseded subscription’s answer binds nothing');
  assert.deepEqual(catchups(h).map((detail) => detail.subscription), [],
      'the superseded replay reports no catch-up');

  socket.emitMessage({type: 'subscribed', session_id: 'session-c', subscription: 2});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'current replay', event_index: 9}});
  assert.deepEqual(h.messages, ['current replay']);
});

test('a refused subscribe reports the failure and keeps dropping frames', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();

  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});

  // An error frame for another request number is not this subscribe's answer.
  socket.emitMessage({type: 'error', subscription: 9});
  assert.deepEqual(h.subscribeFailures, []);

  socket.emitMessage({type: 'error', subscription: 1});
  assert.deepEqual(h.subscribeFailures, [
    {session_id: 'session-b', cursor: 6, subscription: 1, from_session: 'session-a', generation: 1},
  ]);
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'late frame', event_index: 9}});
  assert.deepEqual(h.messages, [], 'the view binds no subscription after the refusal');
});

test('a reconnect bound to the left session is taken over by the subscribe', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  lastSocket(h).emitOpen();
  lastSocket(h).emitClose({code: 1006});

  // The pending reconnect fires while the switch's bootstrap is still in flight: it handshakes
  // for the session being left.
  h.timers[0].fn();
  const reconnected = lastSocket(h);
  assert.match(reconnected.url, /\/ws\/sessions\/session-a/, 'the pending reconnect handshook the left session');
  reconnected.emitOpen();

  // The bootstrap completes: the switch's subscribe takes the open socket over.
  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});
  assert.deepEqual(subscribeFrames(reconnected), [
    {type: 'subscribe', session_id: 'session-b', cursor: 6, subscription: 1},
  ], 'the subscribe takes the socket over');

  reconnected.emitMessage({type: 'subscribed', session_id: 'session-b', subscription: 1});
  reconnected.emitMessage({type: 'message', message: {role: 'user', content: 'target commit', event_index: 6}});
  assert.deepEqual(h.messages, ['target commit']);
});

test('five switches during a held blackout produce one subscribe after the restore', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();
  socket.emitClose({code: 1006});

  // The reconnect fires inside the blackout; it handshakes for the session being left.
  h.timers[0].fn();
  const reconnected = lastSocket(h);
  assert.match(reconnected.url, /\/ws\/sessions\/session-a/);
  reconnected.emitOpen();

  // Five clicks: each supersedes the last and no bootstrap completes during the blackout. The
  // restore: only the last switch's bootstrap completes, so only it requests.
  for (const target of ['b1', 'b2', 'b3', 'b4', 'b5']) {
    h.context.holdSessionFrames(target);
  }
  h.context.SESSION_ID = 'b5';
  h.context.requestSessionSubscription('b5', 42, {from_session: 'b4', generation: 5});

  const requests = subscribeFrames(reconnected);
  assert.equal(requests.length, 1, 'the restore sends one subscribe');
  assert.deepEqual(requests[0], {type: 'subscribe', session_id: 'b5', cursor: 42, subscription: 1});
});

test('the catch-up timing is recorded per subscription', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();

  socket.emitMessage({type: 'message', message: {role: 'user', content: 'seed', event_index: 0}});
  socket.emitMessage({type: 'catchup_complete', session_id: 'session-a', subscription: 0, event_total: 1});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'after the replay', event_index: 1}});
  assert.deepEqual(catchups(h), [
    {subscription: 0, session_id: 'session-a', eventTotal: 1, bytes: Buffer.byteLength(
        JSON.stringify({type: 'message', message: {role: 'user', content: 'seed', event_index: 0}}), 'utf8')},
  ], 'the handshake replay reports its own bytes once');

  h.context.holdSessionFrames('session-b');
  h.context.SESSION_ID = 'session-b';
  h.context.requestSessionSubscription('session-b', 1, {from_session: 'session-a', generation: 1});
  socket.emitMessage({type: 'subscribed', session_id: 'session-b', subscription: 1});
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'replayed', event_index: 1}});
  socket.emitMessage({type: 'catchup_complete', session_id: 'session-b', subscription: 1, event_total: 2});

  const replays = catchups(h);
  assert.equal(replays.length, 2, 'each subscription reports its own replay');
  assert.deepEqual(replays[1], {subscription: 1, session_id: 'session-b', eventTotal: 2, bytes: Buffer.byteLength(
      JSON.stringify({type: 'message', message: {role: 'user', content: 'replayed', event_index: 1}}), 'utf8')});
});

test('the no-session view closes the socket and receives no session frames', () => {
  const h = buildContext('session-a');
  h.context.connectWS();
  const socket = lastSocket(h);
  socket.emitOpen();
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'shown', event_index: 0}});
  assert.deepEqual(h.messages, ['shown']);

  h.context.closeSessionSocket();
  assert.equal(socket.closed, true, 'the page closes its own socket');
  assert.deepEqual(h.timers, [], 'the close schedules no reconnect');

  socket.emitMessage({type: 'message', message: {role: 'user', content: 'archived output', event_index: 1}});
  assert.deepEqual(h.messages, ['shown'], 'a frame of the archived session renders nothing');

  // The welcome view has no socket to reuse: a later open starts a new one.
  h.context.connectWS();
  assert.equal(h.FakeWebSocket.instances.length, 2, 'the next open opens its own socket');
});

test('a bootstrap that completed before the socket opened needs no subscribe', () => {
  const h = buildContext('session-a');
  h.context.SESSION_ID = 'session-b';

  // The switch's bootstrap completed while no socket was open: the request waits, and the
  // reconnect's handshake replays the same session from the same cursor.
  h.context.holdSessionFrames('session-b');
  h.context.eventCursor = 6;
  h.context.requestSessionSubscription('session-b', 6, {from_session: 'session-a', generation: 1});

  h.context.connectWS();
  const socket = lastSocket(h);
  assert.match(socket.url, /\/ws\/sessions\/session-b\?cursor=6/, 'the handshake carries the bootstrap cursor');
  socket.emitOpen();
  assert.deepEqual(subscribeFrames(socket), [], 'the handshake replay already covers the request');
  socket.emitMessage({type: 'message', message: {role: 'user', content: 'handshake replay', event_index: 6}});
  assert.deepEqual(h.messages, ['handshake replay'], 'the view bound to the handshake replay');
});
