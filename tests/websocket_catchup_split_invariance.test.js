// The client is a pure function of the frame sequence it receives.
//
// Once the cursor a client reports is the snapshot its first paint was built
// from, every frame the server replays is one the client does not already have.
// So there is no "catchup phase" to render differently: delivering a frame
// sequence in one connection must leave the same screen as delivering the same
// sequence split across a reconnect at ANY point.
//
// This is the mechanism-level statement of "the last reply must not show twice":
// the old code kept a `catchupDone` flag that suppressed preview clearing and
// dropped `stream` / `separator` frames while it was false, so the split
// delivery diverged from the unsplit one.

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const WEBSOCKET_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'websocket.js'),
  'utf8'
);

function buildClient() {
  const bubbles = [];
  const preview = {visible: false, content: ''};

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
    send(payload) { this.sent.push(payload); }
    close() { this.closed = true; }
    emitOpen() { if (this.onopen) this.onopen(); }
    emitClose() { if (this.onclose) this.onclose(); }
    emitMessage(data) { if (this.onmessage) this.onmessage({data: JSON.stringify(data)}); }
  }

  const context = {
    SESSION_ID: 's1',
    eventCursor: 0,
    reconnectDelay: 1000,
    thinkingStart: null,
    sessionUnread: {},
    localStorage: {getItem: () => null},
    location: {protocol: 'http:', host: 'localhost:8000'},
    console: {log: () => {}, error: () => {}},
    WebSocket: FakeWebSocket,
    setTimeout: () => 0,
    clearTimeout: () => {},
    document: {getElementById: () => null, querySelector: () => null},
    hideStreaming: () => { preview.visible = false; preview.content = ''; },
    showStreaming: (draft) => {
      preview.visible = true;
      preview.content = (draft && draft.content) || '';
    },
    appendMessageObject: (msg) => bubbles.push({role: msg.role, content: msg.content || ''}),
    appendMessage: () => {},
    renderedMessages: [],
    compactOutcome: () => 'none',
    appendCompactFailedNoticeIfMissing: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    pollActiveSessionView: () => {},
    renderExtUsage: () => {},
    refreshSessionStatusNow: () => {},
    setSessionIndicator: () => {},
    getSessionIndicatorState: () => ({}),
    setSessionPendingTriggerIndicator: () => {},
    updateSidebarSessionName: () => {},
    handleSidebarSearch: () => {},
    switchSidebarFilter: () => {},
    currentFilter: 'all',
    showDiffModal: () => {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(WEBSOCKET_JS, context);
  return {context, FakeWebSocket, bubbles, preview};
}

// What is on the screen: the committed bubbles plus the preview, if it holds text.
function screen(bubbles, preview) {
  const out = bubbles.map(b => b.role + ':' + b.content);
  if (preview.visible && preview.content) out.push('assistant:' + preview.content);
  return out;
}

function connect(client) {
  client.context.connectWS();
  const socket = client.FakeWebSocket.instances.at(-1);
  socket.emitOpen();
  return socket;
}

function deliver(socket, frames) {
  for (const frame of frames) socket.emitMessage(frame);
}

const REPLY = 'Here is the answer.';

// One turn: a user message, a streamed reply, the commit, the separator.
const FRAMES = [
  {type: 'message', message: {role: 'user', content: 'q1', event_index: 0}},
  {type: 'stream', message: {role: 'assistant', content: 'Here is', event_index: 1}},
  {type: 'stream', message: {role: 'assistant', content: REPLY, event_index: 1}},
  {type: 'message', message: {role: 'assistant', content: REPLY, event_index: 1}},
  {type: 'message', message: {role: 'separator', event_index: 2}},
  {type: 'master_done', still_thinking: false, event_index: 2},
];

function renderUnsplit(frames) {
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, frames);
  socket.emitMessage({type: 'catchup_complete'});
  return screen(client.bubbles, client.preview);
}

function renderSplitAt(frames, k) {
  const client = buildClient();
  const first = connect(client);
  first.emitMessage({type: 'catchup_complete'});
  deliver(first, frames.slice(0, k));
  // The socket drops; the reconnect replays the rest as catchup.
  first.emitClose();
  const second = connect(client);
  deliver(second, frames.slice(k));
  second.emitMessage({type: 'catchup_complete'});
  return screen(client.bubbles, client.preview);
}

test('a completed turn renders the reply exactly once', () => {
  const rendered = renderUnsplit(FRAMES);
  assert.deepEqual(rendered, ['user:q1', 'assistant:' + REPLY, 'separator:']);
  assert.equal(rendered.filter(x => x === 'assistant:' + REPLY).length, 1);
});

test('splitting the frame sequence at any point renders the same screen', () => {
  const want = renderUnsplit(FRAMES);
  for (let k = 0; k <= FRAMES.length; k++) {
    assert.deepEqual(renderSplitAt(FRAMES, k), want, 'divergence when split at frame ' + k);
  }
});

test('a reconnect mid-stream does not leave the reply on screen twice', () => {
  // Split exactly where the old code duplicated: the preview holds the whole
  // reply and the commit arrives on the next connection.
  const rendered = renderSplitAt(FRAMES, 3);
  assert.equal(rendered.filter(x => x === 'assistant:' + REPLY).length, 1);
});

test('separators delivered after a reconnect are still rendered', () => {
  const rendered = renderSplitAt(FRAMES, 4);
  assert.ok(rendered.includes('separator:'), 'separator was dropped');
});

test('a catchup that never completes still renders and still clears the preview', () => {
  // `catchup_complete` is only sent after a successful catchup; when the server
  // swallows a catchup error the frame never arrives. Rendering must not depend
  // on it.
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, FRAMES);
  const rendered = screen(client.bubbles, client.preview);
  assert.deepEqual(rendered, ['user:q1', 'assistant:' + REPLY, 'separator:']);
});
