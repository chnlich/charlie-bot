// ---------------------------------------------------------------------------
// Sidebar task-tree nesting (groups.js): a child task node renders under its
// parent row, collapsed by default behind an in-memory expand state; the
// five-row preview cap counts root rows only, and the registered limit
// exemptions (cron.js's bound roots) show outside it; a worker leaf carries the leaf
// icon and the archive action alone. The Scheduled tab nests the same way:
// its cron rows group by project at root level with the worker leaves
// collapsed under them. Harness: session_context_stub's
// buildSidebarIndicatorContext.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { buildSidebarIndicatorContext: buildCoreSidebarContext, loadCronPackage, makeSessionMeta } =
  require('./session_context_stub');

// A bound node's schedule lines, clock badge and schedule menu items come from
// the cron script, so each harness here runs it after the core modules, as
// the page does.
function buildSidebarIndicatorContext(ids) {
  const built = buildCoreSidebarContext(ids);
  loadCronPackage(built.context);
  return built;
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

function legacy(id, hour, overrides = {}) {
  return meta(id, {profile: null, updated_at: at(hour), ...overrides});
}

// A bound node row: the task-tree manager a scheduled task fires against,
// carrying the schedule fields the row join stamps onto it.
function boundNode(id, hour, overrides = {}) {
  return meta(id, {
    group: null,
    profile: 'manager',
    schedule_task: `task-${id}`,
    schedule_cron: '0 9 * * *',
    schedule_timezone: 'America/Los_Angeles',
    schedule_next_run: '2026-04-03T04:00:00Z',
    schedule_enabled: true,
    updated_at: at(hour),
    ...overrides,
  });
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
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'all');

  const html = nav.innerHTML;
  const container = html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="r1">/);
  assert.ok(container, 'the root row is followed by its children container');
  assert.match(container[0], / hidden"/);
  assert.ok(html.indexOf('data-tree-children="r1"') < html.indexOf('id="session-c-new"'));
  assert.match(anchorOpenTag(html, 'r1') + rowHtml(html, 'r1'), /data-tree-toggle="r1"[^>]*aria-expanded="false"/);
  assert.doesNotMatch(rowHtml(html, 'w-old'), /data-tree-toggle/);
  // The group count badge counts every rendered row, roots and descendants.
  assert.match(html, /<span class="text-xs text-slate-400 ml-auto">6<\/span>/);
});

test('children order logical sessions before worker leaves, newest first, grandchildren under their parent', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'all');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['r1', 'c-new', 'g1', 'c-old', 'w-new', 'w-old']);
  // Spread the vm-context arrays: strict deep equality compares prototypes.
  assert.deepEqual([...context.Sidebar.treeChildIds('r1')], ['c-new', 'c-old', 'w-new', 'w-old']);
  assert.deepEqual([...context.Sidebar.treeChildIds('c-new')], ['g1']);
  assert.deepEqual([...context.Sidebar.treeChildIds('nobody')], []);
});

test('a row whose parent is absent from the list renders as a root', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([worker('orphan', 'archived-parent', 5), meta('legacy')], 'all');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['orphan', 'legacy']);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children/);
  // The childless logical root leads with a chevron like any tree row; the
  // worker root leads with the worker glyph and never a chevron.
  assert.match(rowHtml(nav.innerHTML, 'legacy'), /data-tree-toggle="legacy"/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'orphan'), /data-tree-toggle/);
});

test('a childless logical tree row leads with a chevron and no children container', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([meta('legacy')], 'all');

  const row = rowHtml(nav.innerHTML, 'legacy');
  assert.match(anchorOpenTag(nav.innerHTML, 'legacy') + row, /data-tree-toggle="legacy"[^>]*aria-expanded="false"/);
  assert.match(row, /title="0 child tasks"/);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children/);
});

test('toggleTreeNode turns a childless row\u2019s chevron and records it, with nothing to reveal', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const chevron = createElement({className: 'tree-chevron'});
  context.document.querySelectorAll = (selector) => {
    if (selector === '[data-tree-toggle="legacy"]') return [chevron];
    return [];
  };

  context.renderSessionList([meta('legacy')], 'all');
  context.toggleTreeNode('legacy');

  assert.equal(context.Sidebar.isTreeNodeExpanded('legacy'), true);
  assert.equal(chevron.classList.contains('rotate-90'), true);
  assert.equal(chevron['aria-expanded'], 'true');

  // A repaint keeps the chevron turned and still renders no children container.
  context.renderSessionList([meta('legacy')], 'all');
  assert.match(rowHtml(nav.innerHTML, 'legacy'), /rotate-90"[^>]*data-tree-toggle="legacy"[^>]*aria-expanded="true"/);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children/);
});

