// ---------------------------------------------------------------------------
// The sidebar archive entry's refusal display (sidebar/filters.js): a failed
// DELETE /api/sessions/{id} surfaces the server's detail to the user through
// the page toast — the 409 blockers shape (message + blocker list), a plain
// string detail, and the status line when the refusal carries no body — while
// a success removes the row and stays quiet.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');

function buildContext({fetchImpl, showToastImpl}) {
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
    archivedForgetSession: () => {},
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
