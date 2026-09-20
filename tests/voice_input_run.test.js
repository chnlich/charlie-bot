// ---------------------------------------------------------------------------
// voice-input.js record-then-upload tests. Each recording is one run object
// owned through the module-level slot; these tests drive toggleVoice() against
// fake mic/XHR/worklet doubles and assert the click-cadence, upload, and
// guard contracts from the inside.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

const tick = () => new Promise((resolve) => setImmediate(resolve));

const RATE = 16000;
const CONFIRM_SAMPLES = 5 * RATE;
const MAX_SAMPLES = 5 * 60 * RATE;
const UPLOAD_TIMEOUT_MS = 60 * 1000;

function buildHarness({sessionId = 'session-a', micError = null} = {}) {
  const state = {
    toasts: [],
    micCalls: 0,
    streams: [],
    xhrs: [],
    workletNodes: [],
    timers: [],
    input: {value: '', focus() {}},
    chatFlags: [],
    buttonClasses: new Set(['bg-slate-800', 'border-slate-600']),
    overlayMap: new Map(),
    beforeunloadCount: 0,
  };

  class FakeStream {
    constructor() {
      this.stopped = false;
      this.tracks = [{stop: () => { this.stopped = true; }}];
      state.streams.push(this);
    }
    getTracks() { return this.tracks; }
  }

  class FakeXHR {
    constructor() {
      this.status = 0;
      this.response = null;
      this.upload = {};
      this.aborted = false;
      this.sentBody = null;
      this.headers = {};
      this.onload = this.onerror = this.onabort = null;
      state.xhrs.push(this);
    }
    open(method, url) { this.method = method; this.url = url; }
    setRequestHeader(name, value) { this.headers[name] = value; }
    send(body) { this.sentBody = body; }
    abort() { this.aborted = true; if (this.onabort) this.onabort(); }
    // Test-side stand-ins for the browser's async events.
    completeUpload() { if (this.upload.onload) this.upload.onload(); }
    respond(status, response) { this.status = status; this.response = response; if (this.onload) this.onload(); }
    failNetwork() { if (this.onerror) this.onerror(); }
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
    // Test-side stand-in for the worklet thread: emit PCM built from real sample
    // values so the level math sees them.
    emitPcm(samples) {
      const copy = new Int16Array(samples); // a private buffer, like the worklet's transfer
      if (this.port.onmessage) {
        this.port.onmessage({data: {type: 'pcm', buffer: copy.buffer}});
      }
    }
    emitPcmCount(count, amplitude = 8000) {
      const samples = new Int16Array(count);
      for (let i = 0; i < count; i++) samples[i] = amplitude;
      this.emitPcm(samples);
    }
    replyFlushed(id) {
      if (this.port.onmessage) this.port.onmessage({data: {type: 'flushed', id}});
    }
  }

  // children mimics a real HTMLCollection: index + length only, none of the
  // array prototype methods, so code under test must go through Array.from.
  const makeEl = () => {
    const kids = [];
    const el = {id: '', className: '', textContent: '', style: {}, removed: false};
    const sync = () => {
      const collection = {length: kids.length};
      for (let i = 0; i < kids.length; i++) collection[i] = kids[i];
      el.children = collection;
    };
    sync();
    el.appendChild = (child) => {
      kids.push(child);
      sync();
    };
    el.remove = () => {
      el.removed = true;
      if (el.id) state.overlayMap.delete(el.id);
    };
    return el;
  };
  const findByClass = (root, name) => {
    for (let i = 0; i < root.children.length; i++) {
      if (root.children[i].className === name) return root.children[i];
    }
    return null;
  };

  const button = {
    classList: {
      add: (...names) => names.forEach((n) => state.buttonClasses.add(n)),
      remove: (...names) => names.forEach((n) => state.buttonClasses.delete(n)),
    },
  };
  const sandbox = {
    SESSION_ID: sessionId,
    console,
    setTimeout: (fn, ms) => {
      state.timers.push({fn, ms, cleared: false});
      return state.timers.length - 1;
    },
    clearTimeout: (id) => {
      if (state.timers[id]) state.timers[id].cleared = true;
    },
    Blob: require('node:buffer').Blob,
    URL: {createObjectURL: () => 'blob:voice-worklet', revokeObjectURL() {}},
    XMLHttpRequest: FakeXHR,
    navigator: {
      mediaDevices: {
        getUserMedia: async () => {
          state.micCalls += 1;
          if (micError) throw micError;
          return new FakeStream();
        },
      },
    },
    window: {
      AudioContext: FakeAudioContext,
      addEventListener: (type) => {
        if (type === 'beforeunload') state.beforeunloadCount += 1;
      },
      removeEventListener: (type) => {
        if (type === 'beforeunload') state.beforeunloadCount -= 1;
      },
    },
    AudioWorkletNode: FakeAudioWorkletNode,
    document: {
      getElementById: (id) => {
        if (id === 'msg-input') return state.input;
        if (id === 'voice-btn') return button;
        return state.overlayMap.get(id) || null;
      },
      createElement: () => makeEl(),
    },
    showToast: (message, isError) => state.toasts.push({msg: message, isError: !!isError}),
    accessTokenAuthorization: () => null,
    Chat: {setVoiceContributed: (v) => state.chatFlags.push(v)},
    autoResize() {},
    saveDraft() {},
  };
  state.input.parentElement = {
    children: [],
    appendChild: (el) => {
      state.input.parentElement.children.push(el);
      if (el.id) state.overlayMap.set(el.id, el);
    },
  };

  const context = vm.createContext(sandbox);
  vm.runInContext(readStatic('voice-input.js'), context, {filename: 'voice-input.js'});

  // Click once and drive the arming chain to `recording`: mic -> audio context
  // -> worklet module.
  const arm = async () => {
    context.toggleVoice();
    await tick();
    await tick();
    await tick();
    return state.workletNodes[state.workletNodes.length - 1];
  };

  // Click stop and complete the flush handshake the worklet would run; the
  // upload XHR exists once the flush resolves.
  const stopWithFlush = async () => {
    context.toggleVoice();
    const worklet = state.workletNodes[state.workletNodes.length - 1];
    const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
    assert.equal(flush.type, 'flush');
    worklet.replyFlushed(flush.id);
    await tick();
    await tick();
    return state.xhrs[state.xhrs.length - 1];
  };

  // The overlay's indicator elements, found through the fake DOM tree.
  state.voiceUi = () => {
    const overlay = state.overlayMap.get('voice-partial-overlay');
    if (!overlay) return null;
    const bar = findByClass(overlay, 'voice-level-bar');
    const meta = findByClass(overlay, 'voice-meta');
    return {
      fill: bar ? findByClass(bar, 'voice-level-fill') : null,
      timer: meta ? findByClass(meta, 'voice-timer') : null,
      hint: meta ? findByClass(meta, 'voice-hint') : null,
    };
  };
  state.arm = arm;
  state.stopWithFlush = stopWithFlush;

  return {context, state};
}

