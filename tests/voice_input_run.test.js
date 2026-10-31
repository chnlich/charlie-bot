// ---------------------------------------------------------------------------
// voice-input.js record-then-upload tests. Each recording is one run object
// owned through the module-level slot; these tests drive toggleVoice() against
// fake mic/XHR/worklet/WebSocket doubles and assert the click-cadence, upload,
// stop-state, guard, backend-menu, and preview-relay contracts from the inside.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, readPackageStatic} = require('./read_static');

const tick = () => new Promise((resolve) => setImmediate(resolve));

const RATE = 16000;
const FIVE_SECONDS = 5 * RATE;
const MAX_SAMPLES = 5 * 60 * RATE;
const UPLOAD_TIMEOUT_MS = 60 * 1000;

// The devices object the fake mic and fake device list produce: the five keys
// the end frame and the upload form carry, exactly as the server logs them.
const FAKE_DEVICES = {
  input_device: 'Fake Mic',
  capture_settings: {echoCancellation: true, noiseSuppression: true, autoGainControl: true, sampleRate: 48000},
  output_device: 'Fake Speakers',
  communications_output_device: 'Fake Comms Headset',
  device_error: null,
};

function buildHarness({sessionId = 'session-a', micError = null} = {}) {
  const state = {
    toasts: [],
    micCalls: 0,
    streams: [],
    deviceList: [
      {kind: 'audioinput', deviceId: 'mic-device', label: 'Fake Mic'},
      {kind: 'audiooutput', deviceId: 'default', label: 'Fake Speakers'},
      {kind: 'audiooutput', deviceId: 'communications', label: 'Fake Comms Headset'},
    ],
    enumerateCalls: 0,
    enumerateError: null,
    enumeratePending: null,
    xhrs: [],
    workletNodes: [],
    sockets: [],
    timers: [],
    audioContexts: [],
    addModuleCalls: 0,
    moduleError: null,
    modulePending: null,
    micPending: null,
    input: {value: '', focus() {}, addEventListener() {}},
    chatFlags: [],
    buttonClasses: new Set(['bg-slate-800', 'border-slate-600']),
    overlayMap: new Map(),
    beforeunloadCount: 0,
    sendLockOpens: 0,
    sendLockSettles: 0,
    caret: {disabled: false},
    storage: {},
  };

  class FakeStream {
    constructor() {
      this.stopped = false;
      this.tracks = [{
        label: 'Fake Mic',
        getSettings: () => ({echoCancellation: true, noiseSuppression: true, autoGainControl: true, sampleRate: 48000}),
        stop: () => { this.stopped = true; },
      }];
      state.streams.push(this);
    }
    getTracks() { return this.tracks; }
    getAudioTracks() { return this.tracks; }
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
      state.audioContexts.push(this);
    }
    get audioWorklet() {
      return {
        addModule: () => {
          state.addModuleCalls += 1;
          if (state.moduleError) return Promise.reject(state.moduleError);
          if (state.modulePending) return state.modulePending;
          return Promise.resolve();
        },
      };
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

  class FakeWebSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 0; // CONNECTING until the test opens it
      this.sentFrames = [];
      this.closed = false;
      this.onopen = this.onmessage = this.onerror = this.onclose = null;
      state.sockets.push(this);
    }
    send(data) {
      if (this.readyState !== 1) throw new Error('WebSocket is not open');
      this.sentFrames.push(data);
    }
    close() {
      this.closed = true;
      this.readyState = 3;
    }
    // Test-side stand-ins for the browser's socket events.
    openSocket() {
      this.readyState = 1;
      if (this.onopen) this.onopen();
    }
    receiveMessage(payload) {
      if (this.onmessage) this.onmessage({data: JSON.stringify(payload)});
    }
    dropSocket() {
      this.readyState = 3;
      if (this.onclose) this.onclose();
    }
    sentText() {
      return this.sentFrames.filter((frame) => typeof frame === 'string');
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
    WebSocket: FakeWebSocket,
    FormData: FormData,
    localStorage: {
      getItem: (key) => (key in state.storage ? state.storage[key] : null),
      setItem: (key, value) => {
        state.storage[key] = String(value);
      },
      removeItem: (key) => {
        delete state.storage[key];
      },
    },
    wsUrlWithToken: (path) => `ws://test-host${path}${path.includes('?') ? '&' : '?'}token=test-token`,
    VOICE_BACKENDS: [
      {id: 'batch', label: 'Batch backend', livePartials: false, unavailableReason: null},
      {id: 'gemini', label: 'Gemini 3.5 Transcribe Live', livePartials: true, unavailableReason: null},
      {id: 'muse', label: 'Muse Voice Transcribe', livePartials: true, unavailableReason: 'needs meta.model_api_key'},
    ],
    VOICE_DEFAULT_BACKEND: 'batch',
    navigator: {
      mediaDevices: {
        getUserMedia: () => {
          state.micCalls += 1;
          if (state.micPending) return state.micPending;
          if (micError) return Promise.reject(micError);
          return Promise.resolve(new FakeStream());
        },
        enumerateDevices: () => {
          state.enumerateCalls += 1;
          if (state.enumeratePending) return state.enumeratePending;
          if (state.enumerateError) return Promise.reject(state.enumerateError);
          return Promise.resolve(state.deviceList);
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
        if (id === 'voice-backend-btn') return state.caret;
        return state.overlayMap.get(id) || null;
      },
      createElement: () => makeEl(),
      addEventListener() {},
    },
    showToast: (message, isError) => state.toasts.push({msg: message, isError: !!isError}),
    accessTokenAuthorization: () => null,
    Chat: {
      setInputMode: (mode) => state.chatFlags.push(mode),
      registerInputMode() {},
    },
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
  // Page order: file-upload.js lands before voice-input.js and owns the
  // send-button lock the transcription windows report to.
  vm.runInContext(readStatic('file-upload.js'), context, {filename: 'file-upload.js'});
  vm.runInContext(readPackageStatic('features', 'voice', 'voice-input.js'), context, {filename: 'voice-input.js'});

  // Wrap the lock's open/close so tests can assert a path never engaged it
  // while the real counting behavior stays live for every other path.
  const realStarted = context.inputWorkStarted;
  const realSettled = context.inputWorkSettled;
  context.inputWorkStarted = (message) => {
    state.sendLockOpens += 1;
    realStarted(message);
  };
  context.inputWorkSettled = (message) => {
    state.sendLockSettles += 1;
    realSettled(message);
  };

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

  // Click stop and complete the flush handshake without expecting an upload:
  // a live stop waits for the relay's final or lands in the stop state.
  const stopLive = async () => {
    context.toggleVoice();
    const worklet = state.workletNodes[state.workletNodes.length - 1];
    const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
    assert.equal(flush.type, 'flush');
    worklet.replyFlushed(flush.id);
    await tick();
    await tick();
  };

  // The overlay's indicator elements, found through the fake DOM tree.
  state.voiceUi = () => {
    const overlay = state.overlayMap.get('voice-partial-overlay');
    if (!overlay) return null;
    const bar = findByClass(overlay, 'voice-level-bar');
    const meta = findByClass(overlay, 'voice-meta');
    return {
      partial: findByClass(overlay, 'voice-partial-text text-sm text-slate-400 whitespace-pre-wrap break-words'),
      fill: bar ? findByClass(bar, 'voice-level-fill') : null,
      timer: meta ? findByClass(meta, 'voice-timer') : null,
      hint: meta ? findByClass(meta, 'voice-hint') : null,
    };
  };

  // A pending timer by its delay, the way the code under test set it.
  state.timerByMs = (ms) => state.timers.find((timer) => timer.ms === ms && !timer.cleared);
  state.arm = arm;
  state.stopWithFlush = stopWithFlush;
  state.stopLive = stopLive;
  state.FakeStream = FakeStream;

  return {context, state};
}

// The upload's WAV rides the multipart form's audio part.
async function sentWavBytes(xhr) {
  const body = xhr.sentBody;
  return body instanceof FormData ? body.get('audio').arrayBuffer() : body;
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

test('arming claims the slot, lights the starting state, and paints the zeroed indicator', async () => {
  const {context, state} = buildHarness();

  const worklet = await state.arm();

  assert.deepEqual([...state.buttonClasses].sort(), ['bg-amber-600', 'border-amber-500']);
  assert.equal(state.micCalls, 1);
  assert.equal(state.enumerateCalls, 1); // device collection starts at recording start
  const ui = state.voiceUi();
  assert.equal(ui.fill.style.width, undefined); // no chunk yet: level untouched
  assert.equal(ui.timer.textContent, '0:00');
  assert.equal(ui.hint.textContent, 'Waiting for microphone signal...');
  assert.equal(worklet.postMessageCalls.length, 0);
});

test('the click lights the starting state and launches both startup branches at once', async () => {
  const {context, state} = buildHarness();
  let releaseMic;
  state.micPending = new Promise((resolve) => { releaseMic = resolve; });

  context.toggleVoice(); // no await: the starting state is synchronous with the click

  assert.deepEqual([...state.buttonClasses].sort(), ['bg-amber-600', 'border-amber-500']);
  assert.equal(state.voiceUi().hint.textContent, 'Starting...');
  // The mic branch is still open, yet the graph branch already launched too.
  assert.equal(state.micCalls, 1);
  assert.equal(state.audioContexts.length, 1);
  assert.equal(state.addModuleCalls, 1);

  releaseMic(new state.FakeStream());
  await tick();
  await tick();
  assert.equal(state.workletNodes.length, 1); // both branches settled: connected
  assert.equal(state.voiceUi().hint.textContent, 'Waiting for microphone signal...');
});

test('the mic branch launches while the worklet module branch is still open', async () => {
  const {context, state} = buildHarness();
  let releaseModule;
  state.modulePending = new Promise((resolve) => { releaseModule = resolve; });

  context.toggleVoice();
  await tick(); // the mic resolves: the module load is still pending

  assert.equal(state.micCalls, 1);
  assert.equal(state.streams.length, 1);
  assert.equal(state.addModuleCalls, 1);
  assert.equal(state.workletNodes.length, 0); // no graph before both branches settle

  releaseModule();
  await tick();
  await tick();
  assert.equal(state.workletNodes.length, 1);
});

test('all-zero chunks keep the button in the starting state', async () => {
  const {state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(RATE, 0);
  worklet.emitPcmCount(RATE, 0);

  assert.deepEqual([...state.buttonClasses].sort(), ['bg-amber-600', 'border-amber-500']);
  assert.equal(state.voiceUi().hint.textContent, 'Waiting for microphone signal...');
});

test('the first chunk with a non-zero sample lights the recording state and the listening hint', async () => {
  const {state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(RATE, 0); // digital silence: still starting

  const oneSample = new Int16Array(RATE);
  oneSample[123] = -7; // a single non-zero sample readies the run
  worklet.emitPcm(oneSample);

  assert.deepEqual([...state.buttonClasses].sort(), ['bg-red-600', 'border-red-500']);
  assert.equal(state.voiceUi().hint.textContent, 'Listening...');

  worklet.emitPcmCount(RATE, 0); // later silence leaves the state alone
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-red-600', 'border-red-500']);
});

test('the recording keeps the chunks that arrived before the ready signal', async () => {
  const {state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(2 * RATE, 0); // 2 s of digital zero before the sound
  worklet.emitPcmCount(RATE); // this chunk lights the recording state

  const upload = await state.stopWithFlush();
  const header = parseWavHeader(await sentWavBytes(upload));
  assert.equal(header.dataBytes, 3 * RATE * 2); // the silent prefix rides the WAV

  upload.respond(200, {text: 'ok'});
  await tick();
  await tick();
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

test('no request goes out while a recording runs past 5 s', async () => {
  const {state} = buildHarness();
  const worklet = await state.arm();

  worklet.emitPcmCount(FIVE_SECONDS + RATE);
  await tick();
  await tick();

  assert.equal(state.xhrs.length, 0); // the recording waits for its release
  assert.equal(state.input.value, '');
});

test('release assembles one 16 kHz PCM16 mono WAV and walks Uploading -> Decoding -> text', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(3 * RATE);
  worklet.emitPcmCount(FIVE_SECONDS, 0);

  const upload = await state.stopWithFlush();

  assert.equal(upload.url, '/api/voice/session-a');
  assert.equal(state.voiceUi().hint.textContent, 'Uploading...');
  assert.equal(upload.method, 'POST');
  const header = parseWavHeader(await sentWavBytes(upload));
  assert.equal(header.riff, 'RIFF');
  assert.equal(header.wave, 'WAVE');
  assert.equal(header.pcm, 1);
  assert.equal(header.channels, 1);
  assert.equal(header.sampleRate, RATE);
  assert.equal(header.bits, 16);
  assert.equal(header.dataBytes, (3 * RATE + FIVE_SECONDS) * 2);
  assert.equal(header.totalBytes, 44 + header.dataBytes);

  upload.completeUpload();
  assert.equal(state.voiceUi().hint.textContent, 'Decoding...');

  upload.respond(200, {text: '你好今天天气不错'});
  await tick();
  await tick();
  assert.equal(state.input.value, '你好今天天气不错');
  assert.deepEqual(state.chatFlags, ['voice']); // the upload's words are marked as voice input
  assert.equal(state.beforeunloadCount, 0); // idle again
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2); // success discarded the buffer: the next click re-arms
});

test('an HTTP error keeps the buffer and the next click retries the same audio', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(FIVE_SECONDS);

  const upload = await state.stopWithFlush();
  upload.respond(500, {error: 'speech inference failed: boom'});
  await tick();
  await tick();

  assert.deepEqual(state.toasts, [{msg: 'speech inference failed: boom', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice(); // retry — startUpload awaits the settled devices first
  await tick();
  await tick();
  const retry = state.xhrs[state.xhrs.length - 1];
  assert.notEqual(retry, upload);
  assert.equal(retry.url, '/api/voice/session-a');
  assert.deepEqual(await sentWavBytes(retry), await sentWavBytes(upload)); // the same buffered audio re-uploads

  retry.respond(200, {text: '你好今天'});
  await tick();
  await tick();
  assert.equal(state.input.value, '你好今天');
});

test('a network error and a 60 s upload timeout both land in the retry state', async () => {
  const {context, state} = buildHarness();
  let worklet = await state.arm();
  worklet.emitPcmCount(FIVE_SECONDS);

  let upload = await state.stopWithFlush();
  upload.failNetwork();
  await tick();
  await tick();
  assert.deepEqual(state.toasts, [{msg: 'Voice request failed: network error', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');

  context.toggleVoice(); // retry — startUpload awaits the settled devices first
  await tick();
  await tick();
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
  worklet.emitPcmCount(FIVE_SECONDS);

  const first = await state.stopWithFlush();
  first.respond(400, {error: 'voice recording exceeds the 300s limit'});
  await tick();
  await tick();
  assert.deepEqual(state.toasts.slice(-1), [{msg: 'voice recording exceeds the 300s limit', isError: true}]);
  assert.equal(state.voiceUi().hint.textContent, 'Upload failed — click to retry');
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice(); // retry with the same buffer — the settled devices await first
  await tick();
  await tick();
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
  worklet.emitPcmCount(FIVE_SECONDS);

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
  const header = parseWavHeader(await sentWavBytes(upload));
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
  worklet.emitPcmCount(FIVE_SECONDS);

  const upload = await state.stopWithFlush();
  context.resetVoiceState();

  assert.ok(upload.aborted);
  assert.equal(state.streams[0].stopped, true);
  assert.equal(state.beforeunloadCount, 0);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.voiceUi(), null);
  assert.deepEqual(state.chatFlags, [null]);

  upload.respond(200, {text: 'late'});
  await tick();
  await tick();
  assert.equal(state.input.value, ''); // released runs never touch the input
});

test('arming failure toasts the error, unlights the button, and frees the slot', async () => {
  const {context, state} = buildHarness({micError: new Error('boom')});

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Voice input failed: boom', isError: true}]);
  assert.equal(state.audioContexts.length, 1);
  assert.equal(state.audioContexts[0].closed, true); // the failed arm closes its context
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
  assert.equal(state.voiceUi(), null);
  assert.equal(state.beforeunloadCount, 0);

  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2);
});

test('a worklet module failure stops the opened mic tracks and frees the slot', async () => {
  const {context, state} = buildHarness();
  state.moduleError = new Error('no module');

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Voice input failed: no module', isError: true}]);
  assert.equal(state.streams.length, 1); // the mic resolved while the module failed
  assert.equal(state.streams[0].stopped, true);
  assert.equal(state.audioContexts[0].closed, true);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);

  state.moduleError = null;
  context.toggleVoice();
  await tick();
  assert.equal(state.micCalls, 2); // the slot is free for the next run
});

test('validation failure keeps the wording, slot empty, and button unlit', async () => {
  const {context, state} = buildHarness({sessionId: ''});

  await context.toggleVoice();
  assert.deepEqual(state.toasts, [{msg: 'Open a session before recording voice input', isError: true}]);
  assert.equal(state.micCalls, 0);
  assert.deepEqual([...state.buttonClasses].sort(), ['bg-slate-800', 'border-slate-600']);
});

// --- Live-preview backends ---------------------------------------------------

async function armLive(state, backendId = 'gemini') {
  state.storage['charliebot-voice-backend'] = backendId;
  const worklet = await state.arm();
  const socket = state.sockets[state.sockets.length - 1];
  return {worklet, socket};
}

test('a live backend opens the relay at recording start and replays pre-open chunks', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);

  assert.equal(socket.url, 'ws://test-host/ws/voice/session-a?backend=gemini&token=test-token');
  assert.equal(socket.readyState, 0); // still connecting while arming
  worklet.emitPcmCount(RATE); // captured before the socket opens
  assert.deepEqual(socket.sentFrames, []); // nothing sent yet: it rides the open replay

  socket.openSocket();
  assert.equal(socket.sentFrames.length, 1);
  assert.equal(socket.sentFrames[0].byteLength, RATE * 2); // one binary PCM16 frame

  worklet.emitPcmCount(RATE, 4000);
  assert.equal(socket.sentFrames.length, 2); // later chunks stream as they arrive
});

test('live partials render as grey not-yet-final text and the hint names the backend', async () => {
  const {state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();

  worklet.emitPcmCount(RATE); // the ready chunk: the hint names the backend
  assert.equal(state.voiceUi().hint.textContent, 'Listening \u00b7 Gemini 3.5 Transcribe Live');
  socket.receiveMessage({type: 'partial', text: '\u4f60\u597d'});
  assert.equal(state.voiceUi().partial.textContent, '\u4f60\u597d');
  assert.equal(state.input.value, ''); // partials never touch the input box
});

test('the end frame waits for the device enumeration without delaying recording start', async () => {
  const {context, state} = buildHarness();
  let releaseEnumeration;
  state.enumeratePending = new Promise((resolve) => { releaseEnumeration = resolve; });
  const {worklet, socket} = await armLive(state);

  assert.equal(state.enumerateCalls, 1); // collection started at recording start
  assert.equal(state.voiceUi().hint.textContent, 'Waiting for microphone signal...'); // already recording

  socket.openSocket();
  worklet.emitPcmCount(RATE);
  assert.equal(state.voiceUi().hint.textContent, 'Listening \u00b7 Gemini 3.5 Transcribe Live'); // the ready chunk
  context.toggleVoice();
  const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
  worklet.replyFlushed(flush.id);
  await tick();
  await tick();
  assert.deepEqual(socket.sentText(), []); // the enumeration is pending: no end frame yet

  releaseEnumeration(state.deviceList);
  await tick();
  await tick();
  assert.deepEqual(socket.sentText().map((frame) => JSON.parse(frame).type), ['end']);
});

test('an enumerateDevices rejection rides the end frame and the recording still completes', async () => {
  const {context, state} = buildHarness();
  state.enumerateError = new Error('permission denied');
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(2 * RATE);

  context.toggleVoice();
  const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
  worklet.replyFlushed(flush.id);
  await tick();
  await tick();

  assert.deepEqual(socket.sentText().map((frame) => JSON.parse(frame)), [{
    type: 'end',
    devices: {
      input_device: 'Fake Mic', // the mic facts survive the enumeration failure
      capture_settings: FAKE_DEVICES.capture_settings,
      output_device: null,
      communications_output_device: null,
      device_error: 'permission denied',
    },
  }]);

  socket.receiveMessage({type: 'final', text: '\u5b8c\u6210'});
  await tick();
  await tick();
  assert.equal(state.input.value, '\u5b8c\u6210'); // the recording still completes
  assert.equal(state.xhrs.length, 0); // the relay archived it; no upload
});

test('a final inside the budget inserts once and ends the run with no upload', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(2 * RATE);
  socket.receiveMessage({type: 'partial', text: '\u4f60\u597d\u4e16'});

  context.toggleVoice();
  const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
  worklet.replyFlushed(flush.id);
  await tick();
  await tick();
  assert.deepEqual(socket.sentText().map((frame) => JSON.parse(frame)), [
    {type: 'end', devices: FAKE_DEVICES},
  ]); // the relay is sealed, the devices ride the end frame
  assert.equal(state.xhrs.length, 0); // the recording waits for the final
  assert.equal(state.beforeunloadCount, 1); // the guard stays engaged through the wait

  socket.receiveMessage({type: 'final', text: '\u4f60\u597d\u4e16\u754c'});
  await tick();
  await tick();
  assert.ok(socket.closed); // the relay is done the moment the final lands
  assert.equal(state.input.value, '\u4f60\u597d\u4e16\u754c'); // inserted once
  assert.equal(state.voiceUi(), null); // the overlay is gone with the run
  assert.equal(state.xhrs.length, 0); // no upload: the relay archived the recording
  assert.equal(state.sendLockOpens, 0); // the send lock never engaged
  assert.equal(state.sendLockSettles, 0);
  assert.equal(state.beforeunloadCount, 0);

  await state.arm(); // the slot is free: the next click starts a new recording
  assert.equal(state.micCalls, 2);
});

test('an empty final inside the budget toasts and inserts nothing', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE);

  context.toggleVoice();
  const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
  worklet.replyFlushed(flush.id);
  await tick();
  await tick();

  socket.receiveMessage({type: 'final', text: ''});
  await tick();
  await tick();
  assert.ok(socket.closed);
  assert.equal(state.input.value, ''); // nothing inserted
  assert.equal(state.xhrs.length, 0); // no upload: the relay archived the recording
  assert.deepEqual(state.toasts, [{msg: 'No speech detected', isError: false}]);
  assert.equal(state.sendLockOpens, 0);

  await state.arm(); // the slot is free for a new recording
  assert.equal(state.micCalls, 2);
});

test('the relay final budget scales with the recording length', async () => {
  // Base 2000 ms plus 20 ms per recorded second, at the measured points and
  // at the cap (the 300 s recording auto-stops itself).
  for (const [seconds, budgetMs] of [[12.5, 2250], [201.6, 6032], [300, 8000]]) {
    const {context, state} = buildHarness();
    const {worklet, socket} = await armLive(state);
    socket.openSocket();
    worklet.emitPcmCount(seconds * RATE);

    const lastFrame = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
    if (!lastFrame || lastFrame.type !== 'flush') context.toggleVoice(); // the cap already stopped
    const flush = worklet.postMessageCalls[worklet.postMessageCalls.length - 1];
    assert.equal(flush.type, 'flush');
    worklet.replyFlushed(flush.id);
    await tick();
    await tick();

    const expiry = state.timerByMs(budgetMs);
    assert.ok(expiry, 'the budget for ' + seconds + ' s is ' + budgetMs + ' ms');
    expiry.fn(); // the expiry enters the stop state; no upload may start from it
    await tick();
    await tick();
    assert.equal(state.xhrs.length, 0);
  }
});

test('a 201.6 s live recording finalizes through the relay inside its scaled budget', async () => {
  const {state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(201.6 * RATE);

  await state.stopLive();
  const expiry = state.timerByMs(6032);
  assert.ok(expiry, 'the budget grew with the recording');
  assert.equal(state.xhrs.length, 0); // the run waits for the final, no upload
  assert.equal(state.beforeunloadCount, 1); // the guard holds through the wait

  socket.receiveMessage({type: 'final', text: '\u4f60\u597d\u4e16\u754c'}); // the measured 2130 ms
  await tick();
  await tick();
  assert.equal(state.input.value, '\u4f60\u597d\u4e16\u754c'); // the relay's final owns the words
  assert.equal(state.xhrs.length, 0); // zero upload requests
  assert.ok(socket.closed);
  assert.equal(state.beforeunloadCount, 0);
  assert.equal(state.sendLockOpens, 0);
});

test('an error frame mid-recording stops visibly and one click uploads the recording', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE);
  socket.receiveMessage({type: 'partial', text: '\u4f60\u597d'});

  socket.receiveMessage({type: 'error', message: 'backend exploded'});
  await tick();
  assert.ok(socket.closed); // the failed relay is done
  assert.equal(state.voiceUi().partial.textContent, ''); // the grey text cleared
  const hint = state.voiceUi().hint.textContent;
  assert.ok(hint.includes('Gemini 3.5 Transcribe Live'), hint);
  assert.ok(hint.includes('backend exploded'), hint);
  assert.equal(state.xhrs.length, 0); // the failure never uploads by itself
  assert.equal(state.beforeunloadCount, 1); // still recording
  worklet.emitPcmCount(RATE); // capture keeps running

  await state.stopLive(); // at stop with a failed relay: the stop state at once
  assert.equal(state.xhrs.length, 0); // still no upload
  assert.ok(state.voiceUi().hint.textContent.includes('backend exploded'));
  assert.equal(state.beforeunloadCount, 1); // the guard stays engaged

  context.toggleVoice(); // one click
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1); // exactly one POST
  const upload = state.xhrs[0];
  assert.equal(upload.url, '/api/voice/session-a');
  assert.equal(upload.sentBody.get('backend'), 'gemini'); // the live backend id stays

  upload.respond(200, {text: 'after error'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'after error');
  assert.equal(state.beforeunloadCount, 0);
});

test('a lost socket mid-recording stops visibly and one click uploads the recording', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE);

  socket.dropSocket();
  await tick();
  assert.equal(state.xhrs.length, 0); // the lost relay never uploads by itself
  const hint = state.voiceUi().hint.textContent;
  assert.ok(hint.includes('Gemini 3.5 Transcribe Live'), hint);
  assert.ok(hint.includes('connection lost'), hint);
  assert.equal(state.beforeunloadCount, 1); // still recording

  await state.stopLive(); // the stop state at once
  assert.equal(state.xhrs.length, 0);
  assert.equal(state.beforeunloadCount, 1);

  context.toggleVoice(); // one click
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1);
  const upload = state.xhrs[0];
  assert.equal(upload.sentBody.get('backend'), 'gemini');

  upload.respond(200, {text: 'after close'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'after close');
  assert.equal(state.beforeunloadCount, 0);
});

test('a spent budget stops visibly and one click uploads the recording', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(21.6 * RATE); // 2432 ms of budget for this length

  await state.stopLive();
  assert.equal(state.xhrs.length, 0); // the run waits for the final

  state.timerByMs(2432).fn(); // the budget expires
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 0); // expiry uploads nothing
  assert.ok(!socket.closed); // the relay stays open for a late final
  const hint = state.voiceUi().hint.textContent;
  assert.ok(hint.includes('Gemini 3.5 Transcribe Live'), hint);
  assert.ok(hint.includes('no final after 2.4 s, still waiting'), hint);
  assert.equal(state.beforeunloadCount, 1); // the guard stays engaged

  context.toggleVoice(); // one click
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1); // exactly one POST
  const upload = state.xhrs[0];
  assert.equal(upload.sentBody.get('backend'), 'gemini');

  upload.respond(200, {text: 'upload words'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'upload words');
  assert.equal(state.beforeunloadCount, 0);
});