test('a worker tree row leads with the worker glyph instead of a chevron', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11)], 'all');

  const leaf = rowHtml(nav.innerHTML, 'w-new');
  const glyphAt = leaf.indexOf('title="Worker (implementation leaf)"');
  const nameAt = leaf.indexOf('class="truncate block session-name"');
  assert.ok(glyphAt !== -1 && nameAt !== -1 && glyphAt < nameAt, 'the worker glyph precedes the name');
  assert.match(leaf, /class="w-3 h-3 [^"]*" title="Worker \(implementation leaf\)"/);
  assert.doesNotMatch(leaf, /data-tree-toggle/);
});

test('a starred paint renders the childless chevron like the All paint', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([meta('legacy', {starred: true})], 'starred');

  assert.match(rowHtml(nav.innerHTML, 'legacy'), /data-tree-toggle="legacy"[^>]*aria-expanded="false"/);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children/);
});

test('a Workspace row shows its nested child count after the chevron, before the indicators', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'all');

  const row = rowHtml(nav.innerHTML, 'r1');
  assert.match(row, /<span[^>]*data-tree-child-count="r1"[^>]*>2<\/span>/);
  assert.match(row, /title="2 active child sessions"/);
  assert.match(row, /class="text-\[10px\] leading-none text-slate-400 tabular-nums flex-shrink-0"/);
  const chevronAt = row.indexOf('data-tree-toggle="r1"');
  const countAt = row.indexOf('data-tree-child-count="r1"');
  const indicatorsAt = row.indexOf('id="spinner-r1"');
  assert.ok(chevronAt !== -1 && chevronAt < countAt, 'the count follows the chevron');
  assert.ok(indicatorsAt !== -1 && countAt < indicatorsAt, 'the count precedes the indicators');
});

test('a manager with only worker children and a worker row draw no count element', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('solo', null, 10), manager('r1', null, 9), worker('w1', 'r1', 11)], 'all');

  assert.doesNotMatch(rowHtml(nav.innerHTML, 'solo'), /data-tree-child-count/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'r1'), /data-tree-child-count/);
  assert.match(anchorOpenTag(nav.innerHTML, 'r1') + rowHtml(nav.innerHTML, 'r1'),
      /data-tree-toggle="r1"[^>]*title="1 child task"/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'w1'), /data-tree-child-count/);
});

test('a bound node with only firing worker leaves draws no child count', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [boundNode('cron1', 10)];
  for (let i = 1; i <= 5; i++) rows.push(worker(`t${i}`, 'cron1', i));

  context.renderSessionList(rows, 'all');

  assert.doesNotMatch(rowHtml(nav.innerHTML, 'cron1'), /data-tree-child-count/);
  assert.match(anchorOpenTag(nav.innerHTML, 'cron1') + rowHtml(nav.innerHTML, 'cron1'),
      /data-tree-toggle="cron1"[^>]*title="5 child tasks"/);
});

test('the count follows manager children out while worker children stay nested', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'all');
  assert.match(rowHtml(nav.innerHTML, 'r1'), /data-tree-child-count="r1"[^>]*>2<\/span>/);

  context.renderSessionList(familyRows().filter(s => s.id !== 'c-new'), 'all');
  assert.match(rowHtml(nav.innerHTML, 'r1'), /data-tree-child-count="r1"[^>]*>1<\/span>/);

  const remainingRows = familyRows().filter(s => s.id !== 'c-new' && s.id !== 'c-old');
  context.renderSessionList(remainingRows, 'all');
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'r1'), /data-tree-child-count/);
  assert.match(anchorOpenTag(nav.innerHTML, 'r1') + rowHtml(nav.innerHTML, 'r1'),
      /data-tree-toggle="r1"[^>]*title="2 child tasks"/);
});

test('the Threads paint shows the count like the Workspace paint', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'threads');

  assert.match(rowHtml(nav.innerHTML, 'r1'), /data-tree-child-count="r1"[^>]*>2<\/span>/);
});

