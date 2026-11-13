// ---------------------------------------------------------------------------
// A sidebar list refreshes the rows a task-tree frame names, not the whole
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
const {createElement} = require('./dom_element_stub');

const PAGE_TIMERS_JS = readStatic('page-timers.js');
const WEBSOCKET_JS = readStatic('websocket.js');
const WEBSOCKET_HANDLERS_JS = readStatic('websocket-handlers.js');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// One open page on the Workspace view. `painted` names the sessions the sidebar paints as rows.
function buildPage({painted = [], filter = 'all', partial = true} = {}) {
  const listeners = new Map();
  const nav = createElement();
  nav.scrollTop = 0;
  const elements = new Map([['session-list', nav]]);
  for (const sid of painted) elements.set('session-' + sid, {id: 'session-' + sid});
  const timers = [];
  const page = {elements, nav, requests: [], cronLoads: 0, reloads: 0, storage: new Map()};
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
    createElement,
    body: {appendChild() {}, style: {}},
  };
  const context = {
    SESSION_ID: 'session-a',
    SESSION_NAME: 'Session A',
    // The page carries only its first screen of sidebar rows unless the caller says it painted them all.
    INITIAL_SESSIONS: {$screen: {partial}},
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
  context.currentFilter = filter;

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
      .filter((request) => request.url.includes('ids='))
      .forEach((request) => request.respond({sessions: {}, gone: []}));
}

// One session row as the sidebar's listing carries it.
function row(id, overrides = {}) {
  return {
    id,
    name: 'Session ' + id,
    group: 'Work',
    updated_at: '2026-04-02T04:00:00Z',
    has_unread: false,
    has_running_tasks: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    starred: false,
    backend: 'claude-opus-4.6',
    ...overrides,
  };
}

// A worker whose task closed is archived by the server, and the page learns it from the task_tree_changed frame
// broadcast for it. Only a network close closes the page's socket now, so the reopen path is the network one; the
// switch's own window serves the sidebar frames through its frame drop.
test('a row archived while the socket was closed leaves the list when it reopens', async () => {
  const page = buildPage();
  const manager = row('b', {profile: 'manager', updated_at: '2026-04-02T10:00:00Z'});
  const worker = row('w', {profile: 'worker', task_parent_id: 'b', updated_at: '2026-04-02T09:00:00Z'});
  page.context.renderSessionList([manager, worker], 'all');
  assert.match(page.nav.innerHTML, /id="session-w"/);

  page.connect();
  page.socket().emitOpen();
  assert.equal(page.fullListCount(), 0, "a page load's first socket reloads nothing");

  // The network close, and the reopen its backoff lands.
  page.socket().emitClose({code: 1006});
  page.runTimers();
  page.socket().emitOpen();
  assert.equal(page.fullListCount(), 1, 'the reopen reloads the list the gap could have changed');

  page.requests
      .filter((request) => request.url === '/api/sessions/?compact=1')
      .forEach((request) => request.respond([manager]));
  await flush();
  assert.doesNotMatch(page.nav.innerHTML, /id="session-w"/, 'the archived row left the list');
});

test('a task_tree_changed frame inside a switch window still refreshes its row', async () => {
  const page = buildPage();
  const manager = row('b', {profile: 'manager', updated_at: '2026-04-02T10:00:00Z'});
  const worker = row('w', {profile: 'worker', task_parent_id: 'b', updated_at: '2026-04-02T09:00:00Z'});
  page.context.renderSessionList([manager, worker], 'all');
  page.connect();
  page.socket().emitOpen();

  // The switch's click holds the frames of the session it left; the sidebar frames name their own
  // session and keep flowing, so the archived worker's broadcast still reaches the row refresh.
  vm.runInContext('holdSessionFrames("c")', page.context);
  page.frame({type: 'message', message: {role: 'user', content: 'left session frame', event_index: 9}});
  page.frame({type: 'task_tree_changed', session_id: 'w'});
  assert.deepEqual(page.rowUrls(), ['/api/sessions/?compact=1&ids=w'],
      'the switch window dropped the session frame and served the sidebar frame');
  page.requests.filter((request) => request.url.includes('&ids='))
      .forEach((request) => request.respond({sessions: {}, gone: ['w']}));
  await flush();
  assert.doesNotMatch(page.nav.innerHTML, /id="session-w"/, 'the archived row left the list mid-switch');
});

test('a row id dropped from the queue on another view still leaves on the return to the Workspace', async () => {
  const page = buildPage();
  const manager = row('b', {profile: 'manager', updated_at: '2026-04-02T10:00:00Z'});
  const worker = row('w', {profile: 'worker', task_parent_id: 'b', updated_at: '2026-04-02T09:00:00Z'});
  page.context.renderSessionList([manager, worker], 'all');
  page.connect();
  page.socket().emitOpen();

  // One row fetch is in flight, and the worker's id queues behind it. The reader then searches, so the Workspace's
  // rows are no longer the list on screen when the in-flight fetch drains the queue.
  page.frame({type: 'task_tree_changed', session_id: 'b'});
  page.frame({type: 'task_tree_changed', session_id: 'w'});
  page.elements.set('sidebar-search', {value: 'needle'});
  page.requests.filter((request) => request.url.includes('&ids='))
      .forEach((request) => request.respond({sessions: {}, gone: []}));
  await flush();
  assert.equal(page.rowUrls().length, 1, 'the queued id is dropped, not fetched');

  // Clearing the search returns to the Workspace, whose own load paints the server's rows, the worker absent.
  page.elements.get('sidebar-search').value = '';
  page.context.handleSidebarSearch('');
  page.requests
      .filter((request) => request.url === '/api/sessions/?compact=1')
      .forEach((request) => request.respond([manager]));
  await flush();
  assert.doesNotMatch(page.nav.innerHTML, /id="session-w"/, 'the return to the Workspace leaves no stale row');
});

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