function parseWavHeader(buffer) {
  const view = new DataView(buffer);
  const text = (offset, length) => {
    let out = '';
    for (let i = 0; i < length; i++) out += String.fromCharCode(view.getUint8(offset + i));
    return out;
  };
  return {
    riff: text(0, 4),
    wave: text(8, 4),
    pcm: view.getUint16(20, true),
    channels: view.getUint16(22, true),
    sampleRate: view.getUint32(24, true),
    bits: view.getUint16(34, true),
    dataBytes: view.getUint32(40, true),
    totalBytes: buffer.byteLength,
  };
}

test('arming claims the slot, lights the button, and paints the zeroed indicator', async () => {
  const {context, state} = buildHarness();

  const worklet = await state.arm();

  assert.deepEqual([...state.buttonClasses].sort(), ['bg-red-600', 'border-red-500']);
  assert.equal(state.micCalls, 1);
  const ui = state.voiceUi();
  assert.equal(ui.fill.style.width, undefined); // no chunk yet: level untouched
  assert.equal(ui.timer.textContent, '0:00');
  assert.equal(ui.hint.textContent, 'Listening...');
  assert.equal(worklet.postMessageCalls.length, 0);
});

test('chunks update the level bar and the elapsed timer client-side', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(RATE, 12000); // 1 s of loud audio
  let ui = state.voiceUi();
  assert.equal(ui.fill.style.width, '100%');
  assert.equal(ui.timer.textContent, '0:01');

  worklet.emitPcmCount(RATE, 0); // 1 s of silence: the bar decays, the timer climbs
  ui = state.voiceUi();
  assert.equal(ui.fill.style.width, '80%');
  assert.equal(ui.timer.textContent, '0:02');
});

