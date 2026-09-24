// ---------------------------------------------------------------------------
// voice-input.js backend-menu tests: the caret's dropdown over the template-
// rendered VOICE_BACKENDS. Drives selection, the check mark, disabled entries,
// localStorage persistence, and the default fallback in a fresh vm per test.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

const BACKENDS = [
  {id: 'local', label: 'Local (sherpa)', livePartials: false, unavailableReason: null},
  {id: 'gemini', label: 'Gemini 3.5 Transcribe Live', livePartials: true, unavailableReason: null},
  {id: 'muse', label: 'Muse Voice Transcribe', livePartials: true, unavailableReason: 'needs meta.model_api_key'},
];

function makeEl() {
  const kids = [];
  const classes = new Set();
  const el = {
    id: '', textContent: '', title: '', type: '', disabled: false, style: {},
    classList: {
      contains: (name) => classes.has(name),
      add: (...names) => names.forEach((n) => classes.add(n)),
      remove: (...names) => names.forEach((n) => classes.delete(n)),
      toggle: (name) => (classes.has(name) ? classes.delete(name) : classes.add(name)),
    },
  };
  // className and classList agree, as they do on a real element.
  Object.defineProperty(el, 'className', {
    get: () => Array.from(classes).join(' '),
    set: (value) => {
      classes.clear();
      value.split(/\s+/).filter(Boolean).forEach((name) => classes.add(name));
    },
  });
  el.children = kids;
  el.appendChild = (child) => kids.push(child);
  return el;
}

function buildHarness({stored = null, defaultBackend = 'local', backends = BACKENDS} = {}) {
  const elements = new Map();
  const voiceBtn = makeEl();
  voiceBtn.id = 'voice-btn';
  const caret = makeEl();
  caret.id = 'voice-backend-btn';
  const micGroup = makeEl(); // the template's mic + caret wrapper; the menu anchors here
  caret.parentElement = micGroup;
  elements.set('voice-btn', voiceBtn);
  elements.set('voice-backend-btn', caret);

  const storage = {};
  if (stored !== null) storage['charliebot-voice-backend'] = stored;

  const sandbox = {
    console,
    localStorage: {
      getItem: (key) => (key in storage ? storage[key] : null),
      setItem: (key, value) => {
        storage[key] = String(value);
      },
      removeItem: (key) => {
        delete storage[key];
      },
    },
    document: {
      getElementById: (id) => elements.get(id) || null,
      createElement: () => makeEl(),
    },
    VOICE_BACKENDS: backends,
    VOICE_DEFAULT_BACKEND: defaultBackend,
    autoResize() {},
    saveDraft() {},
  };
  vm.createContext(sandbox);
  vm.runInContext(readStatic('voice-input.js'), sandbox, {filename: 'voice-input.js'});

  // Open the menu and hand back its entry rows (title row first).
  sandbox.toggleVoiceBackendMenu();
  const menu = micGroup.children[0];
  const rows = menu.children.slice(1);
  return {sandbox, storage, voiceBtn, menu, rows};
}

test('first paint names the effective backend on the mic tooltip', () => {
  const {voiceBtn} = buildHarness();
  assert.equal(voiceBtn.title, 'Voice input \u00b7 Local (sherpa)');
});

test('the menu lists every backend, checks the selection, and disables the unconfigured one', () => {
  const {menu, rows} = buildHarness();

  assert.equal(menu.children[0].textContent, 'Voice backend');
  assert.deepEqual(
      rows.map((row) => row.children[1].textContent),
      ['Local (sherpa)', 'Gemini 3.5 Transcribe Live', 'Muse Voice Transcribe \u00b7 needs meta.model_api_key']);
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['\u2713', '', '']);
  assert.deepEqual(rows.map((row) => row.disabled), [false, false, true]);
  assert.equal(rows[2].title, 'needs meta.model_api_key'); // the missing key is named, never its value
});

test('picking a backend persists it, moves the check, and retitles the mic', () => {
  const {storage, voiceBtn, menu, rows} = buildHarness();

  rows[1].onclick();

  assert.equal(storage['charliebot-voice-backend'], 'gemini');
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['', '\u2713', '']);
  assert.equal(voiceBtn.title, 'Voice input \u00b7 Gemini 3.5 Transcribe Live');
  assert.ok(menu.classList.contains('hidden')); // the menu closed with the pick
});

test('a stored id the page does not know falls back to the server default', () => {
  const {voiceBtn, rows} = buildHarness({stored: 'geminii'});

  assert.equal(voiceBtn.title, 'Voice input \u00b7 Local (sherpa)');
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['\u2713', '', '']);
});

test('a stored backend that lost its credential falls back until the key arrives', () => {
  const {voiceBtn, rows} = buildHarness({stored: 'muse'});

  // The default serves, while the stored entry stays visible and unselectable.
  assert.equal(voiceBtn.title, 'Voice input \u00b7 Local (sherpa)');
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['\u2713', '', '']);
  assert.equal(rows[2].disabled, true);
});

test('the stored choice wins again once it is configured', () => {
  const {voiceBtn, rows} = buildHarness({stored: 'gemini'});

  assert.equal(voiceBtn.title, 'Voice input \u00b7 Gemini 3.5 Transcribe Live');
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['', '\u2713', '']);
});

test('the caret toggles the menu open and closed', () => {
  const {sandbox, menu} = buildHarness();

  assert.ok(!menu.classList.contains('hidden')); // the harness's open
  sandbox.toggleVoiceBackendMenu();
  assert.ok(menu.classList.contains('hidden'));
  sandbox.toggleVoiceBackendMenu();
  assert.ok(!menu.classList.contains('hidden'));
});
