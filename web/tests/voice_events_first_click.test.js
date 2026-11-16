// ---------------------------------------------------------------------------
// voice-events.js startup tests: the click requests the microphone immediately,
// the module loads concurrently, and the recording waits until both are ready.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readPackageStatic} = require('./read_static');

const VOICE_EVENTS_JS = readPackageStatic('features', 'voice', 'voice-events.js');

function makeEl(id = '') {
  const kids = [];
  const classes = new Set();
  const attributes = {};
  return {
    id,
    type: '',
    textContent: '',
    title: '',
    dataset: {},
    attributes,
    children: kids,
    classList: {
      contains: (name) => classes.has(name),
      add: (...names) => names.forEach((n) => classes.add(n)),
      remove: (...names) => names.forEach((n) => classes.delete(n)),
    },
    appendChild: (child) => kids.push(child),
    setAttribute: (name, value) => { attributes[name] = value; },
  };
}

function buildHarness() {
  const ran = [];
  const calls = [];
  const loads = [];
  const idleCallbacks = [];
  const documentListeners = new Map();
  const overflow = makeEl('overflow-menu');
  const button = makeEl('voice-btn');
  let stoppedTracks = 0;
  const stream = {getTracks: () => [{stop() { stoppedTracks += 1; }}]};
  const audioContexts = [];
  class FakeAudioContext {
    constructor() {
      this.state = 'running';
      this.sampleRate = 48000;
      audioContexts.push(this);
    }
    resume() { calls.push('resume'); return Promise.resolve(); }
    close() { this.closed = true; return Promise.resolve(); }
  }
  const sandbox = {
    console: {error: () => {}, warn: () => {}},
    localStorage: {getItem: () => null},
    SESSION_ID: 'synthetic-session',
    VOICE_BACKENDS: [{id: 'batch', label: 'Batch backend', livePartials: false, unavailableReason: null}],
    VOICE_DEFAULT_BACKEND: 'batch',
    Chat: {registerInputMode() {}},
    navigator: {mediaDevices: {getUserMedia: () => {
      calls.push('getUserMedia');
      return Promise.resolve(stream);
    }}},
    performance: {now: () => Date.now()},
    document: {
      getElementById: (id) => (id === 'overflow-menu' ? overflow : id === 'voice-btn' ? button : null),
      createElement: () => makeEl(''),
      addEventListener: (name, callback) => documentListeners.set(name, callback),
    },
    window: {
      AudioContext: FakeAudioContext,
      addEventListener: (name, callback) => { if (name === 'load') loads.push(callback); },
      requestIdleCallback: (callback) => idleCallbacks.push(callback),
    },
    loadPageStyles: () => { calls.push('styles'); return Promise.resolve(); },
    // The module lands after a delay and defines its functions on the global scope.
    loadPageScripts: () => new Promise((resolve) => setTimeout(() => {
      calls.push('module-loaded');
      sandbox.toggleVoice = (request) => {
        ran.push('toggleVoice');
        sandbox.startRequest = request;
        return Promise.resolve();
      };
      sandbox.toggleVoiceBackendMenu = () => ran.push('toggleVoiceBackendMenu');
      sandbox.openVoiceBackendMenu = () => ran.push('openVoiceBackendMenu');
      resolve();
    }, 5)),
  };
  vm.createContext(sandbox);
  vm.runInContext(VOICE_EVENTS_JS, sandbox, {filename: 'voice-events.js'});
  return {
    sandbox,
    overflow,
    button,
    ran,
    calls,
    loads,
    idleCallbacks,
    stream,
    audioContexts,
    stoppedTracks: () => stoppedTracks,
    teardownSession: () => documentListeners.get('charliebot:session-view-teardown')(),
  };
}

test('first click requests the microphone immediately and waits for the module', async () => {
  const {sandbox, button, ran, calls, stream} = buildHarness();
  assert.doesNotThrow(() => sandbox.openVoiceInput());
  assert.deepEqual(calls, ['resume', 'getUserMedia', 'styles']);
  assert.equal(button.dataset.state, 'starting');
  assert.equal(button.title, 'Preparing microphone…');
  assert.ok(button.classList.contains('bg-amber-600'));

  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['toggleVoice']);
  assert.equal(sandbox.startRequest.sessionId, 'synthetic-session');
  assert.equal(sandbox.startRequest.stream, stream);
  assert.ok(sandbox.startRequest.audioContext);
  assert.ok(sandbox.startRequest.clickedAt <= Date.now());
  assert.ok(calls.indexOf('getUserMedia') < calls.indexOf('module-loaded'));
});

test('idle preload starts only after the page load event', () => {
  const {loads, idleCallbacks, calls} = buildHarness();
  assert.equal(loads.length, 1);
  assert.deepEqual(calls, []);
  loads[0]();
  assert.equal(idleCallbacks.length, 1);
  assert.deepEqual(calls, []);
  idleCallbacks[0]();
  assert.deepEqual(calls, ['styles']);
});

test('session teardown cancels a first click while the voice module is loading', async () => {
  const {sandbox, ran, audioContexts, stoppedTracks, teardownSession} = buildHarness();
  sandbox.openVoiceInput();
  teardownSession();

  await new Promise((resolve) => setTimeout(resolve, 30));

  assert.deepEqual(ran, []);
  assert.equal(stoppedTracks(), 1); // the mic resolves after the teardown
  assert.equal(audioContexts[0].closed, true);
});

test('first click on the backend caret defers module functions until voice-input.js lands', async () => {
  const {sandbox, ran} = buildHarness();
  assert.doesNotThrow(() => sandbox.toggleVoiceBackend());
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['toggleVoiceBackendMenu']);
});

test('first click on the overflow entry defers module functions until voice-input.js lands', async () => {
  const {overflow, ran} = buildHarness();
  const entry = overflow.children.find((child) => child.id === 'voice-backend-menu-entry');
  assert.ok(entry, 'voice-backend-menu-entry was not added to the overflow menu');
  overflow.classList.add('show');
  assert.doesNotThrow(() => entry.onclick());
  assert.equal(overflow.classList.contains('show'), false);
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['openVoiceBackendMenu']);
});
