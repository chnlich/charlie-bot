// ---------------------------------------------------------------------------
// Archived view mechanism assertions: the view is one project-grouped tree
// (the Workspace tree format) fed by keyset-paginated fetches — pages merge
// into it, a delivered firing nests under its scheduled context node whatever
// page each arrived on, a context ancestor renders dimmed and takes none of
// the archived row's actions, the status poll id set excludes archived rows,
// and in-list operations touch only the target row and the strip counts. The
// render cap counts archived rows only. Harness follows
// test_sidebar_delete_backfill.test.js (session_context_stub +
// createChatSidebarContext, timers inline).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { anchorOpenTag, baseSessionContext, buildSidebarFilterElements, countRows, createChatSidebarContext,
  inlinePageTimers, makeSessionMeta, rowHtml } = require('./session_context_stub');

function makeArchivedSession(id, overrides = {}) {
  return makeSessionMeta(id, {status: 'archived', ...overrides});
}

// A context ancestor: the unarchived scheduled node an archived page carries
// so its delivered firings can nest under it. Bound-node form, marked
// context_only — the field exists only on /archived rows.
function makeContextSession(id, overrides = {}) {
  return makeSessionMeta(id, {
    context_only: true,
    schedule_task: `task-${id}`,
    schedule_cron: '0 9 * * *',
    schedule_timezone: 'America/Los_Angeles',
    schedule_next_run: '2026-04-03T04:00:00Z',
    schedule_enabled: true,
    ...overrides,
  });
}

