// ---------------------------------------------------------------------------
// The sidebar list operations' server-answer handling (session-actions.js). A
// failed archive (POST /api/sessions/{id}/archive) or unarchive
// (POST /api/sessions/{id}/unarchive) surfaces the server's detail to the user
// through the page toast. The detail takes three shapes: the 409 blockers
// shape (message + blocker list), a plain string detail, and the status line
// when the refusal carries no body. An archive sends its POST only from the
// confirmation dialog's Archive answer; a successful archive removes the row.
// A successful unarchive drops the ids the restore names (a task node's
// response lists the whole restored chain) or just the clicked row, and stays
// quiet. A legacy session's response is the session metadata. The permanent
// delete shares the confirmation dialog: it deletes only from its own confirm.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {escapeHtml} = require('./escape_html_stub');

// One element of the harness document. Writing innerHTML registers each <button id> the markup names, so the
// confirmation's buttons are elements a test can answer; fire() runs an element's click or key listeners.
function makeElement(id, {name = ''} = {}) {
  const listeners = new Map();
  const buttons = new Map();
  const nameEl = {textContent: name};
  let html = '';
  const element = {
    id,
    removed: false,
    addEventListener(type, handler) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(handler);
    },
    fire(type, event = {}) {
      const target = event.target || element;
      (listeners.get(type) || []).forEach((handler) => handler({stopPropagation() {}, ...event, target}));
    },
    querySelector(selector) {
      return selector === '.session-name' ? nameEl : buttons.get(selector) || null;
    },
    remove() {
      element.removed = true;
    },
  };
  Object.defineProperty(element, 'innerHTML', {
    get() {
      return html;
    },
    set(value) {
      html = value;
      for (const [, buttonId] of value.matchAll(/<button id="([^"]+)"/g)) {
        buttons.set('#' + buttonId, makeElement(buttonId));
      }
    },
  });
  return element;
}

// The harness document: getElementById answers the session rows the test paints and the open confirmation; keydown
// listeners are recorded so a test presses keys on the document.
function makeDocument(rows) {
  const mounted = new Map();
  const keyListeners = [];
  return {
    body: {
      appendChild(element) {
        mounted.set(element.id, element);
        return element;
      },
    },
    createElement: (tagName) => makeElement(tagName),
    getElementById(id) {
      const open = mounted.get(id);
      if (open && !open.removed) return open;
      return rows.get(id) || null;
    },
    querySelectorAll: () => [],
    addEventListener(type, handler) {
      if (type === 'keydown') keyListeners.push(handler);
    },
    removeEventListener(type, handler) {
      if (type !== 'keydown') return;
      const index = keyListeners.indexOf(handler);
      if (index !== -1) keyListeners.splice(index, 1);
    },
    pressKey(key) {
      [...keyListeners].forEach((handler) => handler({key, repeat: false, preventDefault() {}}));
    },
  };
}

function buildContext({fetchImpl, showToastImpl, archivedForgetSessionImpl = () => {}, rowName = 'Leaf one'}) {
  const fetchCalls = [];
  const rows = new Map([
    ['session-leaf-1', makeElement('session-leaf-1', {name: rowName})],
  ]);
  const context = {
    console,
    fetch: async (url, opts) => {
      fetchCalls.push({url, opts});
      return fetchImpl(url, opts);
    },
    fetchCalls,
    JSON_HEADERS: {'Content-Type': 'application/json'},
    crypto: {randomUUID: () => 'archive-request-id'},
    escapeHtml,
    showToast: showToastImpl,
    setTimeout: () => 0,
    clearTimeout: () => {},
    // Call-time globals the archive path touches on the id being viewed.
    SESSION_ID: 'other-session',
    updateSidebarHighlight: () => {},
    switchSession: async () => {},
    renderNoActiveSessionView: () => {},
    archivedForgetSession: archivedForgetSessionImpl,
    // status.js's removal seam: a removed row's unread fact leaves with it.
    forgetUnreadFact: () => {},
    switchSidebarFilter: () => {},
    document: makeDocument(rows),
  };
  context.globalThis = context;
  vm.createContext(context);
  // namespace.js first, as on the page: it supplies Sidebar.wire and the
  // currentFilter state the actions module reads.
  vm.runInContext(readStatic('sidebar/namespace.js'), context, {filename: 'namespace.js'});
  // Of the modules the page loads between namespace and session-actions, the
  // archive path touches one helper: Sidebar.removeSessionFromRenderedList.
  context.Sidebar.removeSessionFromRenderedList = () => true;
  vm.runInContext(readStatic('sidebar/session-actions.js'), context, {filename: 'session-actions.js'});
  return context;
}

