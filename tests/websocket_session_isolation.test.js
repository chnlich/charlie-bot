const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

function buildContext(sessionId) {
  const messages = [];
  const timers = [];
  const sidebarActions = [];

  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: sessionId,
    marked: {parse: (txt) => txt},
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    appendMessage: (role, content, isVoice, timestamp, uploadedFiles) => {
      messages.push({role, content, isVoice: !!isVoice, timestamp, uploadedFiles: uploadedFiles || []});
    },
    appendMessageObject: (msg) => {
      const voiceKey = 'is_' + 'voice';
      messages.push({
        role: msg.role,
        content: msg.content,
        isVoice: !!msg[voiceKey],
        timestamp: msg.timestamp,
        uploadedFiles: msg.uploaded_files || [],
      });
    },
    addWorkerCard: () => {},
    updateWorkerStatus: () => {},
    updateSpinner: () => {},
    switchSidebarFilter: (filter) => {
      sidebarActions.push({type: 'filter', value: filter});
    },
    handleSidebarSearch: (query) => {
      sidebarActions.push({type: 'search', value: query});
    },
    document: {
      getElementById: (id) => {
        if (id === 'sidebar-search') return null;
        return null;
      },
      createElement: () => ({className: '', innerHTML: '', dataset: {}}),
    },
  });
  return {context, FakeWebSocket, messages, timers, sidebarActions};
}

test('ignores stale socket events after rapid session switch', () => {
  const {context, FakeWebSocket, messages} = buildContext('session-a');

  context.connectWS();
  const staleSocket = FakeWebSocket.instances[0];
  staleSocket.emitOpen();

  context.SESSION_ID = 'session-b';
  context.connectWS();
  const activeSocket = FakeWebSocket.instances[1];
  activeSocket.emitOpen();

  staleSocket.emitMessage({type: 'message', message: {role: 'user', content: 'old session text'}});
  activeSocket.emitMessage({type: 'message', message: {role: 'user', content: 'active session text'}});

  assert.deepEqual(messages.map((m) => m.content), ['active session text']);
});

test('only active socket schedules reconnect on close', () => {
  const {context, FakeWebSocket, timers} = buildContext('session-a');

  context.connectWS();
  const staleSocket = FakeWebSocket.instances[0];

  context.SESSION_ID = 'session-b';
  context.connectWS();
  const activeSocket = FakeWebSocket.instances[1];

  staleSocket.emitClose();
  assert.equal(timers.length, 0);

  activeSocket.emitClose();
  assert.equal(timers.length, 1);
});

test('disconnectWS detaches handlers and delayed stale callbacks are ignored', () => {
  const {context, FakeWebSocket, messages} = buildContext('session-a');

  context.connectWS();
  const socket = FakeWebSocket.instances[0];
  const delayedMessage = socket.onmessage;

  context.disconnectWS();

  assert.equal(socket.onmessage, null);
  assert.equal(socket.onclose, null);
  assert.equal(socket.closed, true);

  delayedMessage({data: JSON.stringify({type: 'message', message: {role: 'user', content: 'should be ignored'}})});
  assert.equal(messages.length, 0);
});

test('session_group_changed refreshes the active sidebar filter', () => {
  const {context, sidebarActions} = buildContext('session-a');

  context.handleWSEvent({type: 'session_group_changed', session_id: 'session-a', group: 'Work'}, 'session-a', 0);

  assert.deepEqual(sidebarActions, [{type: 'filter', value: 'all'}]);
});

test('session_group_changed preserves active sidebar search', () => {
  const {context, sidebarActions} = buildContext('session-a');

  context.document.getElementById = (id) => {
    if (id === 'sidebar-search') return {value: 'alpha'};
    return null;
  };

  context.handleWSEvent({type: 'session_group_changed', session_id: 'session-a', group: 'Work'}, 'session-a', 0);

  assert.deepEqual(sidebarActions, [{type: 'search', value: 'alpha'}]);
});

test('result event forces a poll without writing the header directly', () => {
  const {context} = buildContext('session-a');
  let pollOpts = null;

  // The WebSocket handler must not call updateUsageDisplay (removed) or any
  // header writer; the forced poll is what updates the header.
  context.updateUsageDisplay = () => {
    throw new Error('updateUsageDisplay must not be called on result events');
  };
  context.pollActiveSessionView = (opts) => {
    pollOpts = opts;
  };

  context.handleWSEvent({type: 'result', total_cost_usd: 1.25}, 'session-a', 0);

  assert.ok(pollOpts && pollOpts.force === true, 'result event must force the usage poll');
});

test('user message deltas forward structured uploaded_files to the renderer', () => {
  const {context, messages} = buildContext('session-a');

  context.handleWSEvent({
    type: 'message',
    message: {
      role: 'user',
      content: '',
      uploaded_files: [{filename: 'report.pdf', path: '/tmp/report.pdf'}],
    },
  }, 'session-a', 0);

  assert.equal(messages.length, 1);
  assert.equal(messages[0].role, 'user');
  assert.deepEqual(messages[0].uploadedFiles, [{filename: 'report.pdf', path: '/tmp/report.pdf'}]);
});