test('Later, Archive and the flat search paint draw no count element', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const starred = familyRows().map(s => ({...s, starred: true}));

  context.renderSessionList(starred, 'starred');
  assert.doesNotMatch(nav.innerHTML, /data-tree-child-count/);

  context.renderSessionList(familyRows(), 'archived');
  assert.doesNotMatch(nav.innerHTML, /data-tree-child-count/);

  context.renderSessionList(familyRows(), 'search');
  assert.doesNotMatch(nav.innerHTML, /data-tree-child-count/);
});

test('the count title names a single direct manager child or multiple children', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('r1', null, 10), manager('c1', 'r1', 11)], 'all');
  assert.match(rowHtml(nav.innerHTML, 'r1'), /title="1 active child session"/);

  context.renderSessionList(familyRows(), 'all');
  assert.match(rowHtml(nav.innerHTML, 'r1'), /title="2 active child sessions"/);
});

test('the five-row preview counts root rows only and hides a capped root with its subtree', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
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
  assert.match(html, /<span class="text-xs text-slate-400 ml-auto">10<\/span>/);
});

test('four roots with many children stay under the preview cap', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [];
  for (let i = 1; i <= 4; i++) rows.push(manager(`work-${i}`, null, 20 - i));
  for (let i = 1; i <= 8; i++) rows.push(worker(`w1-${i}`, 'work-1', i));

  context.renderSessionList(rows, 'all');

  assert.doesNotMatch(nav.innerHTML, /Show all/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra/);
});

test('cron roots show in place without ranks and only the non-cron roots past rank 5 hide', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [
    boundNode('cron-1', 20, {group: 'Work'}),
    manager('work-2', null, 19),
    boundNode('cron-3', 18, {group: 'Work'}),
    manager('cron-child', 'cron-3', 17),
    manager('work-4', null, 16),
    manager('work-5', null, 15),
    manager('work-6', null, 14),
    manager('work-7', null, 13),
    manager('work-8', null, 12),
    manager('work-8-child', 'work-8', 11),
  ];

  context.renderSessionList(rows, 'all');

  let html = nav.innerHTML;
  assert.match(html, />Show all<\/button>/);
  // Every cron root shows in its place, exempt from the limit.
  for (const id of ['cron-1', 'cron-3']) {
    assert.doesNotMatch(anchorOpenTag(html, id), /session-group-limit-extra/);
  }
  // The first five non-cron roots take the ranks; the sixth hides with its subtree.
  for (const id of ['work-2', 'work-4', 'work-5', 'work-6', 'work-7']) {
    assert.doesNotMatch(anchorOpenTag(html, id), /session-group-limit-extra/);
  }
  assert.match(anchorOpenTag(html, 'work-8'), /session-group-limit-extra hidden/);
  assert.match(html, /<div class="tree-subtree session-group-limit-extra hidden[^"]*" data-session-group-limit-extra="Work">/);

  context.toggleSessionGroupLimit('Work');
  context.renderSessionList(rows, 'all');

  html = nav.innerHTML;
  // After Show all every root shows: the capped root's row and subtree lose
  // the hidden class, and the exempt roots still carry no limit class.
  for (const id of ['cron-1', 'cron-3', 'work-2', 'work-4', 'work-5', 'work-6', 'work-7', 'work-8']) {
    assert.ok(anchorOpenTag(html, id), id);
  }
  assert.match(anchorOpenTag(html, 'work-8'), /session-group-limit-extra(?! hidden)/);
  assert.match(html, /<div class="tree-subtree session-group-limit-extra(?! hidden)[^"]*" data-session-group-limit-extra="Work">/);
  for (const id of ['cron-1', 'cron-3']) {
    assert.doesNotMatch(anchorOpenTag(html, id), /session-group-limit-extra/);
  }
  assert.match(html, />Show less<\/button>/);
  assert.match(html, /id="session-cron-child"/);
});

test('five non-cron roots and two cron roots show all seven roots with no Show all', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [
    boundNode('cron-1', 20, {group: 'Work'}),
    manager('work-2', null, 19),
    manager('work-3', null, 18),
    manager('work-4', null, 17),
    manager('work-5', null, 16),
    manager('work-6', null, 15),
    boundNode('cron-7', 14, {group: 'Work'}),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(html, /Show all/);
  assert.doesNotMatch(html, /session-group-limit-extra/);
  for (const id of ['cron-1', 'work-2', 'work-3', 'work-4', 'work-5', 'work-6', 'cron-7']) {
    assert.ok(anchorOpenTag(html, id), id);
  }
});