// The confirmation's button or backdrop as the user reaches it: the open dialog's named button, clicked.
function answerDialog(context, buttonId) {
  const overlay = context.document.getElementById('confirm-overlay');
  assert.ok(overlay, 'the confirmation dialog is open');
  overlay.querySelector('#' + buttonId).fire('click');
}

// The confirm handlers start their request without awaiting it; one macrotask lets it settle.
const settle = () => new Promise((resolve) => setImmediate(resolve));

const archivePosts = (context) => context.fetchCalls
  .filter((call) => call.opts && call.opts.method === 'POST' && call.url.endsWith('/archive'))
  .map((call) => call.url);

const okJson = (body = {}) => async () => ({ok: true, status: 200, json: async () => body});

test('a 409 archive refusal shows the server message and its blockers', async () => {
  const toasts = [];
  const context = buildContext({
    fetchImpl: async () => ({
      ok: false,
      status: 409,
      json: async () => ({detail: {message: 'archived close refused', blockers: ['node-a: run r1 is active']}}),
    }),
    showToastImpl: (msg, isError) => toasts.push([msg, isError]),
  });

  context.archiveSession('leaf-1');
  answerDialog(context, 'confirm-ok-btn');
  await settle();

  assert.deepEqual(toasts, [['archived close refused\nnode-a: run r1 is active', true]]);
});

test('a plain-string refusal detail shows as-is; a bodyless refusal keeps the status line', async () => {
  const toasts = [];
  let call = 0;
  const context = buildContext({
    fetchImpl: async () => {
      call += 1;
      if (call === 1) {
        return {ok: false, status: 403, json: async () => ({detail: 'archiving a task requires operator credentials'})};
      }
      return {ok: false, status: 500, json: async () => {
        throw new Error('not json');
      }};
    },
    showToastImpl: (msg, isError) => toasts.push([msg, isError]),
  });

  context.archiveSession('leaf-1');
  answerDialog(context, 'confirm-ok-btn');
  await settle();
  context.archiveSession('leaf-1');
  answerDialog(context, 'confirm-ok-btn');
  await settle();

  assert.deepEqual(toasts, [['archiving a task requires operator credentials', true], ['Archive failed: 500', true]]);
});

test('a successful archive removes the row and shows no toast', async () => {
  const toasts = [];
  let removed = 0;
  const context = buildContext({
    fetchImpl: okJson({archived: ['leaf-1']}),
    showToastImpl: (msg) => toasts.push(msg),
  });
  context.Sidebar.removeSessionFromRenderedList = () => {
    removed += 1;
    return true;
  };

  context.archiveSession('leaf-1');
  answerDialog(context, 'confirm-ok-btn');
  await settle();

  assert.equal(removed, 1);
  assert.deepEqual(toasts, []);
});

// ---------------------------------------------------------------------------
// The archive confirmation: every archive entry opens it, names the session by its row's title, and sends the one
// POST only from its Archive answer (Archive click or Enter). Cancel, Escape and a backdrop click send nothing.
// ---------------------------------------------------------------------------

test('the row control opens the archive confirmation, which names the session and sends nothing yet', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  // The row's Archive button calls the registered control (groups.js).
  context.Sidebar.sidebarControl('archiveSession', 'leaf-1');

  const overlay = context.document.getElementById('confirm-overlay');
  assert.ok(overlay);
  assert.match(overlay.innerHTML, /Leaf one/);
  assert.deepEqual(context.fetchCalls, []);

  answerDialog(context, 'confirm-ok-btn');
  await settle();

  assert.deepEqual(archivePosts(context), ['/api/sessions/leaf-1/archive']);
  assert.equal(context.document.getElementById('confirm-overlay'), null);
});

test('the touch row-menu Archive item opens the same confirmation', () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});
  let items;
  context.Sidebar.sessionMenuItemSources = [];
  context.registerSessionMenuItems = () => {};
  context.openRowMenu = (_anchor, menuItems) => { items = menuItems; };
  context.window = {matchMedia: () => ({matches: true})};
  vm.runInContext(readStatic('sidebar/session-menus.js'), context, {filename: 'session-menus.js'});

  context.Sidebar.openSessionRowMenu({dataset: {}}, 'leaf-1');
  const archive = items.find((item) => item.label === 'Archive');
  assert.ok(archive, 'the touch row menu has an Archive item');
  archive.onSelect();

  const overlay = context.document.getElementById('confirm-overlay');
  assert.ok(overlay);
  assert.match(overlay.innerHTML, /Leaf one/);
  assert.deepEqual(context.fetchCalls, []);
});

test('the dialog escapes a session title that carries markup', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}, rowName: 'Leaf <img src=x> & co'});

  context.archiveSession('leaf-1');

  const overlay = context.document.getElementById('confirm-overlay');
  assert.doesNotMatch(overlay.innerHTML, /<img/);
  assert.match(overlay.innerHTML, /Leaf &lt;img src=x&gt; &amp; co/);
});