test('the confirm probe fires exactly once at 5 s and inserts its words', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(CONFIRM_SAMPLES - 2);
  assert.equal(state.xhrs.length, 0); // 4.999 s: no probe yet

  worklet.emitPcmCount(2);
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1);
  const confirm = state.xhrs[0];
  assert.equal(confirm.url, '/api/voice/session-a/confirm');
  const header = parseWavHeader(confirm.sentBody);
  assert.equal(header.dataBytes, CONFIRM_SAMPLES * 2); // the opening clip only

  worklet.emitPcmCount(CONFIRM_SAMPLES);
  await tick();
  assert.equal(state.xhrs.length, 1); // never a second probe

  confirm.respond(200, {text: '你好今天'});
  await tick();
  await tick();
  assert.equal(state.input.value, '你好今天');
  assert.deepEqual(state.chatFlags, [true]);
  assert.equal(state.voiceUi().hint.textContent, '你好今天');
});

test('a failed confirm probe only hints and never blocks recording', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(CONFIRM_SAMPLES);
  await tick();
  state.xhrs[0].failNetwork();
  await tick();
  await tick();

  assert.equal(state.voiceUi().hint.textContent, 'Recognition check failed');
  assert.equal(state.input.value, '');
  assert.equal(state.beforeunloadCount, 1); // still recording
  assert.equal(state.micCalls, 1);

  const upload = await state.stopWithFlush();
  assert.equal(upload.url, '/api/voice/session-a'); // recording and upload unaffected
});

test('release assembles one 16 kHz PCM16 mono WAV and walks Uploading -> Decoding -> text', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(3 * RATE);
  worklet.emitPcmCount(CONFIRM_SAMPLES, 0);

  const upload = await state.stopWithFlush();

  assert.equal(upload.url, '/api/voice/session-a');
  assert.equal(state.voiceUi().hint.textContent, 'Uploading...');
  assert.equal(upload.method, 'POST');
  const header = parseWavHeader(upload.sentBody);
  assert.equal(header.riff, 'RIFF');
  assert.equal(header.wave, 'WAVE');
  assert.equal(header.pcm, 1);
  assert.equal(header.channels, 1);
  assert.equal(header.sampleRate, RATE);
  assert.equal(header.bits, 16);
  assert.equal(header.dataBytes, (3 * RATE + CONFIRM_SAMPLES) * 2);
  assert.equal(header.totalBytes, 44 + header.dataBytes);

  upload.completeUpload();
  assert.equal(state.voiceUi().hint.textContent, 'Decoding...');

  upload.respond(200, {text: '你好今天天气不错'});
  await tick();
  await tick();
  assert.equal(state.input.value, '你好今天天气不错');
  assert.deepEqual(state.chatFlags, [true]); // the unanswered confirm inserted nothing
  assert.equal(state.beforeunloadCount, 0); // idle again
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2); // success discarded the buffer: the next click re-arms
});

test('the final text replaces an intact confirm span in place', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  await tick();
  state.xhrs[0].respond(200, {text: '你好今天'});
  await tick();
  await tick();
  state.input.value = '你好今天 手动补充'; // the user typed after the confirm words

  const upload = await state.stopWithFlush();
  upload.respond(200, {text: '你好今天天气不错'});
  await tick();
  await tick();

  // The confirm words are still where the probe put them: the full text takes
  // their place and the manual tail stays after it.
  assert.equal(state.input.value, '你好今天天气不错 手动补充');
});

test('the final text appends when the confirm span was disturbed', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  await tick();
  state.xhrs[0].respond(200, {text: '你好今天'});
  await tick();
  await tick();
  state.input.value = '你好吗'; // the user edited the confirm words away

  const upload = await state.stopWithFlush();
  upload.respond(200, {text: '你好今天天气不错'});
  await tick();
  await tick();

  assert.equal(state.input.value, '你好吗 你好今天天气不错');
});

