// ---------------------------------------------------------------------------
// Archived view mechanism assertions (plan: Sidebar 10k scale, acceptance (b)):
// the initial DOM carries exactly one page of rows, an append adds at most one
// page and rewrites no existing row, the status poll id set excludes archived
// rows, and in-list operations touch only the target row and the strip counts.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { createElement } = require('./dom_element_stub');

const COMPAT_LOADER_JS = readStatic('compat-loader.js');
const CHAT_JS = readStatic('chat.js');
const SIDEBAR_JS = readStatic('sidebar.js');
const PAGE_TIMERS_JS = readStatic('page-timers.js');

function makeArchivedSession(id, overrides = {}) {
  return {
    id,
    name: `Session ${id}`,
    group: null,
    status: 'archived',
    updated_at: '2026-04-02T04:00:00Z',
    has_unread: false,
    has_running_tasks: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    starred: false,
    rating: null,
    backend: 'claude-opus-4.6',
    ...overrides,
  };
}

function makePage(sessions, {hasMore = false, groups = null} = {}) {
  const counts = new Map();
  (groups ? [] : sessions).forEach((s) => {
    const key = s.group || null;
    counts.set(key, (counts.get(key) || 0) + 1);
  });
  const aggregated = groups || Array.from(counts, ([group, total]) => ({group, total}));
  const last = sessions[sessions.length - 1] || null;
  return {
    sessions,
    has_more: hasMore,
    next_before: hasMore && last ? last.updated_at : null,
    next_before_id: hasMore && last ? last.id : null,
    groups: aggregated,
  };
}

function buildContext(overrides = {}) {
  const elements = overrides.elements || new Map();
  const fetchCalls = [];
  const context = {
    SESSION_ID: 'session-live',
    INITIAL_SESSIONS: [],
    INITIAL_LOAD_ERRORS: [],
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: ''},
    history: {pushState: () => {}},
    console: {error: () => {}, log: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}, removeItem: () => {}},
    setInterval: () => 0,
    setTimeout: (fn) => { fn(); return 0; },
    clearInterval: () => {},
    clearTimeout: () => {},
    fetch: overrides.fetch || (async (url) => { throw new Error('unexpected fetch ' + url); }),
    document: {
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: overrides.querySelectorAll || (() => []),
      querySelector: () => null,
      createElement: (tagName) => {
        const el = createElement({tagName: String(tagName).toUpperCase()});
        // Support the escapeHtml pattern: set textContent, read innerHTML escaped.
        let rawText = '';
        Object.defineProperty(el, 'textContent', {
          get() { return rawText; },
          set(v) {
            rawText = String(v);
            el.innerHTML = String(v)
              .replaceAll('&', '&amp;')
              .replaceAll('<', '&lt;')
              .replaceAll('>', '&gt;')
              .replaceAll('"', '&quot;')
              .replaceAll("'", '&#39;');
          },
        });
        return el;
      },
      body: createElement({tagName: 'BODY'}),
      addEventListener: () => {},
      removeEventListener: () => {},
    },
    window: {addEventListener: () => {}, innerHeight: 800},
    CSS: {escape: (value) => String(value)},
    relativeTime: (txt) => txt,
    updateRelativeTimes: () => {},
    marked: {parse: (txt) => txt},
    fixNestedFences: (txt) => txt,
    BACKEND_OPTIONS: {},
    BACKEND_TYPES: {},
    switchSession: async () => {},
    renderNoActiveSessionView: () => {},
    updateSidebarHighlight: () => {},
    showToast: () => {},
    confirm: () => true,
    alert: () => {},
  };
  Object.defineProperty(context, 'fetchCalls', {value: fetchCalls});
  const innerFetch = context.fetch;
  context.fetch = async (url, opts = {}) => {
    fetchCalls.push(url);
    return innerFetch(url, opts);
  };
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(COMPAT_LOADER_JS, context, {filename: 'compat-loader.js'});
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, elements, fetchCalls};
}

function countRows(html) {
  return (html.match(/<a\b[^>]*id="session-/g) || []).length;
}

function archivedContext(pages) {
  const nav = createElement();
  let call = 0;
  const {context, fetchCalls} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['filter-all', createElement({className: 'filter-pill'})],
      ['filter-starred', createElement({className: 'filter-pill'})],
      ['filter-archived', createElement({className: 'filter-pill'})],
      ['filter-scheduled', createElement({className: 'filter-pill'})],
      ['cron-add-btn', createElement()],
    ]),
    fetch: async (url) => {
      assert.match(url, /^\/api\/sessions\/archived\?limit=100/);
      const page = pages[Math.min(call, pages.length - 1)];
      call += 1;
      return {ok: true, async json() { return page; }};
    },
  });
  return {context, nav, fetchCalls};
}