test('every reopen that follows a close reloads the list once', async () => {
  const page = buildPage();
  page.connect();
  page.socket().emitOpen();
  page.socket().emitClose({code: 1006});
  page.runTimers();
  page.socket().emitOpen();
  assert.equal(page.fullListCount(), 1, 'the reopened socket reloads the list');
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

test('a starred-view row change fetches that row alone from the starred view', async () => {
  const page = buildPage({filter: 'starred'});
  page.connect();
  page.socket().emitOpen();

  page.frame({type: 'task_tree_changed', session_id: 'session-x'});

  assert.deepEqual(page.urlsStarting('/api/sessions/starred'), ['/api/sessions/starred?ids=session-x'],
      'the starred view answers the named row alone');
  assert.equal(page.fullListCount(), 0, 'a task-tree frame never refetches the whole list');
  assert.equal(page.urlsStarting('/api/sessions/starred').length, 1);
});

test('a threads-view row change fetches that row alone from the threads view', async () => {
  const page = buildPage({filter: 'threads'});
  page.context.registerSidebarFilter(
      {name: 'threads', label: 'Threads', url: '/api/sessions/chat-threads', before: 'starred'});
  page.context.currentFilter = 'threads';
  page.connect();
  page.socket().emitOpen();

  page.frame({type: 'task_tree_changed', session_id: 'session-x'});

  assert.deepEqual(page.urlsStarting('/api/sessions/chat-threads'), ['/api/sessions/chat-threads?ids=session-x'],
      'the Threads view answers the named row alone');
  assert.equal(page.fullListCount(), 0, 'a task-tree frame never refetches the whole list');
});

test('a view change and a reconnect after a network loss still load the whole list', async () => {
  const page = buildPage({filter: 'starred'});
  page.connect();
  page.socket().emitOpen();
  page.socket().emitClose({code: 1006});
  page.runTimers();
  page.socket().emitOpen();

  assert.equal(page.urlsStarting('/api/sessions/starred').filter((url) => !url.includes('ids=')).length, 1,
      'the reopened socket reloads the starred list');
});

test('the status poll names the rows the page holds, so the answer carries those rows alone', async () => {
  const page = buildPage({painted: ['session-b']});

  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status'),
      ['/api/sessions/status?filter=all&ids=session-a,session-b&answer=ids'],
      'the open chat and the painted row ride in ids; answer=ids asks for those rows alone');
  page.requests.forEach((request) => request.respond({token: 'test-1', rows: {}, gone: []}));
  await flush();

  page.elements.set('sidebar-search', {value: 'needle'});
  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status').slice(1), ['/api/sessions/status?ids=session-a,session-b'],
      'a search names its rows by ids, since its rows are not the Workspace list');
});

test('a page that painted its whole view names the filter alone', async () => {
  const page = buildPage({painted: ['session-a', 'session-b'], partial: false});

  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status'), ['/api/sessions/status?filter=all'],
      'every row the page holds rides in the filter answer, so it names no ids');
});

test('a full listing after the first screen moves the poll back to the filter form', async () => {
  const page = buildPage({painted: ['session-b']});
  vm.runInContext('pollSessionStatus()', page.context);
  page.requests.forEach((request) => request.respond({token: 'test-1', rows: {}, gone: []}));
  await flush();

  // "Show all" paints the whole view, so the poll now holds a row the first screen did not carry.
  page.document.querySelectorAll = (selector) => selector === 'a[id^="session-"]'
    ? ['session-a', 'session-b', 'session-c'].map((sid) => ({id: 'session-' + sid})) : [];
  vm.runInContext('pollSessionStatus()', page.context);

  assert.deepEqual(page.urlsStarting('/api/sessions/status').slice(1), ['/api/sessions/status?filter=all'],
      'the filter answer carries every row the page paints once a full listing moved the row set');
});

test('a status poll falls back to the filter when the held rows outgrow one URL', async () => {
  const painted = Array.from({length: 400}, (_, index) => '00000000-0000-0000-0000-' + String(index).padStart(12, '0'));
  const page = buildPage({painted});

  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(page.urlsStarting('/api/sessions/status'), ['/api/sessions/status?filter=all&ids=session-a'],
      'the filter names the painted rows, and the open chat rides as an id, when naming every held row would split the poll');
});

test('the status poll sends a stable URL for the same row set and the token of the answer before it', async () => {
  const page = buildPage({painted: ['session-b', 'session-c']});
  const urls = () => page.urlsStarting('/api/sessions/status');
  const rows = {'session-a': {}, 'session-b': {has_running_tasks: true}, 'session-c': {}};

  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(urls(), ['/api/sessions/status?filter=all&ids=session-a,session-b,session-c&answer=ids']);
  page.requests.forEach((request) => request.respond({token: 'test-1', rows, gone: []}));
  await flush();

  // Two paints of the same rows in another order ask the same URL: the URL names a set, not a sequence.
  page.document.querySelectorAll = (selector) => selector === 'a[id^="session-"]'
    ? ['session-c', 'session-b'].map((sid) => ({id: 'session-' + sid})) : [];
  vm.runInContext('pollSessionStatus()', page.context);
  assert.deepEqual(urls().slice(1),
      ['/api/sessions/status?filter=all&since=test-1&ids=session-a,session-b,session-c&answer=ids'],
      'the same row set revalidates the answer the page holds, with the token it names');
});
