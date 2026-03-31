const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const WEBSOCKET_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'websocket.js'),
  'utf8'
);

function buildContext(sessionId) {
  const messages = [];
  const timers = [];
  const sidebarActions = [];

  class FakeWebSocket {
    static instances = [];

    constructor(url) {
      this.url = url;
      this.sent = [];
      this.closed = false;
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
      FakeWebSocket.instances.push(this);
    }

    send(payload) {
      this.sent.push(payload);
    }

    close() {
      this.closed = true;
    }

    emitOpen() {
      if (this.onopen) this.onopen();
    }

    emitClose() {
      if (this.onclose) this.onclose();
    }

    emitMessage(data) {
      if (this.onmessage) this.onmessage({data: JSON.stringify(data)});
    }
  }

  const context = {
    SESSION_ID: sessionId,
    eventCursor: 0,
    reconnectDelay: 1000,
    thinkingStart: null,
    sessionUnread: {},
    localStorage: {getItem: () => null},
    location: {protocol: 'http:', host: 'localhost:8000'},
    console: {log: () => {}, error: () => {}},
    marked: {parse: (txt) => txt},
    WebSocket: FakeWebSocket,
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: () => {},
    hideStreaming: () => {},
    showStreaming: () => {},
    appendMessage: (role, content, isVoice) => {
      messages.push({role, content, isVoice: !!isVoice});
    },
    setSessionSpinner: () => {},
    addWorkerCard: () => {},
    updateWorkerStatus: () => {},
    updateSpinner: () => {},
    updateUsageDisplay: () => {},
    showDiffModal: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    switchSidebarFilter: (filter) => {
      sidebarActions.push({type: 'filter', value: filter});
    },
    handleSidebarSearch: (query) => {
      sidebarActions.push({type: 'search', value: query});
    },
    currentFilter: 'all',
    document: {
      getElementById: (id) => {
        if (id === 'sidebar-search') return null;
        return null;
      },
      createElement: () => ({className: '', innerHTML: '', dataset: {}}),
    },
  };

  vm.createContext(context);
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
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

  staleSocket.emitMessage({type: 'user', content: 'old session text'});
  activeSocket.emitMessage({type: 'user', content: 'active session text'});

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

  delayedMessage({data: JSON.stringify({type: 'user', content: 'should be ignored'})});
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

test('result event triggers an immediate session usage refresh', () => {
  const {context} = buildContext('session-a');
  let usageEvent = null;
  let pollCalls = 0;

  context.updateUsageDisplay = (ev) => {
    usageEvent = ev;
  };
  context.pollActiveSessionView = () => {
    pollCalls += 1;
  };

  context.handleWSEvent({type: 'result', total_cost_usd: 1.25}, 'session-a', 0);

  assert.deepEqual(usageEvent, {type: 'result', total_cost_usd: 1.25});
  assert.equal(pollCalls, 1);
});