test('a group of only cron roots shows every root with no Show all', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [
    boundNode('cron-1', 10, {group: 'Cron only'}),
    boundNode('cron-2', 9, {group: 'Cron only'}),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(html, /Show all/);
  assert.doesNotMatch(html, /session-group-limit-extra/);
  assert.ok(anchorOpenTag(html, 'cron-1'));
  assert.ok(anchorOpenTag(html, 'cron-2'));
});

test('an open cron root draws no Show all when no other root sits past rank 5', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  context.SESSION_ID = 'cron-1';
  const rows = [
    boundNode('cron-1', 20, {group: 'Work'}),
    manager('work-2', null, 19),
    manager('work-3', null, 18),
    manager('work-4', null, 17),
    manager('work-5', null, 16),
    manager('work-6', null, 15),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(html, /Show all/);
  assert.doesNotMatch(anchorOpenTag(html, 'cron-1'), /session-group-limit-extra/);
  for (const id of ['work-2', 'work-3', 'work-4', 'work-5', 'work-6']) {
    assert.doesNotMatch(anchorOpenTag(html, id), /session-group-limit-extra/);
  }
});

test('an open non-cron root past rank 5 shows and draws no Show all when it is the only one there', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  context.SESSION_ID = 'work-6';
  const rows = [
    manager('work-1', null, 20),
    manager('work-2', null, 19),
    manager('work-3', null, 18),
    manager('work-4', null, 17),
    manager('work-5', null, 16),
    manager('work-6', null, 15),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(html, /Show all/);
  assert.doesNotMatch(anchorOpenTag(html, 'work-6'), /session-group-limit-extra/);
  assert.doesNotMatch(anchorOpenTag(html, 'work-5'), /session-group-limit-extra/);
});

test('an open child keeps its non-cron root visible past rank 5', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  context.SESSION_ID = 'child-6';
  const rows = [
    manager('work-1', null, 20),
    manager('work-2', null, 19),
    manager('work-3', null, 18),
    manager('work-4', null, 17),
    manager('work-5', null, 16),
    manager('work-6', null, 15),
    manager('child-6', 'work-6', 14),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(anchorOpenTag(html, 'work-6'), /session-group-limit-extra/);
  assert.doesNotMatch(anchorOpenTag(html, 'work-5'), /session-group-limit-extra/);
});

test('an open child keeps its cron root visible and opens the path to that child', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  context.SESSION_ID = 'cron-child';
  const rows = [
    boundNode('cron-root', 10, {group: 'Work'}),
    manager('cron-child', 'cron-root', 9),
    manager('work-1', null, 8),
  ];

  context.renderSessionList(rows, 'all');

  const html = nav.innerHTML;
  assert.doesNotMatch(anchorOpenTag(html, 'cron-root'), /session-group-limit-extra/);
  assert.doesNotMatch(html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="cron-root">/)[0], /hidden/);
  assert.match(anchorOpenTag(html, 'cron-child'), /bg-blue-600\/20 text-blue-300/);
});

test('the cron registration exempts bound roots from the limit and keeps unbound roots ranked', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [
    boundNode('cron-root', 10, {group: 'Work'}),
    manager('unbound-root', null, 9, {schedule_task: null, schedule_cron: '0 9 * * *'}),
  ];

  context.renderSessionList(rows, 'all');

  assert.doesNotMatch(anchorOpenTag(nav.innerHTML, 'cron-root'), /session-group-limit-extra/);
  assert.doesNotMatch(anchorOpenTag(nav.innerHTML, 'unbound-root'), /session-group-limit-extra/);
  assert.doesNotMatch(nav.innerHTML, /Show all/);
});

test('group preview limit exemption registrations validate and reject duplicate predicates', () => {
  const {context} = buildSidebarIndicatorContext([]);
  const exemptsNothing = () => false;

  assert.throws(() => context.registerGroupPreviewLimitExemption(null));
  context.registerGroupPreviewLimitExemption(exemptsNothing);
  assert.throws(() => context.registerGroupPreviewLimitExemption(exemptsNothing));
});

