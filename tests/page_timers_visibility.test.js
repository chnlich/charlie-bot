// ---------------------------------------------------------------------------
// A hidden tab must do no periodic work: no session-status, TUI-status,
// worker-list, active-session-view, thread-detail or ext-usage poll, and no
// thinking tick. Every timer goes through the page-timers registry, so this
// exercises the real registry plus the real call sites in app.js, the sidebar
// modules, workers.js and ext_usage.js.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const PAGE_TIMERS_JS = readStatic('page-timers.js');
const COMPAT_LOADER_JS = readStatic('compat-loader.js');
const CHAT_JS = readStatic('chat.js');
const SIDEBAR_JS = readStatic('sidebar.js');
const APP_JS = readStatic('app.js');
const WORKERS_JS = readStatic('workers.js');
const EXT_USAGE_JS = readStatic('ext_usage.js');

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
    hasListener(type) {
      return (listeners.get(type) || []).length > 0;
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

function buildRegistryContext() {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const context = {
    document,
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    console: {error() {}, warn() {}, log() {}},
  };
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  return {context, document, timers};
}

// ---------------------------------------------------------------------------
// The registry itself
// ---------------------------------------------------------------------------

test('startPageTimer creates no interval while the page is hidden', () => {
  const {context, document, timers} = buildRegistryContext();
  document.hidden = true;

  context.startPageTimer('poll', () => {}, 3000);

  assert.equal(timers.live().length, 0);
  assert.equal(context.pageTimerRegistered('poll'), true, 'the timer stays registered while suspended');
});

test('hiding the page tears down live intervals and showing it recreates them', () => {
  const {context, document, timers} = buildRegistryContext();
  let ticks = 0;
  context.startPageTimer('poll', () => { ticks += 1; }, 3000);
  assert.equal(timers.live().length, 1);

  document.hidden = true;
  document.dispatch('visibilitychange');
  assert.equal(timers.live().length, 0, 'no interval survives the hide');

  document.hidden = false;
  document.dispatch('visibilitychange');
  const live = timers.live();
  assert.equal(live.length, 1);
  assert.equal(live[0].ms, 3000, 'the original cadence is restored');
  live[0].fn();
  assert.equal(ticks, 1);
});

test('resume handlers run on show, before the cadences restart', () => {
  const {context, document, timers} = buildRegistryContext();
  const order = [];
  context.startPageTimer('poll', () => {}, 3000);
  context.onPageResume(() => order.push('resume:' + timers.live().length));

  document.hidden = true;
  document.dispatch('visibilitychange');
  assert.deepEqual(order, [], 'no resume handler runs on hide');

  document.hidden = false;
  document.dispatch('visibilitychange');
  assert.deepEqual(order, ['resume:0'], 'the snapshot fetch happens before the intervals are back');
  assert.equal(timers.live().length, 1);
});

test('stopPageTimer clears the interval and drops the registration', () => {
  const {context, timers} = buildRegistryContext();
  context.startPageTimer('poll', () => {}, 3000);

  context.stopPageTimer('poll');

  assert.equal(timers.live().length, 0);
  assert.equal(context.pageTimerRegistered('poll'), false);
});

// ---------------------------------------------------------------------------
// The real sidebar timers
// ---------------------------------------------------------------------------

function buildSidebarContext() {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const context = {
    document,
    SESSION_ID: 'session-a',
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
    renderWorkersTab() {},
    updateWorkersTabBadge() {},
    switchTab() {},
    showToast() {},
    loadedThreads: {clear() {}},
    BACKEND_OPTIONS: {},
    BACKEND_TYPES: {},
  };
  context.window = {addEventListener() {}, innerHeight: 800};
  context.CSS = {escape: (v) => String(v)};
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(COMPAT_LOADER_JS, context, {filename: 'compat-loader.js'});
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, document, timers};
}

