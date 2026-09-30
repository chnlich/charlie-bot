// ---------------------------------------------------------------------------
// The Threads sidebar filter (filters.js + groups.js): the pill registers
// between Workspace and Later and restores from ?filter=threads; its view
// fetches /api/sessions/chat-threads, renders through the grouped renderer
// (children nested under their parent), shows "No chat threads" when empty,
// and hides the group header's New-session-in-group button across the in-place
// repaints (the Settings gear stays). Harness follows
// sidebar_group_create_button.test.js; the full sidebar module set loads
// through session_context_stub.js because the pill strip lives in filters.js.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {
  baseSessionContext,
  buildSidebarFilterElements,
  createChatSidebarContext,
  inlinePageTimers,
  makeSessionMeta,
} = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

const PILL_IDS = ['filter-all', 'filter-threads', 'filter-starred', 'filter-archived'];

function buildContext({search = '', sessions = []} = {}) {
  const fetchRequests = [];
  const nav = createElement();
  const pillsContainer = createElement();
  const elements = new Map([
    ['session-list', nav],
    ['sidebar-filter-pills', pillsContainer],
    ...buildSidebarFilterElements(),
  ]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = 'none';
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  context.location.search = search;
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  context.fetch = async (url) => {
    fetchRequests.push(url);
    return {ok: true, json: async () => sessions};
  };
  createChatSidebarContext(context);
  return {context, nav, pillsContainer, fetchRequests};
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

function threadRows() {
  return [
    makeSessionMeta('th-root', {name: 'Discord #general thread', group: 'Discord #general'}),
    makeSessionMeta('th-child', {name: 'thread child', group: null, task_parent_id: 'th-root'}),
  ];
}

test('the pill strip renders Threads between Workspace and Later, Archive last', () => {
  const {context, pillsContainer} = buildContext();

  context.restoreSidebarFromUrl();

  const html = pillsContainer.innerHTML;
  const positions = PILL_IDS.map((id) => html.indexOf(`id="${id}"`));
  assert.ok(positions.every((p) => p >= 0), `all four pills render: ${html}`);
  assert.deepEqual([...positions].sort((a, b) => a - b), positions, 'registry order: all, threads, starred, archived');
  assert.match(html, />Threads<\/button>/);
  // restoreFromUrl stays on for the new pill (the default): it is restorable.
  assert.match(html, /enterSidebarFilter\('threads'\)/);
});

test('?filter=threads survives a reload and fetches the chat-threads list', async () => {
  const {context, nav, fetchRequests} = buildContext({search: '?filter=threads', sessions: threadRows()});

  context.restoreSidebarFromUrl();
  await flush();

  assert.deepEqual(fetchRequests, ['/api/sessions/chat-threads']);
  assert.ok(context.document.getElementById('filter-threads').classList.contains('bg-blue-600/20'),
            'Threads is the active pill');
  // The grouped renderer draws the list unchanged: the named group with the
  // child nested under its parent row.
  assert.match(nav.innerHTML, /Discord #general/);
  assert.match(nav.innerHTML, /data-task-parent="th-root"/);
});

test('an empty Threads view shows the No-chat-threads note', async () => {
  const {context, nav} = buildContext({search: '?filter=threads', sessions: []});

  context.restoreSidebarFromUrl();
  await flush();

  assert.match(nav.innerHTML, /No chat threads/);
});

test('the Threads view hides the group-header create button, in-place repaints included', async () => {
  const {context, nav} = buildContext({search: '?filter=threads', sessions: threadRows()});

  context.restoreSidebarFromUrl();
  await flush();

  assert.equal(nav.innerHTML.includes('createSessionInGroup'), false, 'no create button on Threads');
  assert.match(nav.innerHTML, /openGroupHeaderMenu\(this\)/, 'the Settings gear stays');

  // The in-place delete repaint repaints from the last-rendered args with no
  // refetch — the gate derives from the filter, so the button stays hidden.
  const repainted = context.Sidebar.removeSessionFromRenderedList('th-root');
  assert.equal(repainted, true);
  assert.equal(nav.innerHTML.includes('createSessionInGroup'), false, 'still no create button after the repaint');
});

test('the Workspace view keeps the group-header create button', () => {
  const {context, nav, fetchRequests} = buildContext();
  context.INITIAL_SESSIONS = threadRows();

  context.restoreSidebarFromUrl();

  assert.deepEqual(fetchRequests, ['/api/cron/tasks']);
  assert.match(nav.innerHTML, /createSessionInGroup\(this\.dataset\.groupName\)/);
});