test('toggleTreeNode flips the in-memory state, the rendered subtree and the chevron', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const container = createElement({className: 'tree-children hidden'});
  const chevron = createElement({className: 'tree-chevron'});
  context.document.querySelectorAll = (selector) => {
    if (selector === '[data-tree-children="r1"]') return [container];
    if (selector === '[data-tree-toggle="r1"]') return [chevron];
    return [];
  };

  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), false);
  context.toggleTreeNode('r1');

  assert.equal(context.Sidebar.isTreeNodeExpanded('r1'), true);
  assert.equal(container.classList.contains('hidden'), false);
  assert.equal(chevron.classList.contains('rotate-90'), true);
  assert.equal(chevron['aria-expanded'], 'true');

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
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11)], 'all');

  const leaf = rowHtml(nav.innerHTML, 'w-new');
  assert.match(leaf, /Worker \(implementation leaf\)/);
  assert.match(leaf, /title="Archive"/);
  assert.doesNotMatch(leaf, /star-btn|title="Rename"|title="Set group"|title="Settings"|title="Edit task config"/);
  const parent = rowHtml(nav.innerHTML, 'r1');
  assert.doesNotMatch(parent, /Worker \(implementation leaf\)/);
  assert.match(parent, /star-btn/);
  assert.match(parent, /title="Settings"/);
});

test('search results stay flat: no nesting, no chevron', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(familyRows(), 'search');

  assert.deepEqual(anchorIdsInOrder(nav.innerHTML), ['r1', 'w-old', 'c-old', 'g1', 'w-new', 'c-new']);
  assert.doesNotMatch(nav.innerHTML, /data-tree-children|data-tree-toggle/);
});

test('a logical row offers New child session; a worker leaf does not', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11), meta('legacy')], 'all');

  assert.match(rowHtml(nav.innerHTML, 'r1'), /title="New child session"[\s\S]*?sidebarControl\('createChildSession', 'r1'\)|sidebarControl\('createChildSession', 'r1'\)[\s\S]*?title="New child session"/);
  assert.match(rowHtml(nav.innerHTML, 'legacy'), /sidebarControl\('createChildSession', 'legacy'\)/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'w-new'), /New child session|createChildSession/);
});

test('a normal row\u2019s direct buttons end in Settings carrying the menu facts; a worker leaf has none', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([manager('r1', null, 10), worker('w-new', 'r1', 11), legacy('old1', 9)], 'all');

  const r1 = rowHtml(nav.innerHTML, 'r1');
  const at = (mark) => r1.indexOf(mark);
  assert.ok(at('title="Star"') > -1 && at('title="Star"') < at('title="New child session"')
    && at('title="New child session"') < at('title="Archive"') && at('title="Archive"') < at('title="Settings"'),
    `the four direct buttons render in order: ${r1}`);
  assert.match(r1, /sidebarControl\('openSessionRowMenu', this, 'r1'\)/);
  // The facts openSessionRowMenu builds the items from.
  assert.match(r1, /data-current-group="Work"/);
  assert.match(r1, /data-task-parent=""/);
  assert.match(r1, /data-profile="manager"/);
  assert.match(r1, /data-schedule-task=""/);
  // A legacy row's Settings carries the empty profile; a worker leaf keeps
  // the archive action alone.
  assert.match(rowHtml(nav.innerHTML, 'old1'), /data-profile=""/);
  assert.doesNotMatch(rowHtml(nav.innerHTML, 'w-new'), /title="Settings"|openSessionRowMenu/);
});

