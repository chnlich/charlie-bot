// ---------------------------------------------------------------------------
// The Workspace list refreshes the rows a task-tree frame names, not the whole
// list. These tests run the real page-timers, chat and sidebar modules and
// websocket.js with websocket-handlers.js, in index.html order, and drive them
// through a fake socket, a fake document and a recording fetch.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, chatModules, sidebarModules, runStaticModules} = require('./read_static');
const {createFakeWebSocketClass} = require('./fake_websocket');
const {standInForUsageJs} = require('./session_context_stub');

const PAGE_TIMERS_JS = readStatic('page-timers.js');
const WEBSOCKET_JS = readStatic('websocket.js');
const WEBSOCKET_HANDLERS_JS = readStatic('websocket-handlers.js');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// One open page on the Workspace view. `painted` names the sessions the sidebar paints as rows.
function buildPage({painted = []} = {}) {
  const listeners = new Map();
  const elements = new Map([['session-list', {innerHTML: ''}]]);
  for (const sid of painted) elements.set('session-' + sid, {id: 'session-' + sid});
  const timers = [];
  const page = {elements, requests: [], cronLoads: 0, reloads: 0, storage: new Map()};
  const document = {
    hidden: false,
    visibilityState: 'visible',
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(fn);
    },
    removeEventListener() {},
    dispatchEvent: () => true,
    dispatch(type) {
      (listeners.get(type) || []).forEach((fn) => fn());
    },
    getElementById: (id) => elements.get(id) || null,
    querySelector: () => null,
    querySelectorAll(selector) {
      if (selector === 'a[id^="session-"]') return painted.map((sid) => ({id: 'session-' + sid}));
      return [];
    },
    createElement: () => ({style: {}, classList: {add() {}, remove() {}, toggle() {}}, setAttribute() {}}),
    body: {appendChild() {}, style: {}},
  };
  const context = {
    SESSION_ID: 'session-a',
    SESSION_NAME: 'Session A',
    eventCursor: 0,
    STATIC_ASSET_VERSION: 'stub-asset-version',
    currentFilter: 'all',
    PAGE_HOSTNAME: 'localhost:8000',
    document,
    CustomEvent,
    TextEncoder,
    URLSearchParams,
    console: {error() {}, warn() {}, log() {}},
    setInterval: () => 0,
    clearInterval() {},
    setTimeout: (fn) => {
      timers.push(fn);
      return timers.length;
    },
    clearTimeout() {},
    fetch: (url) => new Promise((resolve, reject) => {
      page.requests.push({
        url,
        respond: (body) => resolve({ok: true, json: async () => body}),
        fail: (error) => reject(error),
      });
    }),
    location: {href: '', protocol: 'http:', host: 'localhost', search: '', reload: () => { page.reloads += 1; }},
    localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
    sessionStorage: {
      getItem: (key) => page.storage.get(key) ?? null,
      setItem: (key, value) => page.storage.set(key, value),
    },
    WebSocket: createFakeWebSocketClass(),
    wsUrlWithToken: (path) => path,
    window: {addEventListener() {}, setInterval: () => 0, innerHeight: 800},
    marked: {parse: (text) => text},
    escapeHtml: (value) => String(value),
    relativeTime: (value) => String(value),
    updateRelativeTimes() {},
    platform: {isMobile: false, onChange() {}},
    switching: false,
    masterThinking: false,
    THINKING_SINCE: null,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  runStaticModules(context, chatModules());
  standInForUsageJs(context);
  runStaticModules(context, sidebarModules());
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  vm.runInContext(WEBSOCKET_HANDLERS_JS, context, {filename: 'websocket-handlers.js'});
  context.registerWorkspaceBadge(() => {
    page.cronLoads += 1;
    return Promise.resolve('');
  });

  const sockets = context.WebSocket.instances;
  page.context = context;
  page.document = document;
  page.connect = () => vm.runInContext('connectWS()', context);
  page.socket = () => sockets[sockets.length - 1];
  page.frame = (event) => page.socket().emitMessage(event);
  page.runTimers = () => timers.splice(0).forEach((fn) => fn());
  page.hide = () => {
    document.hidden = true;
    document.visibilityState = 'hidden';
    document.dispatch('visibilitychange');
  };
  page.show = () => {
    document.hidden = false;
    document.visibilityState = 'visible';
    document.dispatch('visibilitychange');
  };
  page.urlsStarting = (prefix) => page.requests.map((request) => request.url).filter((url) => url.startsWith(prefix));
  page.rowUrls = () => page.urlsStarting('/api/sessions/?compact=1&ids=');
  page.fullListCount = () => page.requests.filter((request) => request.url === '/api/sessions/?compact=1').length;
  return page;
}

