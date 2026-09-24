// ---------------------------------------------------------------------------
// Sidebar task-tree nesting (groups.js): a child task node renders under its
// parent row, collapsed by default behind an in-memory expand state; the
// five-row preview cap counts root rows only; a worker leaf carries the leaf
// icon and the archive action alone. Harness follows
// test_sidebar_delete_backfill.test.js (session_context_stub +
// createChatSidebarContext, timers inline).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { baseSessionContext, buildSidebarFilterElements, createChatSidebarContext, inlinePageTimers,
  makeSessionMeta } = require('./session_context_stub');

function buildContext() {
  const nav = createElement();
  const elements = new Map([['session-list', nav], ...buildSidebarFilterElements()]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = 'none';
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  context.fetch = async (url) => { throw new Error('unexpected fetch ' + url); };
  createChatSidebarContext(context);
  return {context, nav};
}

const at = (hour) => `2026-04-02T${String(hour).padStart(2, '0')}:00:00Z`;

function meta(id, overrides = {}) {
  return makeSessionMeta(id, {group: 'Work', status: 'active', ...overrides});
}

function manager(id, parent, hour, overrides = {}) {
  return meta(id, {profile: 'manager', task_parent_id: parent, updated_at: at(hour), ...overrides});
}

function worker(id, parent, hour, overrides = {}) {
  return meta(id, {profile: 'worker', task_parent_id: parent, updated_at: at(hour), ...overrides});
}

// One root with two logical children (one carrying a worker grandchild) and
// two worker leaves, listed in an order the renderer must not keep.
function familyRows() {
  return [
    manager('r1', null, 10),
    worker('w-old', 'r1', 9),
    manager('c-old', 'r1', 8),
    worker('g1', 'c-new', 7),
    worker('w-new', 'r1', 11),
    manager('c-new', 'r1', 9),
  ];
}

function anchorIdsInOrder(html) {
  return [...html.matchAll(/<a\b[^>]*id="session-([^"]+)"/g)].map((m) => m[1]);
}

function anchorOpenTag(html, id) {
  const match = html.match(new RegExp(`<a\\b[^>]*id="session-${id}"[^>]*>`));
  if (!match) throw new Error(`Missing rendered session anchor for ${id}`);
  return match[0];
}

function rowHtml(html, id) {
  const start = html.indexOf(`id="session-${id}"`);
  if (start === -1) throw new Error(`Missing rendered session anchor for ${id}`);
  return html.slice(start, html.indexOf('</a>', start));
}

test('child task rows nest under their parent, collapsed by default', () => {
  const {context, nav} = buildContext();

  context.renderSessionList(familyRows(), 'all');

  const html = nav.innerHTML;
  const container = html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/);
  assert.ok(container, 'the root row is followed by its children container');
  assert.match(container[0], / hidden"/);
  assert.ok(html.indexOf('data-tree-children="r1"') < html.indexOf('id="session-c-new"'));
  assert.match(anchorOpenTag(html, 'r1') + rowHtml(html, 'r1'), /data-tree-toggle="r1"[^>]*aria-expanded="false"/);
  assert.doesNotMatch(rowHtml(html, 'w-old'), /data-tree-toggle/);
  // The group count badge counts every rendered row, roots and descendants.
  assert.match(html, /<span class="text-xs text-slate-500 ml-auto">6<\/span>/);
});

test('children order logical sessions before worker leaves, newest first, grandchildren under their parent', () => {
  const {context, nav} = buildContext();

  context.renderSessionList(familyRows(), 'all');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['r1', 'c-new', 'g1', 'c-old', 'w-new', 'w-old']);
  // Spread the vm-context arrays: strict deep equality compares prototypes.
  assert.deepEqual([...context.Sidebar.treeChildIds('r1')], ['c-new', 'c-old', 'w-new', 'w-old']);
  assert.deepEqual([...context.Sidebar.treeChildIds('c-new')], ['g1']);
  assert.deepEqual([...context.Sidebar.treeChildIds('nobody')], []);
});