test('openSessionRowMenu builds the item list from the button\u2019s data attributes', () => {
  const {context} = buildSidebarIndicatorContext([]);
  // The touch branch keys off (hover: none) at open time; the desktop list is
  // asserted with the media query not matching.
  context.window.matchMedia = (query) => ({matches: false, media: query});
  const calls = [];
  const click = {preventDefault() {}, stopPropagation() {}};
  context.openTaskContextModal = (...args) => calls.push(['taskContext', ...args]);
  context.startRename = (...args) => calls.push(['rename', ...args]);
  let items = null;
  context.openRowMenu = (anchor, built) => { items = built; };
  const open = (dataset, id) => {
    context.Sidebar.openSessionRowMenu(createElement({tagName: 'BUTTON', dataset}), id);
    // Spread into an outer-realm array: a vm array carries the vm's
    // Array.prototype and deepStrictEqual rejects the cross-realm twin.
    return [...items].map((item) => item.label);
  };

  // A root manager, unbound: all four items, each core item's onSelect wired
  // to its old direct button's call. (Move to group opens the group selector
  // modal, outside this harness's reach; the browser harness covers it.
  // cron_broken_ui.test.js drives the schedule items.)
  assert.deepEqual(open({currentGroup: 'Work', profile: 'manager'}, 'r1'),
    ['Rename', 'Move to group\u2026', 'Task & context', 'Add schedule\u2026']);
  items[0].onSelect(click);
  items[2].onSelect(click);
  assert.deepEqual(calls, [
    ['rename', click, 'r1'],
    ['taskContext', 'r1'],
  ]);
  assert.equal(calls[0][1], click, 'rename takes the item\u2019s click event');

  // A child manager moves with its parent: no Move to group.
  assert.deepEqual(open({taskParent: 'r1', profile: 'manager'}, 'c1'),
    ['Rename', 'Task & context', 'Add schedule\u2026']);
  calls.length = 0;
  items[1].onSelect(click);
  assert.deepEqual(calls, [['taskContext', 'c1']]);

  // A legacy row (profile null, unbound): the group move stays (it has no
  // task-tree parent), but no Task & context and no schedule item.
  assert.deepEqual(open({}, 'old1'), ['Rename', 'Move to group\u2026']);

  // A bound row edits its own task instead of adding one.
  assert.deepEqual(open({scheduleTask: 'task-cron1'}, 'cron1'),
    ['Rename', 'Move to group\u2026', 'Edit schedule\u2026']);
});

test('touch menus lead with the Later toggle and child creation and trail with Archive', () => {
  const {context} = buildSidebarIndicatorContext([]);
  context.window.matchMedia = (query) => ({matches: true, media: query});
  // The starred state is read off the row's star button (filters.js's
  // in-place repaint flips text-yellow-400); only the starred row's id
  // resolves here.
  context.document.getElementById = (id) =>
    id === 'star-r1' ? createElement({className: 'star-btn text-yellow-400 !opacity-100'}) : null;
  const calls = [];
  const click = {preventDefault() {}, stopPropagation() {}};
  context.toggleSessionStar = (...args) => calls.push(['star', ...args]);
  context.createChildSession = (...args) => calls.push(['child', ...args]);
  context.archiveSession = (...args) => calls.push(['archive', ...args]);
  let items = null;
  context.openRowMenu = (anchor, built) => { items = built; };
  const open = (dataset, id) => {
    context.Sidebar.openSessionRowMenu(createElement({tagName: 'BUTTON', dataset}), id);
    // Separators render as dashes so the label order reads in one list.
    return [...items].map((item) => item.separator ? '---' : item.label);
  };

  // A root manager: the two touch-only leads, the desktop items in their
  // order, then the separator and Archive.
  assert.deepEqual(open({currentGroup: 'Work', profile: 'manager'}, 'm1'),
    ['Add to Later', 'New child session', 'Rename', 'Move to group\u2026', 'Task & context',
     'Add schedule\u2026', '---', 'Archive']);
  items[0].onSelect(click);
  items[1].onSelect(click);
  items[items.length - 1].onSelect(click);
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [
    ['star', 'm1', false],
    ['child', 'm1'],
    ['archive', 'm1'],
  ]);

  // A task-tree child keeps the desktop order minus Move to group.
  assert.deepEqual(open({taskParent: 'r1', profile: 'manager'}, 'c1'),
    ['Add to Later', 'New child session', 'Rename', 'Task & context', 'Add schedule\u2026', '---', 'Archive']);

  // A starred row reads its state off the star button (only star-r1 resolves
  // to the repainted solid star): Remove from Later toggles away from
  // starred.
  assert.deepEqual(open({currentGroup: 'Work', profile: 'manager'}, 'r1').slice(0, 1),
    ['Remove from Later']);
  items[0].onSelect(click);
  assert.deepEqual(calls.at(-1), ['star', 'r1', true]);
});