test('entering the archived tab renders exactly one page of rows plus the group strip', async () => {
  const sessions = Array.from({length: 100}, (_, i) =>
    makeArchivedSession(`arch-${String(i).padStart(3, '0')}`, {group: i % 2 ? 'Work' : null}));
  const {context, nav, fetchCalls} = archivedContext([makePage(sessions, {hasMore: true})]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);

  assert.equal(context.currentFilter, 'archived');
  assert.equal(fetchCalls.length, 1);
  const [pills, rows] = nav.children;
  assert.match(pills.innerHTML, /All <span[^>]*>100<\/span>/);
  assert.match(pills.innerHTML, /Work <span[^>]*>50<\/span>/);
  assert.match(pills.innerHTML, /\(No group\) <span[^>]*>50<\/span>/);
  assert.equal(rows.children.length, 1);
  assert.equal(countRows(rows.children[0].innerHTML), 100);
  // Archived rows format their time at build: no data-time, so the periodic
  // relative-time sweep never walks this list.
  assert.doesNotMatch(rows.children[0].innerHTML, /data-time=/);
  // No live-state indicators on archived rows.
  assert.doesNotMatch(rows.children[0].innerHTML, /id="spinner-/);
  assert.doesNotMatch(rows.children[0].innerHTML, /id="unread-/);
});

test('an append adds at most one page and rewrites no existing row', async () => {
  const page1 = Array.from({length: 100}, (_, i) => makeArchivedSession(`p1-${String(i).padStart(3, '0')}`));
  const page2 = Array.from({length: 40}, (_, i) => makeArchivedSession(`p2-${String(i).padStart(3, '0')}`));
  const {context, nav} = archivedContext([
    makePage(page1, {hasMore: true, groups: [{group: null, total: 140}]}),
    makePage(page2, {hasMore: false, groups: [{group: null, total: 140}]}),
  ]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const rows = nav.children[1];
  const firstPageEl = rows.children[0];
  const firstPageHtml = firstPageEl.innerHTML;

  context.loadArchivedNextPage();
  await new Promise(setImmediate);

  assert.equal(rows.children.length, 2);
  assert.equal(rows.children[0], firstPageEl);
  assert.equal(rows.children[0].innerHTML, firstPageHtml); // zero rewrites of existing rows
  assert.equal(countRows(rows.children[1].innerHTML), 40);
});

test('the status poll id set excludes archived rows', async () => {
  const sessions = Array.from({length: 5}, (_, i) => makeArchivedSession(`arch-${i}`));
  const {context, nav} = archivedContext([makePage(sessions)]);
  context.document.querySelectorAll = (selector) =>
    selector === 'a[id^="session-"]'
      ? sessions.map((s) => ({id: 'session-' + s.id, tagName: 'A'}))
      : [];

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);

  assert.deepEqual(Array.from(context.sidebarSessionIds()), ['session-live']);
  assert.ok(nav.children[1].children[0].innerHTML.includes('arch-0'));
});

test('set group updates only the target row and the strip counts; the row leaves only a mismatched filter', async () => {
  const sessions = [
    makeArchivedSession('arch-a', {group: 'Work'}),
    makeArchivedSession('arch-b', {group: null}),
  ];
  const rowA = createElement({tagName: 'A', id: 'session-arch-a'});
  const personalPage = makePage([makeArchivedSession('arch-a', {group: 'Personal'})], {
    groups: [{group: 'Personal', total: 1}, {group: null, total: 1}],
  });
  const {context, nav} = archivedContext([makePage(sessions, {hasMore: false}), personalPage]);
  context.document.getElementById = ((orig) => (id) =>
    id === 'session-arch-a' ? rowA : orig(id))(context.document.getElementById);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const rows = nav.children[1];
  const pageHtml = rows.children[0].innerHTML;
  const pills = nav.children[0];

  // Under the "All" strip filter the row stays in place.
  context.applyArchivedGroupChange('arch-a', 'Personal');
  assert.equal(rowA.removed, undefined);
  assert.equal(rows.children[0].innerHTML, pageHtml); // no rewrite of rendered rows
  assert.match(pills.innerHTML, /Personal <span[^>]*>1<\/span>/);
  assert.doesNotMatch(pills.innerHTML, /Work <span[^>]*>1<\/span>/);

  // Under a named strip filter, a row moved elsewhere leaves the list.
  context.setArchivedGroupFilter('Personal');
  await new Promise(setImmediate);
  context.applyArchivedGroupChange('arch-a', 'Work');
  assert.equal(rowA.removed, true);
});

test('unarchive/delete bookkeeping decrements the strip counts', async () => {
  const sessions = [
    makeArchivedSession('arch-a', {group: 'Work'}),
    makeArchivedSession('arch-b', {group: 'Work'}),
    makeArchivedSession('arch-c', {group: null}),
  ];
  const {context, nav} = archivedContext([makePage(sessions)]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const pills = nav.children[0];
  assert.match(pills.innerHTML, /All <span[^>]*>3<\/span>/);
  assert.match(pills.innerHTML, /Work <span[^>]*>2<\/span>/);

  context.archivedForgetSession('arch-a');
  assert.match(pills.innerHTML, /All <span[^>]*>2<\/span>/);
  assert.match(pills.innerHTML, /Work <span[^>]*>1<\/span>/);

  context.archivedForgetSession('arch-c');
  assert.match(pills.innerHTML, /All <span[^>]*>1<\/span>/);
  assert.doesNotMatch(pills.innerHTML, /\(No group\)/);
});

test('auto-append stops at the render cap and continues through the Load more button', async () => {
  const pageOf = (n) => Array.from({length: 100}, (_, i) => makeArchivedSession(`c${n}-${String(i).padStart(3, '0')}`));
  const pages = Array.from({length: 30}, (_, n) =>
    makePage(pageOf(n), {hasMore: true, groups: [{group: null, total: 5000}]}));
  const {context, nav} = archivedContext(pages);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  for (let i = 0; i < 19; i++) {
    context.loadArchivedNextPage();
    await new Promise(setImmediate);
  }

  const rows = nav.children[1];
  const foot = nav.children[2];
  assert.equal(rows.children.length, 20); // 2000 rows rendered
  assert.match(foot.innerHTML, /Load more/);

  // The scroll path is capped out; the explicit button still appends.
  context.loadArchivedNextPage();
  await new Promise(setImmediate);
  assert.equal(rows.children.length, 21);
  assert.match(foot.innerHTML, /Load more/);
});