function makePage(sessions, {hasMore = false, groups = null} = {}) {
  // Group aggregates count archived rows only; a context ancestor is the
  // tree's scaffolding, never part of the user's archived list.
  const counted = sessions.filter((s) => !s.context_only);
  const counts = new Map();
  (groups ? [] : counted).forEach((s) => {
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
  const fetchCalls = [];
  const {context, elements} = baseSessionContext(overrides);

  // Fork mutations land before createChatSidebarContext: the loaded modules
  // bind or shadow these globals at load time.
  context.SESSION_ID = 'session-live';
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = overrides.querySelectorAll || (() => []);
  context.document.querySelector = () => null;
  context.switchSession = async () => {};
  context.renderNoActiveSessionView = () => {};
  context.confirm = () => true;
  context.alert = () => {};
  const innerFetch = overrides.fetch || (async (url) => { throw new Error('unexpected fetch ' + url); });
  context.fetch = async (url, opts = {}) => {
    fetchCalls.push(url);
    return innerFetch(url, opts);
  };
  createChatSidebarContext(context);
  return {context, elements, fetchCalls};
}

// The Settings button's data attributes, read back off the rendered row: the
// dataset openSessionRowMenu builds its item list from.
function settingsDataset(html, id) {
  const tag = rowHtml(html, id).match(/<button[^>]*title="Settings"[^>]*>/);
  if (!tag) throw new Error(`Missing rendered Settings button for ${id}`);
  const dataset = {};
  for (const m of tag[0].matchAll(/data-([\w-]+)="([^"]*)"/g)) {
    dataset[m[1].replace(/-(\w)/g, (_, c) => c.toUpperCase())] = m[2];
  }
  return dataset;
}

function archivedContext(pages) {
  const nav = createElement();
  let call = 0;
  const {context, fetchCalls} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ...buildSidebarFilterElements(),
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

test('entering the archived tab renders one merged project-grouped tree with the strip', async () => {
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
  // One tree, two project groups, "(No group)" last — the Workspace format.
  const groupKeys = [...rows.innerHTML.matchAll(/data-sgroup-key="([^"]*)"/g)].map((m) => m[1]);
  assert.deepEqual([...new Set(groupKeys)], ['Work', '']);
  assert.equal(countRows(rows.innerHTML), 100);
  // Archived rows format their time at build: no data-time, so the periodic
  // relative-time sweep never walks this list.
  assert.doesNotMatch(rows.innerHTML, /data-time=/);
  // Archived rows carry the archived row form: unarchive/delete actions, and
  // none of the live-state indicators archived sessions cannot carry.
  assert.match(rowHtml(rows.innerHTML, 'arch-000'), /title="Unarchive"/);
  assert.doesNotMatch(rows.innerHTML, /id="spinner-/);
  assert.doesNotMatch(rows.innerHTML, /id="unread-/);
});

test('a delivered firing nests under its scheduled context node, and pages dedup', async () => {
  // Page 1: a firing of a scheduled task, archived, whose bound node is still
  // active — so the firing arrives before the ancestor it nests under.
  const page1 = [
    makeArchivedSession('firing', {task_parent_id: 'node', updated_at: '2026-04-02T10:00:00Z'}),
    makeArchivedSession('solo', {group: 'Work'}),
  ];
  // Page 2: the context ancestor plus a re-delivery of a row already on
  // screen — the merge keeps one copy of each.
  const page2 = [
    makeContextSession('node'),
    makeArchivedSession('solo', {group: 'Work'}),
  ];
  const {context, nav} = archivedContext([
    makePage(page1, {hasMore: true, groups: [{group: 'Work', total: 1}]}),
    makePage(page2, {hasMore: false, groups: [{group: 'Work', total: 1}]}),
  ]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const rows = nav.children[1];
  // Before the ancestor arrives the firing renders as a root.
  assert.ok(!rows.innerHTML.includes('data-tree-children="node"'),
      'a firing whose bound node is not yet delivered renders as a root');

  context.loadArchivedNextPage();
  await new Promise(setImmediate);

  // The repaint re-derives the nesting: the firing sits inside its scheduled
  // node's subtree container, and the re-delivered row did not duplicate.
  const container = rows.innerHTML.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="node">/);
  assert.ok(container, 'the context node is followed by its children container');
  assert.ok(rows.innerHTML.indexOf('data-tree-children="node"') < rows.innerHTML.indexOf('id="session-firing"'));
  assert.equal(countRows(rows.innerHTML), 3);
  assert.equal((rows.innerHTML.match(/id="session-solo"/g) || []).length, 1);
  // The context node renders the bound-node form: schedule line and clock.
  const ctxRow = rowHtml(rows.innerHTML, 'node');
  assert.match(ctxRow, /0 9 \* \* \*/);
  assert.match(ctxRow, /Next: /);
  assert.match(anchorOpenTag(rows.innerHTML, 'node'), /opacity-60/);
});

test('a context row is not a row the user archived: dimmed, active-tagged, star only', async () => {
  const sessions = [
    makeContextSession('node'),
    makeArchivedSession('arch-a', {group: null}),
  ];
  const {context, nav} = archivedContext([makePage(sessions)]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const rows = nav.children[1];

  const ctxTag = anchorOpenTag(rows.innerHTML, 'node') + rowHtml(rows.innerHTML, 'node');
  assert.match(ctxTag, /opacity-60/);
  assert.match(ctxTag, /active<\/span>/);
  assert.match(rowHtml(rows.innerHTML, 'node'), /star-btn/);
  assert.doesNotMatch(rowHtml(rows.innerHTML, 'node'), /title="Unarchive"|title="Delete"|title="Set group"/);
  // The archived row next to it keeps its own form.
  assert.match(rowHtml(rows.innerHTML, 'arch-a'), /title="Unarchive"/);
  assert.doesNotMatch(anchorOpenTag(rows.innerHTML, 'arch-a'), /opacity-60/);
});

test('an archived row\u2019s direct buttons are Star, Unarchive and the archived Settings', async () => {
  const sessions = [makeArchivedSession('arch-a', {group: 'Work'})];
  const {context, nav} = archivedContext([makePage(sessions)]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const row = rowHtml(nav.children[1].innerHTML, 'arch-a');

  assert.match(row, /title="Star"/);
  assert.match(row, /title="Unarchive"/);
  assert.match(row, /title="Settings"/);
  // The group move and the delete are menu items now: no Set group button and
  // no direct delete on the row.
  assert.doesNotMatch(row, /title="Set group"|confirmDeletePermanently/);
  // The Settings button reuses the normal row's markup and data attributes
  // (archived.js's [data-current-group] rewrite keys on data-current-group),
  // plus the marker that selects the archived item list.
  const tag = row.match(/<button[^>]*title="Settings"[^>]*>/)[0];
  assert.match(tag, /openSessionRowMenu\(this, 'arch-a'\)/);
  assert.match(tag, /data-row-menu="archived"/);
  assert.match(tag, /data-current-group="Work"/);
  assert.match(tag, /data-task-parent=""/);
});

test('the archived Settings menu: root moves groups then deletes; a child only deletes', async () => {
  const sessions = [
    makeArchivedSession('root', {group: 'Work'}),
    makeArchivedSession('child', {group: 'Work', task_parent_id: 'root'}),
  ];
  const {context, nav} = archivedContext([makePage(sessions)]);

  // The touch branch keys off (hover: none) at open time; the desktop list is
  // asserted with the media query not matching.
  context.window.matchMedia = (query) => ({matches: false, media: query});
  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const deletes = [];
  context.confirmDeletePermanently = (id) => deletes.push(id);
  let menu = null;
  context.openRowMenu = (anchor, built) => { menu = built; };
  const open = (id) => {
    context.Sidebar.openSessionRowMenu(
      {dataset: settingsDataset(nav.children[1].innerHTML, id)}, id);
    const items = [...menu];
    menu = null;
    return items;
  };
  const click = {preventDefault() {}, stopPropagation() {}};

  // A root archived row: Move to group…, a separator, then the danger
  // delete -- the menu keeps the old direct buttons' condition and styling.
  const root = open('root');
  assert.deepEqual(root.filter((item) => !item.separator).map((item) => item.label),
    ['Move to group\u2026', 'Delete permanently']);
  assert.deepEqual(root.map((item) => !!item.separator), [false, true, false]);
  assert.equal(root[2].danger, true);
  root[2].onSelect(click);
  assert.deepEqual(deletes, ['root']);

  // A task-tree child moves with its parent: the delete alone, no separator.
  const child = open('child');
  assert.deepEqual(child.map((item) => item.label), ['Delete permanently']);
  assert.equal(child[0].danger, true);
  child[0].onSelect(click);
  assert.deepEqual(deletes, ['root', 'child']);
});

test('touch archived menus lead with the Later toggle and Unarchive; a child keeps the separator', async () => {
  const sessions = [
    makeArchivedSession('root', {group: 'Work'}),
    makeArchivedSession('child', {group: 'Work', task_parent_id: 'root'}),
  ];
  const {context, nav} = archivedContext([makePage(sessions)]);

  context.window.matchMedia = (query) => ({matches: true, media: query});
  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const calls = [];
  const click = {preventDefault() {}, stopPropagation() {}};
  context.toggleSessionStar = (...args) => calls.push(['star', ...args]);
  context.unarchiveSession = (...args) => calls.push(['unarchive', ...args]);
  context.confirmDeletePermanently = (id) => calls.push(['delete', id]);
  let menu = null;
  context.openRowMenu = (anchor, built) => { menu = built; };
  const open = (id) => {
    context.Sidebar.openSessionRowMenu(
      {dataset: settingsDataset(nav.children[1].innerHTML, id)}, id);
    const items = [...menu];
    menu = null;
    return items;
  };

  // A root archived row: the two touch leads, the group move, then the
  // separator and the danger delete.
  const root = open('root');
  assert.deepEqual(root.map((item) => item.separator ? '---' : item.label),
    ['Add to Later', 'Unarchive', 'Move to group\u2026', '---', 'Delete permanently']);
  assert.equal(root[4].danger, true);
  root[0].onSelect(click);
  root[1].onSelect(click);
  root[4].onSelect(click);
  assert.deepEqual(calls, [['star', 'root', false], ['unarchive', 'root'], ['delete', 'root']]);

  // A task-tree child: the group move drops out, the separator stays.
  const child = open('child');
  assert.deepEqual(child.map((item) => item.separator ? '---' : item.label),
    ['Add to Later', 'Unarchive', '---', 'Delete permanently']);
  assert.equal(child[3].danger, true);
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

  assert.deepEqual(Array.from(context.Sidebar.sidebarSessionIds()), ['session-live']);
  assert.ok(nav.children[1].innerHTML.includes('arch-0'));
});

test('set group updates the strip counts; the row leaves only a mismatched filter', async () => {
  const sessions = [
    makeArchivedSession('arch-a', {group: 'Work'}),
    makeArchivedSession('arch-b', {group: null}),
  ];
  const personalPage = makePage([makeArchivedSession('arch-a', {group: 'Personal'})], {
    groups: [{group: 'Personal', total: 1}, {group: null, total: 1}],
  });
  const {context, nav} = archivedContext([makePage(sessions, {hasMore: false}), personalPage]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  let rows = nav.children[1];
  const treeHtml = rows.innerHTML;
  const pills = nav.children[0];

  // Under the "All" strip filter the row stays in the tree.
  context.applyArchivedGroupChange('arch-a', 'Personal');
  assert.equal(rows.innerHTML, treeHtml); // no repaint under All
  assert.match(pills.innerHTML, /Personal <span[^>]*>1<\/span>/);
  assert.doesNotMatch(pills.innerHTML, /Work <span[^>]*>1<\/span>/);

  // The stored row moved with the counts: the repaint an unarchive brings
  // lands arch-a under its new group header instead of reverting it to Work.
  context.archivedForgetSession('arch-b');
  assert.match(rows.innerHTML, /data-sgroup-key="Personal"[\s\S]*id="session-arch-a"/);
  assert.doesNotMatch(rows.innerHTML, /data-sgroup-key="Work"/);

  // Under a named strip filter, a row moved elsewhere leaves the tree: the
  // repaint drops it and re-derives the nesting for the rows that stay. A
  // strip-filter change rebuilds the view's three containers, so re-read.
  context.setArchivedGroupFilter('Personal');
  await new Promise(setImmediate);
  rows = nav.children[1];
  assert.match(rows.innerHTML, /id="session-arch-a"/);
  context.applyArchivedGroupChange('arch-a', 'Work');
  // arch-a was the filter's only row, so its departure empties the tree.
  assert.doesNotMatch(rows.innerHTML, /id="session-arch-a"/);
  assert.match(rows.innerHTML, /No archived sessions/);
});

test('a star toggle survives the repaint a page append brings', async () => {
  const pages = [
    makePage([makeArchivedSession('arch-a'), makeArchivedSession('arch-b')], {hasMore: true}),
    makePage([makeArchivedSession('arch-c')]),
  ];
  const nav = createElement();
  let call = 0;
  const {context} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ...buildSidebarFilterElements(),
    ]),
    fetch: async (url) => {
      if (url.startsWith('/api/sessions/arch-a/star')) return {ok: true, async json() { return {}; }};
      assert.match(url, /^\/api\/sessions\/archived\?limit=100/);
      const page = pages[Math.min(call, pages.length - 1)];
      call += 1;
      return {ok: true, async json() { return page; }};
    },
  });

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const rows = nav.children[1];
  assert.match(rowHtml(rows.innerHTML, 'arch-a'), /fill="none"/);

  await context.toggleSessionStar('arch-a', false);
  // The append repaints the whole tree from the merged list; the stored row
  // carries the flip, so the star stays painted.
  context.loadArchivedNextPage();
  await new Promise(setImmediate);
  assert.match(rowHtml(rows.innerHTML, 'arch-a'), /fill="currentColor"/);
});

test('unarchive/delete bookkeeping decrements the strip counts and repaints the tree', async () => {
  const sessions = [
    makeArchivedSession('arch-a', {group: 'Work'}),
    makeArchivedSession('arch-b', {group: 'Work'}),
    makeArchivedSession('arch-c', {group: null}),
  ];
  const {context, nav} = archivedContext([makePage(sessions)]);

  context.switchSidebarFilter('archived');
  await new Promise(setImmediate);
  const pills = nav.children[0];
  const rows = nav.children[1];
  assert.match(pills.innerHTML, /All <span[^>]*>3<\/span>/);
  assert.match(pills.innerHTML, /Work <span[^>]*>2<\/span>/);

  context.archivedForgetSession('arch-a');
  assert.match(pills.innerHTML, /All <span[^>]*>2<\/span>/);
  assert.match(pills.innerHTML, /Work <span[^>]*>1<\/span>/);
  // The tree repaint drops the row; the other Work row stays.
  assert.doesNotMatch(rows.innerHTML, /id="session-arch-a"/);
  assert.match(rows.innerHTML, /id="session-arch-b"/);

  context.archivedForgetSession('arch-c');
  assert.match(pills.innerHTML, /All <span[^>]*>1<\/span>/);
  assert.doesNotMatch(pills.innerHTML, /\(No group\)/);
});

test('the render cap counts archived rows only and Load more continues the walk', async () => {
  // Each page: 100 archived rows plus one context ancestor.
  const pageOf = (n) => [
    ...Array.from({length: 100}, (_, i) => makeArchivedSession(`c${n}-${String(i).padStart(3, '0')}`)),
    makeContextSession(`node-${n}`),
  ];
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
  // 2000 archived rows reached the cap; the 20 context ancestors ride along
  // without advancing it.
  assert.equal(countRows(rows.innerHTML), 2020);
  assert.match(foot.innerHTML, /Load more/);

  // The scroll path is capped out; the explicit button still appends.
  context.loadArchivedNextPage();
  await new Promise(setImmediate);
  assert.equal(countRows(rows.innerHTML), 2121);
  assert.match(foot.innerHTML, /Load more/);
});