test('the active session’s ancestors open once per switch and a manual collapse then holds', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
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
  const {context} = buildSidebarIndicatorContext([]);
  const container = createElement({className: 'tree-children hidden'});
  const chevron = createElement({className: 'tree-chevron'});
  context.document.querySelectorAll = (selector) => {
    if (selector === '[data-tree-children="p1"]') return [container];
    if (selector === '[data-tree-toggle="p1"]') return [chevron];
    return [];
  };

  context.Sidebar.expandTreeNode('p1');
  assert.equal(context.Sidebar.isTreeNodeExpanded('p1'), true);
  assert.equal(container.classList.contains('hidden'), false);
  assert.equal(chevron.classList.contains('rotate-90'), true);

  context.Sidebar.expandTreeNode('p1');
  assert.equal(context.Sidebar.isTreeNodeExpanded('p1'), true, 'an already open node is left alone');
  assert.equal(chevron.classList.contains('rotate-90'), true);
});

test('an archived worker row shows the delivered check in place of the leaf icon', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
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

test('the worker sessions of a legacy session nest under it, collapsed by default', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([
    legacy('old1', 10),
    worker('t1', 'old1', 9),
    worker('t2', 'old1', 8),
  ], 'all');

  const html = nav.innerHTML;
  const container = html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="old1">/);
  assert.ok(container, 'the legacy root row is followed by its children container');
  assert.match(container[0], / hidden"/);
  assert.ok(html.indexOf('data-tree-children="old1"') < html.indexOf('id="session-t1"'));
  assert.match(anchorOpenTag(html, 'old1') + rowHtml(html, 'old1'),
      /data-tree-toggle="old1"[^>]*aria-expanded="false"/);
  // The leaves carry the worker glyph and nest in listed order.
  assert.match(rowHtml(html, 't1'), /title="Worker \(implementation leaf\)"/);
  assert.match(rowHtml(html, 't2'), /title="Worker \(implementation leaf\)"/);
  assert.deepEqual(anchorIdsInOrder(html), ['old1', 't1', 't2']);
});

test('a legacy session row keeps its actions but shows no Task & context button', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([legacy('old1', 10), manager('mgr', null, 9)], 'all');

  const legacyRow = rowHtml(nav.innerHTML, 'old1');
  assert.match(legacyRow, /star-btn/);
  assert.match(legacyRow, /title="New child session"/);
  assert.match(legacyRow, /data-profile=""/);
  assert.doesNotMatch(legacyRow, /openTaskContextModal/);
  const managerRow = rowHtml(nav.innerHTML, 'mgr');
  assert.match(managerRow, /data-profile="manager"/);
});

// ---------------------------------------------------------------------------
// A bound node in the Workspace tree: the same tree the All tab builds, with
// the bound row form — schedule lines, clock badge, Settings gear — and
// its firing leaves nested collapsed under it.
// ---------------------------------------------------------------------------

test('a bound node nests its firing leaves under it, collapsed, and keeps the bound row form', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([
    boundNode('cron1', 10),
    worker('t1', 'cron1', 9),
    worker('t2', 'cron1', 8),
    worker('t3', 'cron1', 7),
  ], 'all');

  const html = nav.innerHTML;
  // The three leaves sit inside the bound node's subtree container, collapsed.
  const container = html.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="cron1">/);
  assert.ok(container, 'the bound row is followed by its children container');
  assert.match(container[0], / hidden"/);
  assert.ok(html.indexOf('data-tree-children="cron1"') < html.indexOf('id="session-t1"'));
  // The bound row keeps its form: schedule line, chevron with the count, and
  // the clock badge naming its task.
  assert.match(rowHtml(html, 'cron1'), /0 9 \* \* \*/);
  assert.match(rowHtml(html, 'cron1'), /Next: /);
  assert.match(anchorOpenTag(html, 'cron1') + rowHtml(html, 'cron1'),
      /data-tree-toggle="cron1"[^>]*aria-expanded="false"/);
  assert.match(rowHtml(html, 'cron1'), /Scheduled: task-cron1/);
  assert.match(rowHtml(html, 'cron1'), /title="Settings"/);
  assert.match(rowHtml(html, 'cron1'), /data-schedule-task="task-cron1"/);
  // A worker leaf carries the archive action alone: no star, rename or
  // schedule button, and its click switches sessions.
  for (const t of ['t1', 't2', 't3']) {
    const row = rowHtml(html, t);
    assert.doesNotMatch(row, /star-btn|title="Rename"|title="Edit schedule"|title="Add schedule"/);
    assert.match(row, /title="Archive"/);
    assert.match(anchorOpenTag(html, t), /sidebarControl\('switchSession', 't/);
  }
  // The group header counts every rendered row, roots and descendants.
  assert.match(html, /<span class="text-xs text-slate-400 ml-auto">4<\/span>/);
  assert.deepEqual(anchorIdsInOrder(html), ['cron1', 't1', 't2', 't3']);
  // The grouped render recorded the tree maps itself.
  assert.deepEqual([...context.Sidebar.treeChildIds('cron1')], ['t1', 't2', 't3']);
  assert.equal(context.Sidebar.treeParentId('t1'), 'cron1');
});

test('a bound node’s clock and lines follow the task state: disabled goes grey and stops naming a next run', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([
    boundNode('cron1', 10),
    boundNode('cron2', 9, {schedule_enabled: false}),
  ], 'all');

  const html = nav.innerHTML;
  assert.match(rowHtml(html, 'cron1'), /text-blue-400[^>]*viewBox="0 0 24 24" title="Scheduled: task-cron1"/s);
  assert.match(rowHtml(html, 'cron1'), /Next: /);
  assert.match(rowHtml(html, 'cron2'), /text-slate-500[^>]*viewBox="0 0 24 24" title="Scheduled: task-cron2"/s);
  assert.match(rowHtml(html, 'cron2'), /Disabled/);
  assert.doesNotMatch(rowHtml(html, 'cron2'), /Next: /);
});

