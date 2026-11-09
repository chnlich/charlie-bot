// Rename-prefill mechanism tests: the rename input must prefill the session's
// current name, read live from the DOM at open time. No rename handler string
// may carry session state, the class of bug where a second rename prefills the
// stale render-time name. The groups.js vm harness is shared:
// web/tests/sidebar_groups_context_stub.js.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {loadGroups} = require('./sidebar_groups_context_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const MODALS_JS = readStatic('sidebar/modals.js');

// modals.js wires startRename as a bare global through Sidebar.wire; the fake
// DOM below covers exactly what startRename touches: getElementById,
// getBoundingClientRect, querySelector, classList, style, focus, select.
function loadModals(dom, fetchImpl) {
  const Sidebar = {};
  const context = {
    Sidebar,
    Chat: {registerSeparatorButton: () => {}},
    globalThis: null,
    document: dom.document,
    console: {error: () => {}},
    setTimeout: () => 0,
    fetch: fetchImpl || (() => Promise.resolve({ok: true})),
    JSON_HEADERS: {},
    // status.js and session-view.js define these on the page; this harness
    // loads modals.js alone, and the commit path calls both.
    updateSidebarSessionName: () => {},
    refreshDisplayedSessionName: () => {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  vm.runInContext(MODALS_JS, context, {filename: 'modals.js'});
  return context;
}

function fakeRect(width) {
  return {top: 100, left: 50, width, height: 28};
}

function makeDom({rowWidth = 220, rowName = 'demo session', headerName = 'demo session', withRow = true}) {
  const input = {
    value: '',
    style: {},
    classList: {add() {}, remove() {}, contains: () => false},
    focus() {},
    select() {},
  };
  const nameSpan = {textContent: rowName, getBoundingClientRect: () => ({top: 100, left: 50, width: rowWidth, height: 20})};
  const rowEl = withRow ? {
    dataset: {},
    removeAttribute(name) { delete this.dataset[name.replace(/^data-(.)/, (_, c) => c.toUpperCase())]; },
    getBoundingClientRect: () => fakeRect(rowWidth),
    querySelector: (sel) => (sel === '.session-name' ? nameSpan : null),
  } : null;
  const headerEl = {textContent: headerName, getBoundingClientRect: () => fakeRect(400)};
  const elements = {'rename-input': input, 'header-session-name': headerEl};
  if (rowEl) elements['session-s1'] = rowEl;
  return {
    input, nameSpan, rowEl, headerEl,
    document: {getElementById: (id) => (id in elements ? elements[id] : null)},
  };
}

function clickRename(dom, id = 's1') {
  const context = loadModals(dom);
  context.startRename({preventDefault() {}, stopPropagation() {}}, id);
  return context;
}

test('row handlers carry no session name: every startRename call holds only the id', () => {
  const hostile = "O'Brien & Co <b>\"x\"";
  for (const filter of ['all', 'scheduled']) {
    const html = loadGroups().Sidebar.renderSessionItem(
      {id: 's1', name: hostile, updated_at: '2026-07-29T17:12:00Z'}, filter);
    const calls = html.match(/sidebarControl\('startRename'[^)]*\)/g) || [];
    assert.ok(calls.length >= 1, `${filter} row is missing a startRename call`);
    for (const call of calls) {
      assert.match(call, /^sidebarControl\('startRename', event, 's1'\)$/,
        `${filter} row bakes session state into the handler: ${call}`);
    }
  }
});

test('prefill reads the live row text: a renamed session prefills its new name', () => {
  const dom = makeDom({rowName: 'demo session'});
  clickRename(dom);
  assert.equal(dom.input.value, 'demo session');
  // The field sits on the name line and takes its height, so the row's time-and-backend line stays visible.
  assert.equal(dom.input.style.top, '100px');
  assert.equal(dom.input.style.height, '20px');
  assert.equal(dom.input.style.paddingTop, '0px');
  assert.equal(dom.input.style.paddingBottom, '0px');
  // updateSidebarSessionName (status.js) and the session_renamed websocket
  // handler both funnel into this one write: the title text, plus
  // data-full-name when the title is a shortened one. A non-shortened row has
  // neither, so the text is the name.
  dom.nameSpan.textContent = 'renamed by first pass';
  clickRename(dom);
  assert.equal(dom.input.value, 'renamed by first pass',
    'second rename prefilled a stale name instead of the live row text');
});

test('a shortened Threads title prefills the full session name', () => {
  const dom = makeDom({rowName: '2026-11-10 12:17'});
  dom.rowEl.dataset.fullName = 'Slack #docs-review 2026-11-10 12:17';
  clickRename(dom);
  assert.equal(dom.input.value, 'Slack #docs-review 2026-11-10 12:17',
    'Enter on an untouched field would have committed the shortened title');
});

test('Enter on an untouched shortened field commits the full name', async () => {
  const fetches = [];
  const dom = makeDom({rowName: '2026-11-10 12:17'});
  dom.rowEl.dataset.fullName = 'Slack #docs-review 2026-11-10 12:17';
  const context = loadModals(dom, async (url, opts) => {
    fetches.push({url, opts});
    return {ok: true};
  });
  context.startRename({preventDefault() {}, stopPropagation() {}}, 's1');
  context.handleRenameKey({key: 'Enter', preventDefault() {}});
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(fetches.length, 1, 'one PATCH went out');
  assert.equal(fetches[0].url, '/api/sessions/s1');
  assert.deepEqual(JSON.parse(fetches[0].opts.body), {name: 'Slack #docs-review 2026-11-10 12:17'});
});

test('hostile names round-trip exactly through the prefill', () => {
  const hostile = "O'Brien & Co\nline2 \\ end";
  const dom = makeDom({rowName: hostile, headerName: 'stale header'});
  clickRename(dom);
  assert.equal(dom.input.value, hostile);
});

test('missing row (filter mismatch) falls back to the header for anchor and value', () => {
  const dom = makeDom({withRow: false, headerName: 'header fallback name'});
  clickRename(dom);
  assert.equal(dom.input.value, 'header fallback name');
  assert.equal(dom.input.style.top, '100px');
  assert.equal(dom.input.style.width, '400px');
});

test('zero-width row rect (hidden row) falls back to the header instead of corner-painting', () => {
  const dom = makeDom({rowWidth: 0, rowName: 'row text', headerName: 'header fallback name'});
  clickRename(dom);
  assert.equal(dom.input.value, 'header fallback name');
  assert.equal(dom.input.style.left, '50px');
});

test('a chat-header click opens the field on the header name line, not on the session row', () => {
  const dom = makeDom({rowName: 'row text', headerName: 'header name'});
  const context = loadModals(dom);
  // The header's onclick handler sits on the div that holds the heading.
  context.startRename({
    preventDefault() {}, stopPropagation() {},
    currentTarget: {contains: (el) => el === dom.headerEl},
  }, 's1');
  assert.equal(dom.input.value, 'header name');
  assert.equal(dom.input.style.width, '400px', 'the field took the row width instead of the header name line');
  assert.equal(dom.input.style.height, '28px');
});

// The page's document carries a loader in place of modals.js, because the module loads on the first sidebar
// control. The separator line's Clone and Elon-e buttons paint on the first transcript render, which a later
// sidebar click never repaints, so the loader registers them at load.
test('the page loader registers the separator buttons before the first transcript render', () => {
  const rendered = [];
  const context = {
    Sidebar: {},
    Chat: {
      CLONE_SVG: '<svg id="clone"></svg>',
      registerSeparatorButton(render) { rendered.push(render); },
    },
    loadPageScripts() { throw new Error('the loader must not fetch at load'); },
    document: {getElementById: () => null},
    localStorage: {getItem: () => null, setItem() {}},
    console: {error() {}},
    globalThis: null,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  const loader = fs.readFileSync(
    path.join(__dirname, '..', 'templates', 'js', 'sidebar', 'modals.js'), 'utf8');
  vm.runInContext(loader, context, {filename: 'templates/js/sidebar/modals.js'});

  assert.equal(rendered.length, 1, 'the loader registers no separator buttons');
  const html = rendered[0]('session-a', 3);
  assert.match(html, /onclick="forkSession\('session-a', 3\)"/);
  assert.match(html, /onclick="eloneSession\('session-a', 3\)"/);
});
