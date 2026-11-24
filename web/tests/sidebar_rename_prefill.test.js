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

const {readStatic, runIconModule} = require('./read_static');
const {loadGroups} = require('./sidebar_groups_context_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const MODALS_JS = readStatic('sidebar/modals.js');

// modals.js wires startRename as a bare global through Sidebar.wire; the fake
// DOM below covers exactly what startRename touches: getElementById,
// getBoundingClientRect, querySelector, querySelectorAll, classList, style
// (the field's and the name's), focus, select, and the field's computed left
// padding (px-3, 12px).
function loadModals(dom, fetchImpl) {
  const Sidebar = {};
  const context = {
    Sidebar,
    Chat: {registerSeparatorButton: () => {}},
    globalThis: null,
    document: dom.document,
    getComputedStyle: () => ({paddingLeft: '12px'}),
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
  return {top: 100, left: 50, width, height: 28, right: 50 + width};
}

// The name span holds the row's title as its text and the full name as its title, the way renderSessionRowShell
// paints it. fullName defaults to the title: a row whose title is not shortened. buttons lists the row's direct
// buttons as {left, width}; a width of 0 is a button with no box.
function makeDom({rowWidth = 220, rowName = 'demo session', fullName = rowName, headerName = 'demo session',
  headerHeight = 28, withRow = true, buttons = []}) {
  const rowClasses = new Set();
  const input = {
    value: '',
    style: {},
    classList: {add() {}, remove() {}, contains: () => false},
    focus() {},
    select() {},
  };
  const nameSpan = {
    textContent: rowName,
    title: fullName,
    style: {},
    getBoundingClientRect: () => ({top: 100, left: 50, width: rowWidth, height: 20}),
  };
  const rowEl = withRow ? {
    getBoundingClientRect: () => fakeRect(rowWidth),
    querySelector: (sel) => (sel === '.session-name' ? nameSpan : null),
    querySelectorAll: () => buttons.map((b) => ({
      getBoundingClientRect: () => ({top: 100, left: b.left, width: b.width, height: 24, right: b.left + b.width}),
    })),
    classList: {
      add(name) { rowClasses.add(name); },
      remove(name) { rowClasses.delete(name); },
      contains(name) { return rowClasses.has(name); },
    },
  } : null;
  const headerEl = {textContent: headerName, getBoundingClientRect: () => ({...fakeRect(400), height: headerHeight})};
  const elements = {'rename-input': input, 'header-session-name': headerEl};
  if (rowEl) elements['session-s1'] = rowEl;
  return {
    input, nameSpan, rowEl, rowClasses, headerEl,
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
  // select() leaves the caret at the end of a long name; the field opens on the name's start instead.
  assert.equal(dom.input.scrollLeft, 0, 'the field opened scrolled to the end of a long name');
  // updateSidebarSessionName (status.js) and the session_renamed websocket
  // handler both funnel into this one write: the title text and the full name
  // as the title. A non-shortened row's title is its name.
  dom.nameSpan.textContent = 'renamed by first pass';
  dom.nameSpan.title = 'renamed by first pass';
  clickRename(dom);
  assert.equal(dom.input.value, 'renamed by first pass',
    'second rename prefilled a stale name instead of the live row text');
});

test('a shortened Threads title prefills the full session name', () => {
  const dom = makeDom({rowName: '2026-11-10 12:17', fullName: 'Slack #docs-review 2026-11-10 12:17'});
  clickRename(dom);
  assert.equal(dom.input.value, 'Slack #docs-review 2026-11-10 12:17',
    'Enter on an untouched field would have committed the shortened title');
});

test('Enter on an untouched shortened field commits the full name', async () => {
  const fetches = [];
  const dom = makeDom({rowName: '2026-11-10 12:17', fullName: 'Slack #docs-review 2026-11-10 12:17'});
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
  assert.equal(dom.nameSpan.style.visibility, '', 'the commit left the row name hidden');
  assert.ok(!dom.rowClasses.has('renaming'), 'the commit left the hover actions on the name line');
});

test('the field ends 4 px before the leftmost row button that has a box', () => {
  // The name's box starts at 50 and its field padding is 12 px, so the field starts at 38. The first button with a
  // box starts at 120; the zero-width button before it does not count.
  const dom = makeDom({rowWidth: 220, buttons: [{left: 0, width: 0}, {left: 150, width: 24}, {left: 120, width: 24}]});
  clickRename(dom);
  assert.equal(dom.input.style.left, '38px');
  assert.equal(dom.input.style.width, '78px');
});

test('a row with no button that has a box runs the field to its right edge less 4 px', () => {
  const dom = makeDom({rowWidth: 220, buttons: [{left: 0, width: 0}]});
  clickRename(dom);
  assert.equal(dom.input.style.width, '228px');
});

test('the row name is hidden and the row is renaming while the field is open', () => {
  const dom = makeDom({rowName: 'demo session'});
  const context = clickRename(dom);
  assert.equal(dom.nameSpan.style.visibility, 'hidden', 'the name text shows beside the field');
  assert.ok(dom.rowClasses.has('renaming'), 'the hover actions keep their full-row cover over the second line');
  context.handleRenameKey({key: 'Escape', preventDefault() {}});
  assert.equal(dom.nameSpan.style.visibility, '');
  assert.ok(!dom.rowClasses.has('renaming'), 'Escape left the hover actions on the name line');
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
  // The header name is one line tall; the field covers that line, not the row's.
  const dom = makeDom({rowName: 'row text', headerName: 'header name', headerHeight: 20});
  const context = loadModals(dom);
  // The header's onclick handler sits on the div that holds the heading.
  context.startRename({
    preventDefault() {}, stopPropagation() {},
    currentTarget: {contains: (el) => el === dom.headerEl},
  }, 's1');
  assert.equal(dom.input.value, 'header name');
  assert.equal(dom.input.style.width, '400px', 'the field took the row width instead of the header name line');
  assert.equal(dom.input.style.height, '20px', 'the field covers the header name line');
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
  runIconModule(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  const loader = fs.readFileSync(
    path.join(__dirname, '..', 'templates', 'js', 'sidebar', 'modals.js'), 'utf8');
  vm.runInContext(loader, context, {filename: 'templates/js/sidebar/modals.js'});

  assert.equal(rendered.length, 1, 'the loader registers no separator buttons');
  const html = rendered[0]('session-a', 3);
  assert.match(html, /onclick="forkSession\('session-a', 3\)"/);
  assert.match(html, /onclick="eloneSession\('session-a', 3\)"/);
});
