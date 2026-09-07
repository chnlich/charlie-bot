// ---------------------------------------------------------------------------
// voice-input.js run-structure tests. Each recording is one run object owned
// through the module-level slot; these tests drive toggleVoice() against fake
// mic/socket/worklet doubles and assert the click-cadence and stop-ordering
// contract from the inside.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

const tick = () => new Promise((resolve) => setImmediate(resolve));

const STOP_FRAME = '{"type":"stop"}';
const CLOSED_TOAST = 'Voice connection closed before transcription finished';
const INVALID_TOAST = 'Invalid voice response from server';

function buildHarness({sessionId = 'session-a', micError = null} = {}) {
  const state = {
    toasts: [],
    micCalls: 0,
    streams: [],
    sockets: [],
    workletNodes: [],
    input: {value: '', focus() {}},
    chatFlags: [],
    buttonClasses: new Set(['bg-slate-800', 'border-slate-600']),
    overlay: () => null,
  };
  const overlayElements = new Map();
  state.overlay = () => overlayElements.get('voice-partial-overlay')?.textContent ?? null;

  class FakeStream {
    constructor() {
      this.stopped = false;
      this.tracks = [{stop: () => { this.stopped = true; }}];
      state.streams.push(this);
    }
    getTracks() { return this.tracks; }
  }

  class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;

    constructor(url) {
      this.url = url;
      this.sent = [];
      this.readyState = 0;
      this.onopen = this.onmessage = this.onclose = this.onerror = null;
      state.sockets.push(this);
    }
    send(payload) { this.sent.push(payload); }
    close() { this.readyState = 3; }
    open() { this.readyState = 1; if (this.onopen) this.onopen(); }
    emitMessage(data) { if (this.onmessage) this.onmessage({data}); }
    emitClose() { if (this.onclose) this.onclose(); }
  }

  class FakeAudioContext {
    constructor() {
      this.sampleRate = 48000;
      this.closed = false;
    }
    get audioWorklet() {
      return {addModule: async () => {}};
    }
    createMediaStreamSource() {
      return {connect() {}, disconnect() {}};
    }
    close() {
      this.closed = true;
      return Promise.resolve();
    }
  }

  class FakeAudioWorkletNode {
    constructor(_ctx, _name, _options) {
      this.disconnected = false;
      this.postMessageCalls = [];
      this.port = {
        onmessage: null,
        postMessage: (data) => this.postMessageCalls.push(data),
      };
      state.workletNodes.push(this);
    }
    disconnect() { this.disconnected = true; }
    // Test-side stand-ins for the worklet thread's replies.
    emitPcm(byteLength) {
      if (this.port.onmessage) this.port.onmessage({data: {type: 'pcm', buffer: new ArrayBuffer(byteLength)}});
    }
    replyFlushed(id) {
      if (this.port.onmessage) this.port.onmessage({data: {type: 'flushed', id}});
    }
  }

  const button = {
    classList: {
      add: (...names) => names.forEach((n) => state.buttonClasses.add(n)),
      remove: (...names) => names.forEach((n) => state.buttonClasses.delete(n)),
    },
  };
  const sandbox = {
    SESSION_ID: sessionId,
    console,
    setTimeout,
    clearTimeout,
    Blob: require('node:buffer').Blob,
    URL: {createObjectURL: () => 'blob:voice-worklet', revokeObjectURL() {}},
    WebSocket: FakeWebSocket,
    navigator: {
      mediaDevices: {
        getUserMedia: async () => {
          state.micCalls += 1;
          if (micError) throw micError;
          return new FakeStream();
        },
      },
    },
    window: {AudioContext: FakeAudioContext},
    AudioWorkletNode: FakeAudioWorkletNode,
    document: {
      getElementById: (id) => {
        if (id === 'msg-input') return state.input;
        if (id === 'voice-btn') return button;
        return overlayElements.get(id) || null;
      },
      createElement: () => {
        const el = {id: '', className: '', textContent: ''};
        el.remove = () => overlayElements.delete(el.id);
        return el;
      },
    },
    showToast: (message, isError) => state.toasts.push({msg: message, isError: !!isError}),
    wsUrlWithToken: (path) => 'ws://test' + path,
    Chat: {setVoiceContributed: (v) => state.chatFlags.push(v)},
    autoResize() {},
    saveDraft() {},
    detachSocketHandlers: (socket) => {
      socket.onopen = null;
      socket.onmessage = null;
      socket.onclose = null;
      socket.onerror = null;
    },
  };
  state.input.parentElement = {appendChild: (el) => overlayElements.set(el.id, el)};

  const context = vm.createContext(sandbox);
  vm.runInContext(readStatic('voice-input.js'), context, {filename: 'voice-input.js'});

  // Click once and drive the arming chain to `recording`: mic -> socket open
  // -> audio context + worklet module.
  const arm = async () => {
    context.toggleVoice();
    await tick();
    state.sockets[state.sockets.length - 1].open();
    await tick();
    await tick();
    return state.sockets[state.sockets.length - 1];
  };

  // Click stop and complete the flush handshake the worklet would run.
  const stopWithFlush = async () => {
    context.toggleVoice();
    const worklet = state.workletNodes[state.workletNodes.length - 1];
    const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
    assert.equal(flush.type, 'flush');
    worklet.replyFlushed(flush.id);
    await tick();
  };
  state.arm = arm;
  state.stopWithFlush = stopWithFlush;

  return {context, state};
}

