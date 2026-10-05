// ---------------------------------------------------------------------------
// The sidebar list operations' server-answer handling (sidebar/filters.js):
// a failed archive (DELETE /api/sessions/{id}) or unarchive
// (POST /api/sessions/{id}/unarchive) surfaces the server's detail to the
// user through the page toast — the 409 blockers shape (message + blocker
// list), a plain string detail, and the status line when the refusal carries
// no body. A successful archive removes the row; a successful unarchive
// drops every id the restore names (a task node's response lists the whole
// restored chain) or just the clicked row (a legacy session's response is
// the session metadata), and stays quiet.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

function buildContext({fetchImpl, showToastImpl, archivedForgetSessionImpl = () => {}}) {
  const context = {
    console,
    fetch: fetchImpl,
    showToast: showToastImpl,
    setTimeout: () => 0,
    clearTimeout: () => {},
    // Call-time globals the archive path touches on the id being viewed.
    SESSION_ID: 'other-session',
    updateSidebarHighlight: () => {},
    switchSession: async () => {},
    renderNoActiveSessionView: () => {},
    archivedForgetSession: archivedForgetSessionImpl,
    switchSidebarFilter: () => {},
    document: {
      getElementById: () => null,
      querySelectorAll: () => [],
      createElement: () => ({style: {}, remove: () => {}}),
    },
  };
  context.globalThis = context;
  vm.createContext(context);
  // namespace.js first, as on the page: it supplies Sidebar.wire and the
  // currentFilter/sessionUnread state the filters module reads.
  vm.runInContext(readStatic('sidebar/namespace.js'), context, {filename: 'namespace.js'});
  // The other modules the page loads between namespace and filters only add
  // render helpers; the archive path touches Sidebar.removeSessionFromRenderedList.
  context.Sidebar.removeSessionFromRenderedList = () => true;
  vm.runInContext(readStatic('sidebar/filters.js'), context, {filename: 'filters.js'});
  return context;
}

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

  await context.archiveSession('leaf-1');

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

  await context.archiveSession('leaf-1');
  await context.archiveSession('leaf-1');

  assert.deepEqual(toasts, [['archiving a task requires operator credentials', true], ['Archive failed: 500', true]]);
});

test('a successful archive removes the row and shows no toast', async () => {
  const toasts = [];
  let removed = 0;
  const context = buildContext({
    fetchImpl: async () => ({ok: true, status: 200, json: async () => ({archived: ['leaf-1']})}),
    showToastImpl: (msg) => toasts.push(msg),
  });
  context.Sidebar.removeSessionFromRenderedList = () => {
    removed += 1;
    return true;
  };

  await context.archiveSession('leaf-1');

  assert.equal(removed, 1);
  assert.deepEqual(toasts, []);
});


// ---------------------------------------------------------------------------
// The unarchive entry: the restored-ids answer drops the whole restored chain
// from the archived list, the legacy metadata answer drops only the clicked
// row, and a refusal rides the same toast the archive refusal rides.
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