test('an HTTP error keeps the buffer and the next click retries the same audio', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  const upload = await state.stopWithFlush();
  upload.respond(500, {error: 'speech inference failed: boom'});
  await tick();
  await tick();

  assert.deepEqual(state.toasts, [{msg: 'speech inference failed: boom', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice();
  const retry = state.xhrs[state.xhrs.length - 1];
  assert.notEqual(retry, upload);
  assert.equal(retry.url, '/api/voice/session-a');
  assert.deepEqual(retry.sentBody, upload.sentBody); // the same buffered audio re-uploads

  retry.respond(200, {text: '你好今天'});
  await tick();
  await tick();
  assert.equal(state.input.value, '你好今天');
});

test('a network error and a 60 s upload timeout both land in the retry state', async () => {
  const {context, state} = buildHarness();
  let worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  let upload = await state.stopWithFlush();
  upload.failNetwork();
  await tick();
  await tick();
  assert.deepEqual(state.toasts, [{msg: 'Voice request failed: network error', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');

  context.toggleVoice(); // retry
  upload = state.xhrs[state.xhrs.length - 1];
  const timeoutTimer = state.timers.find((timer) => timer.ms === UPLOAD_TIMEOUT_MS && !timer.cleared);
  assert.ok(timeoutTimer, 'the upload arms a 60 s timeout');
  timeoutTimer.fn();
  await tick();
  await tick();
  assert.ok(upload.aborted);
  assert.deepEqual(state.toasts.slice(-1), [{msg: 'Voice upload timed out', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
});

test('a server 400 or 503 on the upload lands in the retry state with the server text', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  const first = await state.stopWithFlush();
  first.respond(400, {error: 'voice recording exceeds the 300s limit'});
  await tick();
  await tick();
  assert.deepEqual(state.toasts.slice(-1), [{msg: 'voice recording exceeds the 300s limit', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice(); // retry with the same buffer
  const second = state.xhrs[state.xhrs.length - 1];
  second.respond(503, {error: 'speech models are still downloading'});
  await tick();
  await tick();
  assert.deepEqual(state.toasts.slice(-1), [{msg: 'speech models are still downloading', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
  assert.equal(state.beforeunloadCount, 0);
});

test('a click while the upload is in flight cancels it and drops the buffer', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  const upload = await state.stopWithFlush();
  assert.equal(state.beforeunloadCount, 1); // request in flight

  context.toggleVoice();
  assert.ok(upload.aborted);
  assert.deepEqual(state.toasts, [{msg: 'Voice upload canceled', isError: true}]);
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2); // buffer discarded: the next click re-arms
});

test('the beforeunload guard engages while recording and disengages when idle', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  assert.equal(state.beforeunloadCount, 1);

  const upload = await state.stopWithFlush();
  assert.equal(state.beforeunloadCount, 1); // still in flight

  upload.respond(200, {text: 'done'});
  await tick();
  await tick();
  assert.equal(state.beforeunloadCount, 0);
});

test('recording auto-stops at the 5-minute cap and the upload trims to it', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(MAX_SAMPLES / 2);
  worklet.emitPcmCount(MAX_SAMPLES / 2);
  await tick();
  await tick();

  const upload = await state.stopWithFlush();
  const header = parseWavHeader(upload.sentBody);
  assert.equal(header.dataBytes, MAX_SAMPLES * 2); // trimmed to the server's cap

  upload.respond(200, {text: 'long dictation'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'long dictation');
  assert.equal(state.beforeunloadCount, 0);
});

test('teardown aborts an in-flight upload and frees the slot', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(CONFIRM_SAMPLES);

  const upload = await state.stopWithFlush();
  context.resetVoiceState();

  assert.ok(upload.aborted);
  assert.equal(state.streams[0].stopped, true);
  assert.equal(state.beforeunloadCount, 0);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.voiceUi(), null);
  assert.deepEqual(state.chatFlags, [false]);

  upload.respond(200, {text: 'late'});
  await tick();
  await tick();
  assert.equal(state.input.value, ''); // released runs never touch the input
});

test('arming failure toasts the error, unlights the button, and frees the slot', async () => {
  const {context, state} = buildHarness({micError: new Error('boom')});

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Voice input failed: boom', isError: true}]);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.voiceUi(), null);
  assert.equal(state.beforeunloadCount, 0);

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