test('sidebar timers stay dormant while hidden and all start on show', () => {
  const {context, document, timers} = buildSidebarContext();
  document.elements.set('thinking', {classList: {add() {}, remove() {}}});
  document.elements.set('thinking-time', {textContent: ''});
  document.elements.set('send-btn', {disabled: false, classList: {add() {}, remove() {}}});
  document.hidden = true;

  context.startTuiStatusPolling();
  context.startThinking({keepSendEnabled: true});
  context.THINKING_SINCE = '2026-08-02T00:00:00Z';
  context.ensureActiveSessionViewPolling();
  context.restartWorkersPolling();

  assert.equal(timers.live().length, 0, 'a hidden tab runs no sidebar timer');

  document.hidden = false;
  document.dispatch('visibilitychange');

  assert.deepEqual(
    timers.live().map((t) => t.ms).sort((a, b) => a - b),
    [1000, 3000, 3000, 3000],
    'thinking tick plus the three 3s polls resume together'
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

// ---------------------------------------------------------------------------
// The adaptive sidebar-status poll in app.js
// ---------------------------------------------------------------------------

function buildAppContext() {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const calls = [];
  const noop = (name) => () => { calls.push(name); };
  const context = {
    document,
    SESSION_ID: 'session-a',
    SESSION_BOOTSTRAP: null,
    THINKING_SINCE: null,
    DRAFT_KEY: null,
    statusPollMs: 3000,
    switching: false,
    ws: null,
    reconnectTimer: null,
    reconnectDelay: 1000,
    WebSocket: {OPEN: 1},
    autoResize() {},
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    console: {error() {}, warn() {}, log() {}},
    initAuth: noop('initAuth'),
    initSidebarResize: noop('initSidebarResize'),
    initLatexResize: noop('initLatexResize'),
    initBacklogResize: noop('initBacklogResize'),
    fetchSlashCommands: noop('fetchSlashCommands'),
    startTuiStatusPolling: noop('startTuiStatusPolling'),
    restoreSidebarFromUrl: noop('restoreSidebarFromUrl'),
    updateRelativeTimes: noop('updateRelativeTimes'),
    postProcessRenderedMessages: noop('postProcessRenderedMessages'),
    initScrollPagination: noop('initScrollPagination'),
    connectWS: noop('connectWS'),
    scheduleLazySessionDataLoad: noop('scheduleLazySessionDataLoad'),
    ensureActiveSessionViewPolling: noop('ensureActiveSessionViewPolling'),
    refreshSessionStatusNow: noop('refreshSessionStatusNow'),
    fetchTuiStatus: noop('fetchTuiStatus'),
    pollActiveSessionView: noop('pollActiveSessionView'),
    updateThinkingTime: noop('updateThinkingTime'),
    pollSessionStatus: async () => { calls.push('pollSessionStatus'); return false; },
    platform: {onChange() {}},
    localStorage: {getItem: () => null, setItem() {}},
  };
  context.window = {addEventListener() {}};
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(APP_JS, context, {filename: 'app.js'});
  return {context, document, timers, calls};
}

test('app.js schedules no sidebar-status poll while the tab loads hidden', () => {
  const {document, timers, calls} = buildAppContext();
  document.hidden = true;

  document.dispatch('DOMContentLoaded');

  assert.equal(timers.live().length, 0, 'the adaptive status poll does not run in a hidden tab');

  document.hidden = false;
  document.dispatch('visibilitychange');

  const live = timers.live();
  assert.equal(live.length, 1);
  assert.equal(live[0].ms, 3000, 'the poll resumes on the cadence it was registered with');
  assert.ok(calls.includes('refreshSessionStatusNow'), 'the sidebar scope is refreshed immediately on show');
  assert.ok(calls.includes('fetchTuiStatus'), 'TUI dots are refreshed immediately on show');
  assert.ok(calls.includes('updateThinkingTime'), 'the thinking display is recomputed on show');
});

// ---------------------------------------------------------------------------
// The workers.js thread-detail polls and the ext_usage.js timers
// ---------------------------------------------------------------------------

function buildWorkersContext(running) {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const fetches = [];
  document.elements.set('thread-detail-t1', {classList: {contains: () => false}});
  document.elements.set('thread-dot-t1', {classList: {contains: (cls) => running && cls === 'bg-blue-500'}});
  document.elements.set('thread-events-t1', {
    innerHTML: '',
    dataset: {},
    parentElement: {querySelector: () => null, insertBefore() {}},
  });
  const context = {
    document,
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    console: {error() {}, warn() {}, log() {}},
    fetch: async (url) => {
      fetches.push(url);
      return {ok: true, json: async () => []};
    },
  };
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(WORKERS_JS, context, {filename: 'workers.js'});
  return {context, document, timers, fetches};
}

test('a running worker detail poll stays dormant while hidden and resumes on show', async () => {
  const {context, document, timers, fetches} = buildWorkersContext(true);
  document.hidden = true;

  context.startThreadPoll('t1', 'session-a');

  assert.equal(timers.live().length, 0, 'the thread poll registers no interval in a hidden tab');
  assert.equal(context.pageTimerRegistered('thread-events-t1'), true, 'the poll keeps its registration');

  document.hidden = false;
  document.dispatch('visibilitychange');
  const live = timers.live();
  assert.equal(live.length, 1);
  assert.equal(live[0].ms, 5000, 'the 5s cadence resumes on show');
  await live[0].fn();
  assert.equal(fetches.length, 2, 'one tick fetches the events and metadata pair');

  document.hidden = true;
  document.dispatch('visibilitychange');
  assert.equal(timers.live().length, 0, 'hiding suspends the poll again');

  context.stopAllThreadPolls();
  assert.equal(context.pageTimerRegistered('thread-events-t1'), false, 'session switch drops the registration');
});

test('a finished worker detail poll does the final fetch then unregisters', async () => {
  const {context, timers, fetches} = buildWorkersContext(false);

  context.startThreadPoll('t1', 'session-a');
  assert.equal(timers.live().length, 1);

  await timers.live()[0].fn();

  assert.equal(fetches.length, 2, 'the finishing tick still fetches the events and metadata pair');
  assert.equal(context.pageTimerRegistered('thread-events-t1'), false, 'the poll stops itself');
  assert.equal(timers.live().length, 0);
});

test('a collapsed worker detail poll clears its registration', () => {
  const {context, timers} = buildWorkersContext(true);

  context.startThreadPoll('t1', 'session-a');
  assert.equal(timers.live().length, 1);

  context.stopThreadPoll('t1');

  assert.equal(timers.live().length, 0);
  assert.equal(context.pageTimerRegistered('thread-events-t1'), false);
});

function buildExtUsageContext() {
  const document = createFakeDocument();
  const timers = createTimerHarness();
  const fetches = [];
  const context = {
    document,
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    console: {error() {}, warn() {}, log() {}},
    fetch: async (url) => {
      fetches.push(url);
      return {ok: true, json: async () => []};
    },
    localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
    // config.js's shared meter-fill literal; this harness skips config.js.
    // Today's [] payload never paints a bucket, but a provider-bearing fixture
    // must fail on assertions, not on a missing global.
    PROGRESS_BAR_FILL_CLASS: 'h-full rounded-full transition-all duration-300',
  };
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(EXT_USAGE_JS, context, {filename: 'ext_usage.js'});
  return {context, document, timers, fetches};
}

test('the ext-usage strip runs no timer while hidden and both cadences resume on show', async () => {
  const {document, timers, fetches} = buildExtUsageContext();
  document.hidden = true;

  document.dispatch('DOMContentLoaded');
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(fetches.length, 1, 'the one-time bootstrap fetch still runs');
  assert.equal(timers.live().length, 0, 'no reset tick or usage poll runs in a hidden tab');

  document.hidden = false;
  document.dispatch('visibilitychange');

  assert.deepEqual(
    timers.live().map((t) => t.ms).sort((a, b) => a - b),
    [60000, 600000],
    'the 60s repaint and the 10min refetch resume together'
  );
});