test('first click claims the slot synchronously and paints Starting', async () => {
  const {context, state} = buildHarness();

  context.toggleVoice();
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-red-600', 'border-red-500']);
  assert.equal(state.overlay(), 'Starting...');
  assert.equal(state.micCalls, 1);

  await tick();
  const socket = state.sockets[0];
  socket.open();
  await tick();
  await tick();
  assert.equal(state.overlay(), 'Listening...');
  assert.equal(state.sockets.length, 1);
  assert.equal(socket.url, 'ws://test/ws/voice/session-a');
});

test('clicks while a run is arming are ignored, not queued', async () => {
  const {context, state} = buildHarness();

  context.toggleVoice();
  context.toggleVoice();
  context.toggleVoice();
  await tick();
  state.sockets[0].open();
  await tick();
  await tick();

  assert.equal(state.micCalls, 1);
  assert.equal(state.sockets.length, 1);
  assert.equal(state.overlay(), 'Listening...');
});

test('stop is idempotent: a double stop click sends one stop frame', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  context.toggleVoice();
  context.toggleVoice();
  const worklet = state.workletNodes[0];
  const flushes = worklet.postMessageCalls.filter((m) => m.type === 'flush');
  assert.equal(flushes.length, 1);
  worklet.replyFlushed(flushes[0].id);
  await tick();

  assert.equal(socket.sent.filter((m) => m === STOP_FRAME).length, 1);
  assert.equal(state.overlay(), 'Finalizing...');
});

test('stop flushes the tail through the socket before the stop message', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  context.toggleVoice();
  const worklet = state.workletNodes[0];
  const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];

  worklet.emitPcm(4096);
  assert.equal(socket.sent[socket.sent.length - 1].byteLength, 4096);

  worklet.replyFlushed(flush.id);
  await tick();

  assert.equal(socket.sent[socket.sent.length - 1], STOP_FRAME);
  assert.equal(state.overlay(), 'Finalizing...');
});

test('clicks during Finalizing create no socket; final fills the input and frees the slot', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();
  await state.stopWithFlush();

  context.toggleVoice();
  await tick();
  assert.equal(state.sockets.length, 1);

  socket.emitMessage(JSON.stringify({type: 'final', text: 'hello world'}));
  assert.equal(state.input.value, 'hello world');
  assert.equal(state.overlay(), null);
  assert.deepEqual(state.chatFlags, [true]);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
  assert.equal(state.sockets.length, 2);
  assert.equal(state.overlay(), 'Starting...');
});

test('an empty final toasts No speech detected and frees the slot', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  socket.emitMessage(JSON.stringify({type: 'final', text: '   '}));
  assert.deepEqual(state.toasts, [{msg: 'No speech detected', isError: false}]);
  assert.equal(state.input.value, '');
  assert.equal(state.overlay(), null);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
  assert.equal(state.sockets.length, 2);
});

test('teardown removes the slot first, then aborts connecting socket and unused stream', async () => {
  const {context, state} = buildHarness();

  context.toggleVoice();
  await tick();
  const stream = state.streams[0];
  const socket = state.sockets[0];
  assert.equal(socket.readyState, 0);

  context.resetVoiceState();
  assert.equal(stream.stopped, true);
  assert.equal(socket.readyState, 3);
  assert.equal(socket.onopen, null);
  assert.equal(socket.onmessage, null);
  assert.equal(socket.onclose, null);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.overlay(), null);

  socket.open();
  socket.emitMessage(JSON.stringify({type: 'final', text: 'late'}));
  socket.emitClose();
  await tick();
  assert.deepEqual(state.toasts, []);
  assert.equal(state.input.value, '');

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
  assert.equal(state.sockets.length, 2);
});

test('socket close during recording discards with the closed-connection toast', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  socket.emitClose();
  assert.deepEqual(state.toasts, [{msg: CLOSED_TOAST, isError: true}]);
  assert.equal(state.overlay(), null);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
});

test('undecodable server frames discard with the invalid-response toast', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  socket.emitMessage('not json');
  assert.deepEqual(state.toasts, [{msg: INVALID_TOAST, isError: true}]);
  assert.equal(state.overlay(), null);
});

test('server error frames surface the server text and free the slot', async () => {
  const {context, state} = buildHarness();
  const socket = await state.arm();

  socket.emitMessage(JSON.stringify({type: 'error', text: 'speech inference failed: boom'}));
  assert.deepEqual(state.toasts, [{msg: 'speech inference failed: boom', isError: true}]);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
});

test('arming failure toasts the error, unlights the button, and frees the slot', async () => {
  const {context, state} = buildHarness({micError: new Error('boom')});

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Voice input failed: boom', isError: true}]);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.overlay(), null);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
});

test('validation failure keeps the wording, slot empty, and button unlit', async () => {
  const {context, state} = buildHarness({sessionId: ''});

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Open a session before recording voice input', isError: true}]);
  assert.equal(state.micCalls, 0);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
});
