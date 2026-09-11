// ---------------------------------------------------------------------------
// Inline-delete backfill for the sidebar's grouped views: archive and
// permanent-delete repaint the grouped list in place from the last-rendered
// list minus the session, so the 5-row preview window backfills and the group
// header count and Show-all toggle resync to the remaining total — with zero
// list fetches. The archived tab and the search overlay keep node-only row
// removal, and deleting the viewed session keeps its switch/empty behavior.
// Harness follows test_archived_view.test.js (session_context_stub +
// createChatSidebarContext); timers run inline, so the PM refresh fires inside
// the paint call and a delete-path fetch leak cannot hide behind a timer.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { baseSessionContext, buildSidebarFilterElements, createChatSidebarContext } = require('./session_context_stub');

function makeSession(id, overrides = {}) {
  return {
    id,
    name: `Session ${id}`,
    group: 'Work',
    status: 'active',
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

function makeCronSession(id, overrides = {}) {
  return makeSession(id, {
    group: null,
    schedule_project: 'CronProj',
    scheduled_task: `task-${id}`,
    schedule_cron: '0 9 * * *',
    schedule_timezone: 'America/Los_Angeles',
    schedule_next_run: '2026-04-03T04:00:00Z',
    ...overrides,
  });
}

function rowStubs(ids) {
  return ids.map((id) => createElement({tagName: 'A', id: 'session-' + id}));
}

// Archive / permanent-delete succeed; every other URL (the PM refresh's
// scheduled + cron pulls, list fetches) throws, so a delete-flow refetch
// cannot pass silently.
function deleteOkFetch() {
  return async (url, opts = {}) => {
    if (opts.method === 'DELETE' && /^\/api\/sessions\/[^/]+(\/permanent)?$/.test(url)) {
      return {ok: true, json: async () => ({})};
    }
    throw new Error('unexpected fetch ' + url);
  };
}

function buildContext({sessionId = 'session-live', rows = [], fetchHandler} = {}) {
  const fetchCalls = [];
  const nav = createElement();
  const elements = new Map([
    ['session-list', nav],
    ...buildSidebarFilterElements(),
  ]);
  rows.forEach((row) => elements.set(row.id, row));
  const {context} = baseSessionContext({elements});

  context.SESSION_ID = sessionId;
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  context.setInterval = () => 0;
  context.setTimeout = (fn) => { fn(); return 0; };
  context.clearInterval = () => {};
  context.clearTimeout = () => {};
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = (selector) => (selector === 'a[id^="session-"]' ? rows : []);
  context.document.querySelector = () => null;
  const switches = [];
  const recordSwitch = (id) => { switches.push(id); };
  context.switchSession = recordSwitch;
  const noActiveViews = [];
  const recordNoActiveView = () => { noActiveViews.push(true); };
  context.renderNoActiveSessionView = recordNoActiveView;
  context.confirm = () => true;
  context.alert = () => {};
  const innerFetch = fetchHandler || deleteOkFetch();
  context.fetch = async (url, opts = {}) => {
    fetchCalls.push(url);
    return innerFetch(url, opts);
  };
  createChatSidebarContext(context);
  // session-view.js wires its real switchSession/renderNoActiveSessionView
  // globals at load (after filters.js), so the stubs the delete path resolves
  // at call time must be re-installed over the wired ones.
  context.switchSession = recordSwitch;
  context.renderNoActiveSessionView = recordNoActiveView;
  return {context, nav, fetchCalls, switches, noActiveViews, elements};
}

const settle = () => new Promise(setImmediate);

async function paintGrouped(context, sessions) {
  context.renderSessionList(sessions, 'all');
  await settle();
}

async function paintScheduled(context, sessions) {
  context.currentFilter = 'scheduled';
  context.renderSessionList(sessions, 'scheduled');
  await settle();
}

function countRows(html) {
  return (html.match(/<a\b[^>]*id="session-/g) || []).length;
}

test('the delete repaint dispatcher is Sidebar-reachable only, never a bare onclick global', () => {
  const {context} = buildContext();
  assert.equal(context.removeSessionFromRenderedList, undefined);
  assert.equal(typeof context.Sidebar.removeSessionFromRenderedList, 'function');
});

test('archiving a visible row backfills the preview and resyncs count and toggle', async () => {
  const sessions = Array.from({length: 6}, (_, i) => makeSession(`s${i + 1}`));
  const {context, nav, fetchCalls} = buildContext();
  await paintGrouped(context, sessions);

  // Preview state before: 6 rows rendered, the 6th hidden, toggle present.
  assert.equal(countRows(nav.innerHTML), 6);
  assert.match(nav.innerHTML, /session-group-limit-extra hidden/);
  assert.match(nav.innerHTML, /Show all/);
  assert.ok(nav.innerHTML.includes('ml-auto">6</span>'));

  const baseline = fetchCalls.length;
  await context.archiveSession('s2');
  await settle();

  // Backfill: min(5, remaining) visible rows — the previously hidden s6 now
  // paints without any limit extras — and no toggle at the new total.
  assert.equal(countRows(nav.innerHTML), 5);
  assert.ok(nav.innerHTML.includes('Session s6'));
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-toggle/);
  assert.doesNotMatch(nav.innerHTML, /Show all/);
  assert.ok(nav.innerHTML.includes('ml-auto">5</span>'));
  // No-refetch contract: the whole delete flow issued exactly the DELETE call.
  assert.deepEqual(fetchCalls.slice(baseline), ['/api/sessions/s2']);
});

test('an expanded preview stays expanded across the delete repaint', async () => {
  const sessions = Array.from({length: 7}, (_, i) => makeSession(`s${i + 1}`));
  const {context, nav} = buildContext();
  await paintGrouped(context, sessions);

  context.toggleSessionGroupLimit('Work');
  await context.archiveSession('s2');
  await settle();

  // Six rows remain: still over the limit, and the repaint reads the module
  // limit state, so every row stays unhidden and the toggle reads "Show less".
  assert.equal(countRows(nav.innerHTML), 6);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra hidden/);
  assert.match(nav.innerHTML, /Show less/);
  assert.ok(nav.innerHTML.includes('ml-auto">6</span>'));
});

test('archiving on the scheduled tab backfills cron groups the same way', async () => {
  const sessions = Array.from({length: 6}, (_, i) => makeCronSession(`c${i + 1}`));
  const {context, nav, fetchCalls} = buildContext();
  await paintScheduled(context, sessions);

  assert.equal(countRows(nav.innerHTML), 6);
  assert.match(nav.innerHTML, /cron-group-limit-extra hidden/);
  assert.match(nav.innerHTML, /Show all/);
  assert.ok(nav.innerHTML.includes('ml-auto">6/6 enabled</span>'));

  const baseline = fetchCalls.length;
  await context.archiveSession('c2');
  await settle();

  assert.equal(countRows(nav.innerHTML), 5);
  assert.doesNotMatch(nav.innerHTML, /cron-group-limit-extra/);
  assert.doesNotMatch(nav.innerHTML, /cron-group-limit-toggle/);
  assert.doesNotMatch(nav.innerHTML, /Show all/);
  assert.ok(nav.innerHTML.includes('ml-auto">5/5 enabled</span>'));
  assert.deepEqual(fetchCalls.slice(baseline), ['/api/sessions/c2']);
});

test('deleting from the search overlay removes only the row node', async () => {
  const grouped = Array.from({length: 6}, (_, i) => makeSession(`s${i + 1}`));
  const searchResults = [
    makeSession('search-1', {group: null, status: 'archived'}),
    makeSession('search-2', {group: null}),
    makeSession('search-3', {group: null}),
  ];
  const deletedRow = createElement({tagName: 'A', id: 'session-search-1'});
  const {context, nav, fetchCalls} = buildContext({rows: [deletedRow]});
  // Paint a grouped list first so lastGroupedRenderArgs exists: the overlay
  // delete must not repaint it even though the args are sitting there.
  await paintGrouped(context, grouped);
  context.renderSessionList(searchResults, 'search');
  const searchHtml = nav.innerHTML;
  assert.equal(countRows(searchHtml), 3);

  const baseline = fetchCalls.length;
  await context.archiveSession('search-1');
  await settle();

  // Node-only removal: the row stub left the DOM, the remaining search rows
  // stay painted, and #session-list was never repainted (the stub stores the
  // painted HTML verbatim, so byte identity proves no render touched it).
  assert.equal(deletedRow.removed, true);
  assert.equal(nav.innerHTML, searchHtml);
  assert.ok(nav.innerHTML.includes('search-2'));
  assert.ok(nav.innerHTML.includes('search-3'));
  assert.deepEqual(fetchCalls.slice(baseline), ['/api/sessions/search-1']);
});

test('deleting on the archived tab stays node-only with the forget bookkeeping', async () => {
  const grouped = Array.from({length: 6}, (_, i) => makeSession(`s${i + 1}`));
  const row = createElement({tagName: 'A', id: 'session-arch-a'});
  const {context, nav, fetchCalls} = buildContext({rows: [row]});
  await paintGrouped(context, grouped);
  const paintedHtml = nav.innerHTML;

  context.currentFilter = 'archived';
  const forgetCalls = [];
  const realForgetSession = context.archivedForgetSession;
  context.archivedForgetSession = (id) => {
    forgetCalls.push(id);
    return realForgetSession(id);
  };
  const baseline = fetchCalls.length;

  await context.deleteSessionPermanently('arch-a');
  await settle();

  assert.equal(row.removed, true);
  assert.equal(nav.innerHTML, paintedHtml); // no grouped repaint on the archived tab
  assert.deepEqual(forgetCalls, ['arch-a']);
  assert.deepEqual(fetchCalls.slice(baseline), ['/api/sessions/arch-a/permanent']);
});

test('deleting the viewed session switches to the first remaining row', async () => {
  const sessions = Array.from({length: 6}, (_, i) => makeSession(`s${i + 1}`));
  const {context, nav, switches} = buildContext({
    sessionId: 's1',
    rows: rowStubs(['s2', 's3', 's4', 's5', 's6']),
  });
  await paintGrouped(context, sessions);

  await context.archiveSession('s1');
  await settle();

  assert.equal(countRows(nav.innerHTML), 5);
  assert.deepEqual(switches, ['s2']);
});

test('deleting the last remaining session renders the empty view', async () => {
  const {context, nav, switches, noActiveViews} = buildContext({sessionId: 's1'});
  await paintGrouped(context, [makeSession('s1')]);

  await context.archiveSession('s1');
  await settle();

  assert.match(nav.innerHTML, /No sessions yet/);
  assert.deepEqual(noActiveViews, [true]);
  assert.deepEqual(switches, []);
});