test('a row whose parent is absent from the list renders as a root', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([worker('orphan', 'archived-parent', 5), meta('legacy')], 'all');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['orphan', 'legacy']);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children/);
  assert.doesNotMatch(nav.innerHTML, /data-tree-toggle/);
});

test('the five-row preview counts root rows only and hides a capped root with its subtree', () => {
  const {context, nav} = buildContext();
  const rows = [];
  for (let i = 1; i <= 6; i++) rows.push(manager(`work-${i}`, null, 20 - i));
  for (let i = 1; i <= 3; i++) rows.push(worker(`w1-${i}`, 'work-1', i));
  rows.push(worker('w6-1', 'work-6', 1));

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.match(html, />Show all<\/button>/);
  assert.equal(anchorOpenTag(html, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(anchorOpenTag(html, 'work-6').includes('session-group-limit-extra hidden'), true);
  for (let i = 1; i <= 3; i++) {
    assert.equal(anchorOpenTag(html, `w1-${i}`).includes('session-group-limit-extra'), false);
  }
  assert.match(html, /<div class="tree-subtree session-group-limit-extra hidden[^"]*" data-session-group-limit-extra="Work">/);
  assert.match(html, /<span class="text-xs text-slate-500 ml-auto">10<\/span>/);
});

test('four roots with many children stay under the preview cap', () => {
  const {context, nav} = buildContext();
  const rows = [];
  for (let i = 1; i <= 4; i++) rows.push(manager(`work-${i}`, null, 20 - i));
  for (let i = 1; i <= 8; i++) rows.push(worker(`w1-${i}`, 'work-1', i));

  context.renderSessionList(rows, 'all');

  assert.doesNotMatch(nav.innerHTML, /Show all/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra/);
});

test('toggleTreeNode flips the in-memory state, the rendered subtree and the chevron', () => {
  const {context, nav} = buildContext();
  const container = createElement({className: 'tree-children hidden'});
  const chevron = createElement({className: 'tree-chevron'});
  let refreshes = 0;
  context.document.querySelectorAll = (selector) => {
    if (selector === '[data-tree-children="r1"]') return [container];
    if (selector === '[data-tree-toggle="r1"]') return [chevron];
    return [];
  };
  context.Sidebar.refreshSessionIndicator = () => { refreshes += 1; };

  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), false);
  context.toggleTreeNode('r1');

  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), true);
  assert.equal(container.classList.contains('hidden'), false);
  assert.equal(chevron.classList.contains('rotate-90'), true);
  assert.equal(chevron['aria-expanded'], 'true');
  assert.equal(refreshes, 1);

  // A repaint while expanded renders the subtree open and the chevron turned.
  context.renderSessionList(familyRows(), 'all');
  const open = nav.innerHTML.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/);
  assert.doesNotMatch(open[0], /hidden/);
  assert.match(rowHtml(nav.innerHTML, 'r1'), /rotate-90"[^>]*data-tree-toggle="r1"[^>]*aria-expanded="true"/);

  context.toggleTreeNode('r1');
  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), false);
  assert.equal(container.classList.contains('hidden'), true);
  assert.equal(chevron['aria-expanded'], 'false');
});

test('a worker leaf row shows the leaf icon and keeps the archive action alone', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11)], 'all');

  const leaf = rowHtml(nav.innerHTML, 'w-new');
  assert.match(leaf, /Worker \(implementation leaf\)/);
  assert.match(leaf, /title="Archive"/);
  assert.doesNotMatch(leaf, /star-btn|title="Rename"|title="Set group"|title="Edit task config"/);
  const parent = rowHtml(nav.innerHTML, 'r1');
  assert.doesNotMatch(parent, /Worker \(implementation leaf\)/);
  assert.match(parent, /star-btn/);
  assert.match(parent, /title="Set group"/);
});

test('search results stay flat: no nesting, no chevron', () => {
  const {context, nav} = buildContext();

  context.renderSessionList(familyRows(), 'search');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['r1', 'w-old', 'c-old', 'g1', 'w-new', 'c-new']);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children|data-tree-toggle/);
});

