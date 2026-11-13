// ---------------------------------------------------------------------------
// voice-events.js first-click regression tests: verify that the mic button, the
// caret, and the composer's ⋯ menu entry defer their function lookups until
// voice-input.js has loaded, rather than throwing ReferenceError synchronously
// at click time on a fresh page.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readPackageStatic} = require('./read_static');

const VOICE_EVENTS_JS = readPackageStatic('features', 'voice', 'voice-events.js');

function makeEl(id = '') {
  const kids = [];
  const classes = new Set();
  return {
    id,
    type: '',
    textContent: '',
    title: '',
    children: kids,
    classList: {
      contains: (name) => classes.has(name),
      add: (...names) => names.forEach((n) => classes.add(n)),
      remove: (...names) => names.forEach((n) => classes.delete(n)),
    },
    appendChild: (child) => kids.push(child),
  };
}

function buildHarness() {
  const ran = [];
  const overflow = makeEl('overflow-menu');
  const sandbox = {
    console: {error: () => {}},
    localStorage: {getItem: () => null},
    VOICE_BACKENDS: [{id: 'batch', label: 'Batch backend', livePartials: false, unavailableReason: null}],
    VOICE_DEFAULT_BACKEND: 'batch',
    Chat: {registerInputMode() {}},
    document: {
      getElementById: (id) => (id === 'overflow-menu' ? overflow : id === 'voice-btn' ? makeEl('voice-btn') : null),
      createElement: () => makeEl(''),
    },
    loadPageStyles: () => Promise.resolve(),
    // The module lands after a delay and defines its functions on the global scope.
    loadPageScripts: () => new Promise((resolve) => setTimeout(() => {
      sandbox.toggleVoice = () => ran.push('toggleVoice');
      sandbox.toggleVoiceBackendMenu = () => ran.push('toggleVoiceBackendMenu');
      sandbox.openVoiceBackendMenu = () => ran.push('openVoiceBackendMenu');
      resolve();
    }, 5)),
  };
  vm.createContext(sandbox);
  // Run only voice-events.js before the click: voice-input.js has not run yet.
  vm.runInContext(VOICE_EVENTS_JS, sandbox, {filename: 'voice-events.js'});
  return {sandbox, overflow, ran};
}

test('first click on the mic button defers toggleVoice until voice-input.js lands', async () => {
  const {sandbox, ran} = buildHarness();
  assert.doesNotThrow(() => {
    sandbox.openVoiceInput();
  });
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['toggleVoice']);
});

test('first click on the backend caret defers toggleVoiceBackendMenu until voice-input.js lands', async () => {
  const {sandbox, ran} = buildHarness();
  assert.doesNotThrow(() => {
    sandbox.toggleVoiceBackend();
  });
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['toggleVoiceBackendMenu']);
});

test('first click on the overflow menu entry defers openVoiceBackendMenu until voice-input.js lands', async () => {
  const {overflow, ran} = buildHarness();
  const entry = overflow.children.find((child) => child.id === 'voice-backend-menu-entry');
  assert.ok(entry, 'voice-backend-menu-entry was not added to the overflow menu');
  overflow.classList.add('show');
  assert.doesNotThrow(() => {
    entry.onclick();
  });
  assert.equal(overflow.classList.contains('show'), false);
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(ran, ['openVoiceBackendMenu']);
});
