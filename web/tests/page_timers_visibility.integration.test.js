// ---------------------------------------------------------------------------
// A hidden tab must do no periodic work: no session-status,
// active-session-view, transcript or ext-usage poll, and no thinking tick.
// Every timer goes through the page-timers registry, so this exercises the
// real registry plus the real call sites in app.js, the sidebar modules and
// ext_usage.js.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, chatModules, sidebarModules, runStaticModules} = require('./read_static');
const {standInForUsageJs} = require('./session_context_stub');

const PAGE_TIMERS_JS = readStatic('page-timers.js');
function createFakeDocument() {
  const listeners = new Map();
  return {
    hidden: false,
    elements: new Map(),
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(fn);
    },
    removeEventListener() {},
    dispatch(type) {
      (listeners.get(type) || []).forEach((fn) => fn());
    },
    getElementById(id) {
      return this.elements.get(id) || null;
    },
    querySelector: () => null,
    querySelectorAll: () => [],
    createElement: () => ({style: {}, classList: {add() {}, remove() {}, toggle() {}}, setAttribute() {}}),
    body: {appendChild() {}, style: {}},
  };
}

function createTimerHarness() {
  const intervals = new Map();
  const cleared = [];
  let nextId = 1;
  return {
    intervals,
    cleared,
    live() {
      return Array.from(intervals.values());
    },
    setInterval(fn, ms) {
      const id = nextId++;
      intervals.set(id, {id, fn, ms});
      return id;
    },
    clearInterval(id) {
      cleared.push(id);
      intervals.delete(id);
    },
  };
}

// ---------------------------------------------------------------------------
// The real sidebar timers
// ---------------------------------------------------------------------------

function buildSidebarContext() {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const context = {
    document,
    SESSION_ID: 'session-a',
    // index.html's host fact; the displayed-session module restates it in the tab title.
    PAGE_HOSTNAME: 'localhost:8000',
    SESSION_NAME: 'Session session-a',
    platform: {isMobile: false, onChange() {}},
    THINKING_SINCE: null,
    masterThinking: false,
    switching: false,
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    setTimeout: () => 0,
    clearTimeout: () => {},
    console: {error() {}, warn() {}, log() {}},
    fetch: async () => ({ok: true, json: async () => ({}), text: async () => ''}),
    localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: ''},
    history: {pushState() {}},
    URLSearchParams,
    marked: {parse: (t) => t},
    fixNestedFences: (t) => t,
    escapeHtml: (v) => String(v),
    hljs: {highlight: (v) => ({value: v})},
    shouldAutoScroll: () => false,
    relativeTime: (t) => t,
    updateRelativeTimes() {},
    formatTokens: (n) => String(n),
    switchTab() {},
    showToast() {},
    BACKEND_OPTIONS: {},
    BACKEND_TYPES: {},
  };
  context.window = {addEventListener() {}, innerHeight: 800};
  context.CSS = {escape: (v) => String(v)};
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  runStaticModules(context, chatModules());
  standInForUsageJs(context);
  runStaticModules(context, sidebarModules());
  return {context, document, timers};
}

test('sidebar timers stay dormant while hidden and all start on show', () => {
  const {context, document, timers} = buildSidebarContext();
  document.elements.set('thinking', {classList: {add() {}, remove() {}}});
  document.elements.set('thinking-time', {textContent: ''});
  document.elements.set('send-btn', {disabled: false, classList: {add() {}, remove() {}}});
  document.hidden = true;

  context.startThinking({keepSendEnabled: true});
  context.THINKING_SINCE = '2026-08-02T00:00:00Z';
  context.ensureActiveSessionViewPolling();

  assert.equal(timers.live().length, 0, 'a hidden tab runs no sidebar timer');

  document.hidden = false;
  document.dispatch('visibilitychange');

  assert.deepEqual(
    timers.live().map((t) => t.ms).sort((a, b) => a - b),
    [1000, 3000],
    'thinking tick plus the 3s session-view poll resume together'
  );
});

test('the thinking display is recomputed from thinkingStart across a hide/show cycle', () => {
  const {context, document} = buildSidebarContext();
  const timeEl = {textContent: ''};
  document.elements.set('thinking', {classList: {add() {}, remove() {}}});
  document.elements.set('thinking-time', timeEl);
  document.elements.set('send-btn', {disabled: false, classList: {add() {}, remove() {}}});

  context.thinkingStart = Date.now() - 5000;
  context.startThinking({keepSendEnabled: true});
  assert.equal(timeEl.textContent, '5s');

  document.hidden = true;
  document.dispatch('visibilitychange');
  timeEl.textContent = 'stale';

  // 5 more seconds pass while the tab is hidden and the tick is suspended.
  context.thinkingStart -= 5000;
  context.updateThinkingTime();
  assert.equal(timeEl.textContent, '10s', 'elapsed time comes from thinkingStart, not from tick count');
});
