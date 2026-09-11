// Rename-prefill mechanism tests: the rename input must prefill the session's
// current name read live from the DOM at open time, and no rename handler
// string may carry session state (the class of bug where a second rename
// prefills the stale render-time name). The groups.js vm harness is shared:
// tests/sidebar_groups_context_stub.js.
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {loadGroups} = require('./sidebar_groups_context_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const MODALS_JS = readStatic('sidebar/modals.js');

// modals.js wires startRename as a bare global through Sidebar.wire; the fake
// DOM below covers exactly what startRename touches: getElementById,
// getBoundingClientRect, querySelector, classList, style, focus, select.
function loadModals(dom) {
  const Sidebar = {};
  const context = {
    Sidebar,
    globalThis: null,
    document: dom.document,
    console: {error: () => {}},
    setTimeout: () => 0,
    fetch: () => Promise.resolve({ok: true}),
    JSON_HEADERS: {},
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
  const nameSpan = {textContent: rowName};
  const rowEl = withRow ? {
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
    const calls = html.match(/startRename\([^)]*\)/g) || [];
    assert.ok(calls.length >= 1, `${filter} row is missing a startRename call`);
    for (const call of calls) {
      assert.match(call, /^startRename\(event, 's1'\)$/,
        `${filter} row bakes session state into the handler: ${call}`);
    }
  }
});

test('header handler in the template reads the live global, no server-side interpolation', () => {
  const fs = require('node:fs');
  const path = require('node:path');
  const html = fs.readFileSync(path.join(__dirname, '..', 'web', 'templates', 'index.html'), 'utf8');
  const attr = html.match(/onclick="(startRename\([^"]*)"/);
  assert.ok(attr, 'index.html header is missing a startRename onclick handler');
  assert.equal(attr[1], 'startRename(event, SESSION_ID)',
    'header handler must be the static global-reading form with no interpolated session state');
});

test('prefill reads the live row text: a renamed session prefills its new name', () => {
  const dom = makeDom({rowName: 'demo session'});
  clickRename(dom);
  assert.equal(dom.input.value, 'demo session');
  // updateSidebarSessionName (status.js) and the session_renamed websocket
  // handler both funnel into this one textContent write.
  dom.nameSpan.textContent = 'renamed by first pass';
  clickRename(dom);
  assert.equal(dom.input.value, 'renamed by first pass',
    'second rename prefilled a stale name instead of the live row text');
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