test('a bound row\u2019s Settings names its schedule task; an unbound manager row names none', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList([boundNode('cron1', 10), manager('mgr', null, 9)], 'all');

  assert.match(rowHtml(nav.innerHTML, 'cron1'), /data-schedule-task="task-cron1"/);
  assert.match(rowHtml(nav.innerHTML, 'mgr'), /data-schedule-task=""/);
  assert.match(rowHtml(nav.innerHTML, 'mgr'), /data-profile="manager"/);
});

test('a grouped paint records the tree maps, refreshes tree indicators, and keeps expand state across repaints', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = [boundNode('cron1', 10), worker('t1', 'cron1', 9)];
  let indicatorRefreshes = 0;
  context.Sidebar.refreshTreeIndicators = () => { indicatorRefreshes += 1; };

  context.renderSessionList(rows, 'all');

  assert.equal(indicatorRefreshes, 1, 'the collapsed parent takes its leaves\u2019 stand-in state');
  assert.equal(context.Sidebar.treeParentId('t1'), 'cron1');

  // Expanding here keeps the subtree open on the next repaint.
  context.toggleTreeNode('cron1');
  context.renderSessionList(rows, 'all');
  assert.doesNotMatch(nav.innerHTML.match(/<div class="tree-children[^"]*"[^>]*data-tree-children="cron1">/)[0], /hidden/);
});

test('a collapsed bound row\u2019s gear and subtree mark survive expand and collapse', () => {
  const {context, shown} = buildSidebarIndicatorContext(['cron1', 'cron2', 'leaf1', 'leaf2']);
  context.renderSessionList([
    boundNode('cron1', 10),
    worker('leaf1', 'cron1', 9, {has_running_tasks: true}),
    boundNode('cron2', 8),
    worker('leaf2', 'cron2', 7, {has_unread: true}),
  ], 'all');
  context.setSessionIndicator('cron1', 'idle');
  context.setSessionIndicator('cron2', 'idle');

  const gearRow = shown('cron1');
  assert.equal(gearRow.gear, true, 'the running leaf lights the parent row\u2019s gear');
  const markRow = shown('cron2');
  assert.equal(markRow.subtreeMark, true, 'the unread leaf lights the parent row\u2019s subtree mark');

  context.Sidebar.expandTreeNode('cron1');
  context.Sidebar.expandTreeNode('cron2');
  assert.deepEqual(shown('cron1'), gearRow, 'expansion never changes the row\u2019s icon');
  assert.deepEqual(shown('cron2'), markRow, 'expansion never changes the row\u2019s icon');
  context.Sidebar.toggleTreeNode('cron1');
  context.Sidebar.toggleTreeNode('cron2');
  assert.deepEqual(shown('cron1'), gearRow, 'collapse never changes the row\u2019s icon');
  assert.deepEqual(shown('cron2'), markRow, 'collapse never changes the row\u2019s icon');
});