test('a spent budget keeps waiting and a final before the click still wins', async () => {
  const {state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE); // 2020 ms of budget for this length

  await state.stopLive();
  state.timerByMs(2020).fn(); // the budget expires
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 0); // no upload
  assert.ok(!socket.closed); // the relay stayed open

  socket.receiveMessage({type: 'final', text: 'still mine'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'still mine'); // inserted through the final path
  assert.equal(state.xhrs.length, 0); // zero POSTs
  assert.ok(socket.closed); // the relay closed with the run
  assert.equal(state.beforeunloadCount, 0); // the guard is released
  assert.equal(state.sendLockOpens, 0); // the send lock never engaged
});

test('a spent budget then a click uploads once and discards the late final', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE);

  await state.stopLive();
  state.timerByMs(2020).fn(); // the budget expires
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 0);

  context.toggleVoice(); // the stop state's click
  assert.ok(socket.closed); // the relay closes before any upload work starts
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1); // exactly one POST
  const upload = state.xhrs[0];
  assert.equal(upload.sentBody.get('backend'), 'gemini');

  socket.receiveMessage({type: 'final', text: 'too late'}); // the handlers are gone
  await tick();
  assert.equal(state.input.value, ''); // the late final is not inserted

  upload.respond(200, {text: 'upload words'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'upload words'); // the upload result owns the input
  assert.equal(state.beforeunloadCount, 0);
});

