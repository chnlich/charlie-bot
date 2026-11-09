// ---------------------------------------------------------------------------
// The Threads sidebar filter (the chat_threads package's threads-filter.js,
// filters.js and groups.js): the pill registers between Workspace and Later
// and restores from ?filter=threads. Its view fetches /api/sessions/chat-threads,
// and renders through the grouped renderer, with children nested under their
// parent. It shows "No chat threads" when empty, and hides the group header's
// New-session-in-group button across the in-place repaints (the Settings gear
// stays). Harness follows
// sidebar_group_create_button.test.js; the full sidebar module set loads
// through session_context_stub.js because the pill strip lives in filters.js.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {
  baseSessionContext,
  buildSidebarFilterElements,
  createChatSidebarContext,
  inlinePageTimers,
  makeSessionMeta,
  sessionColumns,
} = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');
const { readPackageStatic } = require('./read_static');

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
  context.INITIAL_SESSIONS = {};
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
  vm.runInContext(readPackageStatic('features', 'chat_threads', 'threads-filter.js'), context,
                  {filename: 'threads-filter.js'});
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
  assert.match(nav.innerHTML, /sidebarControl\('openGroupHeaderMenu', this\)/, 'the Settings gear stays');

  // The in-place delete repaint repaints from the last-rendered args with no
  // refetch — the gate derives from the filter, so the button stays hidden.
  const repainted = context.Sidebar.removeSessionFromRenderedList('th-root');
  assert.equal(repainted, true);
  assert.equal(nav.innerHTML.includes('createSessionInGroup'), false, 'still no create button after the repaint');
});

test('the Workspace view keeps the group-header create button', () => {
  const {context, nav, fetchRequests} = buildContext();
  context.INITIAL_SESSIONS = sessionColumns(threadRows());

  context.restoreSidebarFromUrl();

  // The restore paints the server-rendered list with no fetch: this harness
  // registers no Workspace badge loader.
  assert.deepEqual(fetchRequests, []);
  assert.match(nav.innerHTML, /sidebarControl\('createSessionInGroup', this\.dataset\.groupName\)/);
});

// ---------------------------------------------------------------------------
// The row-title rule: a Threads row under a named group header shows its title
// without the header's label (the header already names the channel), so the
// date and time read. The full name stays the row's data (data-full-name) for
// the rename field and the opening notice.
// ---------------------------------------------------------------------------

// Five rows covering every branch of the rule: two roots under one named
// header, a root whose title does not start with its header's label, a child
// row nested under a grouped root (it follows the rule its root's header sets),
// and the keyless bucket, where no header names the channel.
function threadTitleRows() {
  return [
    makeSessionMeta('th-docs-0', {name: 'Slack #docs-review 2026-11-10 12:17', group: 'Slack #docs-review'}),
    makeSessionMeta('th-docs-1', {name: 'Slack #docs-review 2026-11-09 11:10', group: 'Slack #docs-review'}),
    makeSessionMeta('th-renamed', {name: 'Renamed thread about the launch', group: 'Slack #docs-review'}),
    makeSessionMeta('th-child', {name: 'Slack #docs-review 2026-11-08 09:30', group: null,
                                 task_parent_id: 'th-docs-0'}),
    makeSessionMeta('th-ungrouped', {name: 'Slack #ops-misc 2026-11-09 10:20', group: null}),
  ];
}

test('a Threads row under a named header shows the date and time, the header keeps the label', async () => {
  const {context, nav} = buildContext({search: '?filter=threads', sessions: threadTitleRows()});

  context.restoreSidebarFromUrl();
  await flush();

  const html = nav.innerHTML;
  // The grouped roots and the child row open with their own date and time.
  assert.match(html, /session-name">2026-11-10 12:17<\/span>/);
  assert.match(html, /session-name">2026-11-09 11:10<\/span>/);
  assert.match(html, /session-name">2026-11-08 09:30<\/span>/, 'the child row follows its root’s header');
  // The full names are gone from the titles, and stay as each row's data.
  assert.equal(html.includes('session-name">Slack #docs-review'), false);
  assert.equal(html.match(/data-full-name="Slack #docs-review/g).length, 3,
               'one data-full-name per shortened row: two roots and the child');
  // The header, the non-matching title, the keyless bucket and the second line keep their text.
  assert.match(html, /uppercase tracking-wider">Slack #docs-review<\/span>/);
  assert.match(html, /session-name">Renamed thread about the launch<\/span>/);
  assert.match(html, /session-name">Slack #ops-misc 2026-11-09 10:20<\/span>/);
  assert.equal(html.includes('data-full-name="Slack #ops-misc'), false,
               'an ungrouped row keeps its full title and carries no data-full-name');
});

test('every other sidebar view keeps the full title, with no data-full-name', async () => {
  const {context, nav, fetchRequests} = buildContext();
  context.INITIAL_SESSIONS = sessionColumns(threadTitleRows());

  context.restoreSidebarFromUrl();

  assert.deepEqual(fetchRequests, []);
  const html = nav.innerHTML;
  assert.match(html, /session-name">Slack #docs-review 2026-11-10 12:17<\/span>/);
  assert.match(html, /session-name">Slack #ops-misc 2026-11-09 10:20<\/span>/);
  assert.equal(html.includes('data-full-name'), false, 'no row is shortened, so no row carries data-full-name');
});
