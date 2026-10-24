// ---------------------------------------------------------------------------
// voice-input.js backend-menu tests: the caret's dropdown over the template-
// rendered VOICE_BACKENDS. Drives selection, the check mark, disabled entries,
// localStorage persistence, and the default fallback in a fresh vm per test.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readPackageStatic} = require('./read_static');

const BACKENDS = [
  {id: 'batch', label: 'Batch backend', livePartials: false, unavailableReason: null},
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
  el.getBoundingClientRect = () => ({left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0});
  return el;
}

function buildHarness({stored = null, defaultBackend = 'batch', backends = BACKENDS} = {}) {
  const elements = new Map();
  const voiceBtn = makeEl();
  voiceBtn.id = 'voice-btn';
  const caret = makeEl();
  caret.id = 'voice-backend-btn';
  const micGroup = makeEl(); // the template's mic + caret wrapper; the menu anchors here
  caret.parentElement = micGroup;
  elements.set('voice-btn', voiceBtn);
  elements.set('voice-backend-btn', caret);
  const overflow = makeEl(); // the composer's ⋯ menu; the entry joins it at load
  overflow.id = 'overflow-menu';
  elements.set('overflow-menu', overflow);

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
      addEventListener() {},
    },
    Chat: {registerInputMode() {}},
    VOICE_BACKENDS: backends,
    VOICE_DEFAULT_BACKEND: defaultBackend,
    autoResize() {},
    saveDraft() {},
    window: {innerWidth: 1024},
  };
  vm.createContext(sandbox);
  vm.runInContext(readPackageStatic('features', 'voice', 'voice-input.js'), sandbox, {filename: 'voice-input.js'});

  // Open the menu and hand back its entry rows (title row first).
  sandbox.toggleVoiceBackendMenu();
  const menu = micGroup.children[0];
  const rows = menu.children.slice(1);
  return {sandbox, storage, voiceBtn, overflow, menu, rows};
}

test('first paint names the effective backend on the mic tooltip', () => {
  const {voiceBtn} = buildHarness();
  assert.equal(voiceBtn.title, 'Voice input \u00b7 Batch backend');
});

test('the menu lists every backend, checks the selection, and disables the unconfigured one', () => {
  const {menu, rows} = buildHarness();

  assert.equal(menu.children[0].textContent, 'Voice backend');
  assert.deepEqual(
      rows.map((row) => row.children[1].textContent),
      ['Batch backend', 'Gemini 3.5 Transcribe Live', 'Muse Voice Transcribe \u00b7 needs meta.model_api_key']);
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

  assert.equal(voiceBtn.title, 'Voice input \u00b7 Batch backend');
  assert.deepEqual(rows.map((row) => row.children[0].textContent), ['\u2713', '', '']);
});

test('a stored backend that lost its credential falls back until the key arrives', () => {
  const {voiceBtn, rows} = buildHarness({stored: 'muse'});

  // The default serves, while the stored entry stays visible and unselectable.
  assert.equal(voiceBtn.title, 'Voice input \u00b7 Batch backend');
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

test('the ⋯ menu entry opens the same backend menu and a tap picks a backend', () => {
  const {overflow, menu, rows, storage} = buildHarness();
  const entry = overflow.children.find((child) => child.id === 'voice-backend-menu-entry');
  assert.equal(entry.textContent, 'Voice backend');

  menu.classList.add('hidden');
  overflow.classList.add('show');
  entry.onclick();

  assert.ok(!menu.classList.contains('hidden'));
  assert.ok(!overflow.classList.contains('show'));

  rows[1].onclick();
  assert.equal(storage['charliebot-voice-backend'], 'gemini');
  assert.ok(menu.classList.contains('hidden'));
});

test('the backend menu moves right when right alignment would cross the left screen edge', () => {
  const {sandbox, menu} = buildHarness();
  // A 320 px screen whose mic group ends at 100 px: right-aligned, the 256 px menu starts at -156 px.
  // The layout is modelled: a right offset moves the menu's right edge left of the group's edge.
  sandbox.window.innerWidth = 320;
  menu.getBoundingClientRect = () => {
    const right = 100 - parseFloat(menu.style.right || '0');
    return {left: right - 256, right, top: 0, bottom: 0, width: 256, height: 0};
  };
  // Close, then reopen so the menu is positioned for this screen.
  sandbox.toggleVoiceBackendMenu();
  sandbox.toggleVoiceBackendMenu();
  const rect = menu.getBoundingClientRect();
  assert.ok(rect.left >= 8, `the menu's left edge ${rect.left} crosses the left screen edge`);
  assert.ok(rect.right <= 312, `the menu's right edge ${rect.right} crosses the right screen edge`);
});