test('the upload form carries the devices object beside the backend', async () => {
  const {state} = buildHarness();
  const worklet = await state.arm();
  worklet.emitPcmCount(RATE);

  const upload = await state.stopWithFlush();
  assert.equal(upload.sentBody.get('backend'), 'batch');
  assert.deepEqual(JSON.parse(upload.sentBody.get('devices')), FAKE_DEVICES);

  upload.respond(200, {text: 'words'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'words');
});

test('the batch backend opens no relay and uploads right after stop', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();

  assert.equal(state.sockets.length, 0); // no relay for a non-live backend
  worklet.emitPcmCount(FIVE_SECONDS);
  assert.equal(state.voiceUi().hint.textContent, 'Listening...'); // the ready chunk lit it
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 0); // nothing is sent before the stop

  const upload = await state.stopWithFlush();
  assert.equal(upload.sentBody.has('transcript'), false);
  assert.equal(upload.sentBody.get('backend'), 'batch');

  upload.respond(200, {text: 'recognized words'});
  await tick();
  await tick();
  assert.equal(state.input.value, 'recognized words');
});

test('an empty upload result after the stop-state click inserts nothing', async () => {
  const {context, state} = buildHarness();
  const {worklet, socket} = await armLive(state);
  socket.openSocket();
  worklet.emitPcmCount(RATE); // 2020 ms of budget for this length

  await state.stopLive();
  state.timerByMs(2020).fn(); // the budget expires
  await tick();
  await tick();

  context.toggleVoice(); // the stop state's click
  await tick();
  await tick();
  assert.equal(state.xhrs.length, 1); // exactly one POST

  state.xhrs[0].respond(200, {text: ''});
  await tick();
  await tick();
  assert.equal(state.input.value, ''); // the empty result inserted nothing
  assert.deepEqual(state.toasts, [{msg: 'No speech detected', isError: false}]);
});

test('the caret is disabled while a run is active and back on release', async () => {
  const {context, state} = buildHarness();
  const worklet = await state.arm();
  assert.equal(state.caret.disabled, true);

  const upload = await state.stopWithFlush();
  assert.equal(state.caret.disabled, true); // the upload is still in flight
  upload.respond(200, {text: 'done'});
  await tick();
  await tick();
  assert.equal(state.caret.disabled, false);

  const second = await state.arm(); // a second run locks the caret again
  assert.equal(state.caret.disabled, true);
  second.emitPcmCount(RATE);
  const retryUpload = await state.stopWithFlush();
  retryUpload.respond(500, {error: 'boom'});
  await tick();
  await tick();
  assert.equal(state.caret.disabled, true); // the retry state keeps the lock
});