// Answers every row request still in flight with the empty row set: the merge then has no painted list to repaint.
function answerRows(page) {
  page.requests
      .filter((request) => request.url.includes('&ids='))
      .forEach((request) => request.respond({sessions: {}, gone: []}));
}

test('a task-tree frame fetches its row, and frames that arrive in flight form the next request', async () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();

  page.frame({type: 'task_tree_changed', session_id: 'session-x'});
  assert.deepEqual(page.rowUrls(), ['/api/sessions/?compact=1&ids=session-x']);

  page.frame({type: 'task_tree_changed', session_id: 'session-y'});
  page.frame({type: 'task_tree_changed', session_id: 'session-z'});
  assert.equal(page.rowUrls().length, 1, 'frames during the in-flight fetch wait for it');

  answerRows(page);
  await flush();
  assert.deepEqual(page.rowUrls(), [
    '/api/sessions/?compact=1&ids=session-x',
    '/api/sessions/?compact=1&ids=session-y,session-z',
  ]);
  assert.equal(page.fullListCount(), 0, 'a task-tree frame never refetches the whole list');
});

test('a hidden tab requests no row or cron list until it is visible, then requests each once', async () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();
  page.hide();

  page.frame({type: 'task_tree_changed', session_id: 'session-x'});
  page.frame({type: 'task_tree_changed', session_id: 'session-y'});
  await flush();
  assert.deepEqual(page.requests, [], 'a hidden tab makes no request');
  assert.equal(page.cronLoads, 0);

  page.show();
  assert.deepEqual(page.rowUrls(), ['/api/sessions/?compact=1&ids=session-x,session-y']);
  assert.equal(page.cronLoads, 1, 'the cron task list loads once for the two frames');
});

test('a row fetch that answers after the tab hid starts no further request until it is visible', async () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();

  page.frame({type: 'task_tree_changed', session_id: 'session-x'});
  page.frame({type: 'task_tree_changed', session_id: 'session-y'});
  page.hide();
  answerRows(page);
  await flush();
  assert.equal(page.urlsStarting('/api/sessions/').length, 1, 'the hidden tab starts no follow-up request');

  page.show();
  assert.deepEqual(page.rowUrls(), [
    '/api/sessions/?compact=1&ids=session-x',
    '/api/sessions/?compact=1&ids=session-y',
  ]);
});

test('a reconnect after a network loss reloads the list once; a page-driven reconnect does not', async () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();
  page.socket().emitClose({code: 1006});
  page.runTimers();
  page.socket().emitOpen();
  assert.equal(page.fullListCount(), 1, 'the reopened socket reloads the list');

  vm.runInContext('disconnectWS()', page.context);
  page.connect();
  page.socket().emitOpen();
  assert.equal(page.fullListCount(), 1, 'a switch-driven reconnect does not reload the list');
});

test('a stale-page close reloads a hidden tab once it is visible', () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();
  page.hide();

  page.socket().emitClose({code: 4409});
  assert.equal(page.reloads, 0, 'a hidden tab does not reload');

  page.show();
  assert.equal(page.reloads, 1, 'the reload runs once the tab is visible');
});

test('the status poll names the Workspace rows by filter and sends only the sessions the list does not paint', async () => {
  const page = buildPage({painted: ['session-b']});

  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status'), ['/api/sessions/status?filter=all&ids=session-a'],
      'the open chat is not painted, so it rides in ids; the Workspace rows come by filter');
  page.requests.forEach((request) => request.respond({}));
  await flush();

  page.elements.set('sidebar-search', {value: 'needle'});
  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status').slice(1), ['/api/sessions/status?ids=session-a,session-b'],
      'a search names its rows by ids, since its rows are not the Workspace list');
});