test('Cancel closes the archive confirmation and sends no request', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  context.archiveSession('leaf-1');
  answerDialog(context, 'confirm-cancel-btn');
  await settle();

  assert.equal(context.document.getElementById('confirm-overlay'), null);
  assert.deepEqual(context.fetchCalls, []);
});

test('Escape closes the archive confirmation and sends no request', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  context.archiveSession('leaf-1');
  context.document.pressKey('Escape');
  await settle();

  assert.equal(context.document.getElementById('confirm-overlay'), null);
  assert.deepEqual(context.fetchCalls, []);
});

test('a click outside the archive confirmation closes it and sends no request', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  context.archiveSession('leaf-1');
  const overlay = context.document.getElementById('confirm-overlay');
  overlay.fire('click', {target: overlay});
  await settle();

  assert.equal(context.document.getElementById('confirm-overlay'), null);
  assert.deepEqual(context.fetchCalls, []);
});

test('Enter sends the archive once; the confirmation is gone for a second Enter', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  context.archiveSession('leaf-1');
  context.document.pressKey('Enter');
  context.document.pressKey('Enter');
  await settle();

  assert.deepEqual(archivePosts(context), ['/api/sessions/leaf-1/archive']);
});

// ---------------------------------------------------------------------------
// The permanent delete shares the confirmation dialog. Its Delete answer is its only request, and Enter does not
// answer it: the Enter key is the archive confirmation's alone.
// ---------------------------------------------------------------------------

test('the delete confirmation deletes only from its Delete answer', async () => {
  const context = buildContext({fetchImpl: okJson(), showToastImpl: () => {}});

  context.confirmDeletePermanently('leaf-1');
  context.document.pressKey('Enter');
  answerDialog(context, 'confirm-cancel-btn');
  await settle();
  assert.deepEqual(context.fetchCalls, []);

  context.confirmDeletePermanently('leaf-1');
  answerDialog(context, 'confirm-ok-btn');
  await settle();

  const deletes = context.fetchCalls.filter((call) => call.opts && call.opts.method === 'DELETE');
  assert.deepEqual(deletes.map((call) => call.url), ['/api/sessions/leaf-1/permanent']);
  assert.equal(context.document.getElementById('confirm-overlay'), null);
});

// ---------------------------------------------------------------------------
// The unarchive entry: the restored-ids answer drops the whole restored chain
// from the archived list, and the legacy metadata answer drops only the clicked
// row. A refusal rides the same toast the archive refusal rides.
// ---------------------------------------------------------------------------

test('a task-node unarchive drops every restored id from the archived list', async () => {
  const forgotten = [];
  const toasts = [];
  let repainted = 0;
  const context = buildContext({
    fetchImpl: async () => ({
      ok: true,
      status: 200,
      json: async () => ({restored: ['root-1', 'mid-1', 'leaf-1']}),
    }),
    showToastImpl: (msg, isError) => toasts.push([msg, isError]),
    archivedForgetSessionImpl: (id) => forgotten.push(id),
  });
  context.currentFilter = 'archived';
  context.Sidebar.removeSessionFromRenderedList = () => {
    repainted += 1;
    return true;
  };

  await context.unarchiveSession('leaf-1');

  assert.deepEqual(forgotten, ['root-1', 'mid-1', 'leaf-1']);
  assert.equal(repainted, 1);
  assert.deepEqual(toasts, []);
});

test('a legacy unarchive (session metadata body) drops only the clicked row', async () => {
  const toasts = [];
  const forgotten = [];
  const context = buildContext({
    fetchImpl: async () => ({
      ok: true,
      status: 200,
      json: async () => ({id: 'legacy-1', status: 'active', name: 'Legacy'}),
    }),
    showToastImpl: (msg, isError) => toasts.push([msg, isError]),
    archivedForgetSessionImpl: (id) => forgotten.push(id),
  });
  context.currentFilter = 'archived';
  context.Sidebar.removeSessionFromRenderedList = () => true;

  await context.unarchiveSession('legacy-1');

  assert.deepEqual(forgotten, ['legacy-1']);
  assert.deepEqual(toasts, []);
});

test('a failed unarchive shows the server detail through the toast', async () => {
  const toasts = [];
  const forgotten = [];
  const context = buildContext({
    fetchImpl: async () => ({
      ok: false,
      status: 403,
      json: async () => ({detail: 'unarchiving a task requires operator credentials'}),
    }),
    showToastImpl: (msg, isError) => toasts.push([msg, isError]),
    archivedForgetSessionImpl: (id) => forgotten.push(id),
  });
  context.currentFilter = 'archived';

  await context.unarchiveSession('leaf-1');

  assert.deepEqual(toasts, [['unarchiving a task requires operator credentials', true]]);
  assert.deepEqual(forgotten, []);
});