test('a logical row offers New child session; a worker leaf does not', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11), meta('legacy')], 'all');

  assert.match(rowHtml(nav.innerHTML, 'r1'), /title="New child session"[\s\S]*?createChildSession\('r1'\)|createChildSession\('r1'\)[\s\S]*?title="New child session"/);
  assert.match(rowHtml(nav.innerHTML, 'legacy'), /createChildSession\('legacy'\)/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'w-new'), /New child session|createChildSession/);
});

test('a logical row offers Task & context between New child session and Archive; a worker leaf does not', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11), meta('legacy')], 'all');

  const r1 = rowHtml(nav.innerHTML, 'r1');
  assert.match(r1, /title="Task &amp; context"/);
  assert.ok(r1.indexOf("createChildSession('r1')") < r1.indexOf("openTaskContextModal('r1')"));
  assert.ok(r1.indexOf("openTaskContextModal('r1')") < r1.indexOf("archiveSession('r1')"));
  assert.match(rowHtml(nav.innerHTML, 'legacy'), /openTaskContextModal\('legacy'\)/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'w-new'), /Task &amp; context|openTaskContextModal/);
});

test('the active session’s ancestors open once per switch and a manual collapse then holds', () => {
  const {context, nav} = buildContext();
  context.SESSION_ID = 'g1';

  context.renderSessionList(familyRows(), 'all');

  let html = nav.innerHTML;
  assert.doesNotMatch(html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/)[0], /hidden/);
  assert.doesNotMatch(html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="c-new">/)[0], /hidden/);
  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), true);
  assert.equal(context.Sidebar.isTreeNodeExpanded('c-new'), true);
  assert.equal(context.Sidebar.treeParentId('g1'), 'c-new');
  assert.equal(context.Sidebar.treeParentId('r1'), null);

  // The user folds the root: the next repaint of the same session keeps it folded.
  context.Sidebar.refreshSessionIndicator = () => {};
  context.toggleTreeNode('r1');
  context.renderSessionList(familyRows(), 'all');
  html = nav.innerHTML;
  assert.match(html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/)[0], / hidden"/);

  // A switch to another nested session reveals its path again.
  context.SESSION_ID = 'w-old';
  context.renderSessionList(familyRows(), 'all');
  assert.doesNotMatch(nav.innerHTML.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/)[0], /hidden/);
});

test('expandTreeNode opens a collapsed parent and is a no-op on an open one', () => {
  const {context} = buildContext();
  const container = createElement({className: 'tree-children hidden'});
  const chevron = createElement({className: 'tree-chevron'});
  let refreshes = 0;
  context.document.querySelectorAll = (selector) => {
    if (selector === '[data-tree-children="p1"]') return [container];
    if (selector === '[data-tree-toggle="p1"]') return [chevron];
    return [];
  };
  context.Sidebar.refreshSessionIndicator = () => { refreshes += 1; };

  context.Sidebar.expandTreeNode('p1');
  assert.equal(context.Sidebar.isTreeNodeExpanded('p1'), true);
  assert.equal(container.classList.contains('hidden'), false);
  assert.equal(chevron.classList.contains('rotate-90'), true);
  assert.equal(refreshes, 1);

  context.Sidebar.expandTreeNode('p1');
  assert.equal(refreshes, 1, 'an already open node is left alone');
});

test('an archived worker row shows the delivered check in place of the leaf icon', () => {
  const {context, nav} = buildContext();
  context.renderSessionList([
    manager('r1', null, 10, {name: 'Root'}),
    worker('w1', 'r1', 9, {name: 'Live worker'}),
    worker('w2', 'r1', 8, {name: 'Done worker', status: 'archived'}),
  ], 'all');
  const html = nav.innerHTML;
  const liveRow = html.slice(html.indexOf('id="session-w1"'), html.indexOf('id="session-w2"'));
  const doneRow = html.slice(html.indexOf('id="session-w2"'));
  assert.match(liveRow, /title="Worker \(implementation leaf\)"/);
  assert.doesNotMatch(liveRow, /title="Worker \(delivered\)"/);
  assert.match(doneRow, /title="Worker \(delivered\)"[^>]*>[\s\S]*?M5 13l4 4L19 7/);
  assert.doesNotMatch(doneRow, /title="Worker \(implementation leaf\)"/);
});
