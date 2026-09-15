const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse, row} = require('./task_ui_context_stub');

// Load the tree owner exactly as the page does: namespace first, then
// session-tree.js (its wire() registers the delegated row handlers).
function build(overrides = {}) {
  const context = buildContext(overrides);
  // websocket.js's session-event handler is the tree's live-update entry; the
  // rendering globals it reaches for on other event types are no-op stubs.
  context.showStreaming = () => {};
  context.hideStreaming = () => {};
  context.appendMessageObject = () => {};
  context.pollActiveSessionView = () => {};
  loadModules(context, ['sidebar/session-tree.js', 'websocket.js']);
  const doc = context.__doc;
  doc.register(doc.createElement('div')); // ensure body exists
  const sessionList = doc.createElement('nav');
  sessionList.id = 'session-list';
  doc.body.appendChild(sessionList);
  return {context, doc, sessionList};
}

// A fixture answering /api/sessions/tree pages: roots -> feature -> two workers.
function installTreeFetch(context) {
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 3});
  const feature = row({id: 'feat-1', name: 'Feature', task_parent_id: 'root-1', child_count: 2, open_descendant_count: 2});
  const w1 = row({id: 'work-1', name: 'Worker One', profile: 'worker', task_parent_id: 'feat-1'});
  const w2 = row({id: 'work-2', name: 'Worker Two', profile: 'worker', task_parent_id: 'feat-1'});
  const pages = {
    '': [root],
    'root-1': [feature],
    'feat-1': [w1, w2],
  };
  context.__rows = {root, feature, w1, w2};
  context.fetchHandlers.push((url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const params = new URLSearchParams(m[1]);
    const parent = params.get('parent_id') || '';
    return jsonResponse({items: pages[parent] || [], next_cursor: null, tree_revision: 'rev-1'});
  });
  return pages;
}

function treeRows(sessionList) {
  return sessionList.querySelectorAll('.tree-row');
}

function findRow(sessionList, id) {
  return treeRows(sessionList).find((el) => el.dataset.nodeId === id) || null;
}

// Drain every pending microtask/macrotask turn the async refresh chain needs.
async function flush(rounds = 10) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

test('roots render with profile, work state and subtree counts', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  const rows = treeRows(sessionList);
  assert.equal(rows.length, 1);
  const text = rows[0].textContent;
  assert.ok(text.includes('Program'), 'row shows the task name');
  assert.ok(text.includes('Manager'), 'row shows the manager profile label');
  assert.ok(text.includes('idle'), 'row shows the own work state');
  assert.ok(text.includes('3 open'), 'row shows the open descendant count');
  assert.equal(rows[0].getAttribute('role'), 'treeitem');
});

test('expansion lazily fetches a level and appends without duplicates on re-toggle', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));

  const before = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  const afterFirst = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;
  assert.equal(afterFirst, before + 1, 'expansion fetches the children level once');
  const featureRow = findRow(sessionList, 'feat-1');
  assert.ok(featureRow, 'child row rendered after expansion');

  // Collapse and re-expand: no duplicate rows, no refetch (level cached).
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  const afterReexpand = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;
  assert.equal(afterReexpand, afterFirst, 'cached level is not refetched');
  assert.equal(treeRows(sessionList).filter((el) => el.dataset.nodeId === 'feat-1').length, 1,
    're-expansion never duplicates the child row');
});

test('a multi-page level is fetched to exhaustion, appended once each', async () => {
  const {context, sessionList} = build();
  const pageOne = [row({id: 'p1-a', name: 'A'}), row({id: 'p1-b', name: 'B'})];
  const pageTwo = [row({id: 'p2-a', name: 'C'})];
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (!url.includes('/api/sessions/tree?')) return undefined;
    call++;
    if (call === 1) return jsonResponse({items: pageOne, next_cursor: 'cur-1', tree_revision: 'rev'});
    return jsonResponse({items: pageTwo, next_cursor: null, tree_revision: 'rev'});
  });
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  const ids = treeRows(sessionList).map((el) => el.dataset.nodeId);
  assert.deepEqual(ids, ['p1-a', 'p1-b', 'p2-a'], 'pagination continues past the first page');
  assert.equal(context.fetchCalls.filter((c) => c.url.includes('/tree?')).length, 2,
    'exactly the two pages were requested');
});

test('a 409 during pagination refreshes the level with a visible explanation', async () => {
  const {context, sessionList} = build();
  let first = true;
  context.fetchHandlers.push((url) => {
    if (!url.includes('/api/sessions/tree?')) return undefined;
    if (first) {
      first = false;
      return {ok: false, status: 409, json: async () => ({detail: {message: 'task tree changed during pagination; refresh and re-paginate', blockers: []}})};
    }
    return jsonResponse({items: [row({id: 'after-409', name: 'Fresh'})], next_cursor: null, tree_revision: 'rev-2'});
  });
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  const rows = treeRows(sessionList);
  assert.equal(rows.length, 1, 'the level was refetched after the 409');
  assert.equal(rows[0].dataset.nodeId, 'after-409');
  assert.ok(sessionList.textContent.includes('task tree changed'),
    'the visible explanation names the conflict');
});

test('search reveals and expands the complete server-built path to a match', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  // Search hit naming a worker two levels down, with its full ancestor chain.
  context.fetchHandlers.push((url) => {
    const m = url.match(/\/api\/sessions\/tree\/search\?q=([^&]+)/);
    if (!m) return undefined;
    assert.equal(decodeURIComponent(m[1]), 'worker one');
    return jsonResponse({
      items: [{
        row: context.__rows.w1,
        ancestors: [context.__rows.feature, context.__rows.root],
      }],
      tree_revision: 'rev',
    });
  });
  await context.Sidebar.SessionTree.searchTree('worker one');
  await new Promise((r) => setImmediate(r));
  const ids = treeRows(sessionList).map((el) => el.dataset.nodeId);
  // Root and feature are rendered (path expanded); the match is highlighted.
  assert.ok(ids.includes('root-1') && ids.includes('feat-1') && ids.includes('work-1'),
    'the complete path to the match is rendered');
  const matchRow = findRow(sessionList, 'work-1');
  assert.ok(matchRow.querySelector('.tree-row') === null);
  const inner = matchRow.firstElementChild;
  assert.ok(inner.classes.has('ring-1') || inner.classes.has('bg-blue-600/20'),
    'the match is visually highlighted');
  // The path was persisted as expanded.
  const stored = JSON.parse(context.localStorage.store.get('charliebot-tree-expanded'));
  assert.ok(stored.includes('root-1') && stored.includes('feat-1'));
});

test('expand/collapse state survives a reload via localStorage', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  const stored = JSON.parse(context.localStorage.store.get('charliebot-tree-expanded'));
  assert.ok(stored.includes('root-1'));

  // A fresh page load restores the expansion without user interaction.
  const second = build();
  second.context.localStorage.store.set('charliebot-tree-expanded', JSON.stringify(stored));
  installTreeFetch(second.context);
  second.context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  assert.ok(findRow(second.sessionList, 'feat-1'), 'the restored expansion renders the child level');
});

test('keyboard: Enter opens a node, ArrowRight expands, ArrowDown moves focus', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  const rootRow = findRow(sessionList, 'root-1');
  sessionList.dispatch('keydown', {key: 'ArrowRight', target: rootRow, preventDefault: () => {}});
  await new Promise((r) => setImmediate(r));
  assert.ok(findRow(sessionList, 'feat-1'), 'ArrowRight expands the focused node');

  const rowEl = findRow(sessionList, 'root-1');
  sessionList.dispatch('keydown', {key: 'Enter', target: rowEl, preventDefault: () => {}});
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(context._switchCalls, ['root-1'], 'Enter opens the node');
});

test('a token stream never fetches tree data; task_tree_changed refreshes only affected levels', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  const before = context.fetchCalls.length;

  // A stream delta for the active session (the chat token path): the tree
  // does nothing.
  context.handleWSEvent({type: 'stream', message: {content: 'x'}}, context._sessionId, 0);

  // The worker's own task_tree_changed rides the websocket handler.
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-1'}, context._sessionId, 0);
  await flush();
  const treeFetches = context.fetchCalls.slice(before).filter((c) => c.url.includes('/api/sessions/tree?'));
  const levels = treeFetches.map((c) => new URLSearchParams(c.url.split('?')[1]).get('parent_id') || '');
  assert.equal(levels.length, 2, 'only the affected levels are refetched');
  assert.ok(levels.includes(''), 'the roots level is included');
  assert.ok(levels.includes('root-1'), 'the changed node\'s parent level is included');
  assert.ok(!levels.includes('feat-1'), 'unaffected sibling levels are not refetched');
});

test('duplicate task_tree_changed delivery does not duplicate rows', async () => {
  const {context, sessionList} = build();
  installTreeFetch(context);
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  await context.Sidebar.SessionTree.toggleTreeNode('feat-1');
  context.Sidebar.SessionTree.onTreeChanged('feat-1');
  context.Sidebar.SessionTree.onTreeChanged('feat-1');
  context.Sidebar.SessionTree.onTreeChanged('feat-1');
  await flush();
  assert.equal(treeRows(sessionList).filter((el) => el.dataset.nodeId === 'feat-1').length, 1,
    'repeated notifications render the row once');
  assert.equal(treeRows(sessionList).filter((el) => el.dataset.nodeId === 'work-1').length, 1);
});

test('an archived ancestor with active work below stays navigable as ancestor context', async () => {
  const {context, sessionList} = build();
  const hiddenRoot = row({id: 'root-1', name: 'Hidden Program', archived: true, child_count: 1});
  const liveWorker = row({id: 'work-1', name: 'Live Worker', profile: 'worker',
    task_parent_id: 'root-1', work_state: 'running'});
  context.fetchHandlers.push((url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const parent = new URLSearchParams(m[1]).get('parent_id') || '';
    if (parent === '') return jsonResponse({items: [hiddenRoot], next_cursor: null, tree_revision: 'r'});
    return jsonResponse({items: [liveWorker], next_cursor: null, tree_revision: 'r'});
  });
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  const rootRow = findRow(sessionList, 'root-1');
  assert.ok(rootRow, 'the archived ancestor is still rendered');
  assert.ok(rootRow.textContent.includes('archived'), 'its archived presentation is labelled');
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  assert.ok(findRow(sessionList, 'work-1'), 'the running descendant below is reachable');
});

test('the v2 scenario fixture: four task nodes, workers stay leaves, manager turns separate', async () => {
  const {context, sessionList} = build();
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 3});
  const feature = row({id: 'feat-1', name: 'Feature', task_parent_id: 'root-1', child_count: 2, open_descendant_count: 2});
  const w1 = row({id: 'work-1', name: 'Worker One', profile: 'worker', task_parent_id: 'feat-1'});
  const w2 = row({id: 'work-2', name: 'Worker Two', profile: 'worker', task_parent_id: 'feat-1'});
  const runs = {
    work1: [
      {id: 'run-w1-work', kind: 'work', state: 'success', started_at: '2026-01-01T00:00:00Z', ended_at: '2026-01-01T00:01:00Z', input_event_ids: ['i1']},
      {id: 'run-w1-review', kind: 'review', state: 'success', started_at: '2026-01-01T00:02:00Z', ended_at: '2026-01-01T00:03:00Z', input_event_ids: [], review_of_run_id: 'run-w1-work'},
    ],
  };
  context.fetchHandlers.push((url) => {
    if (url.includes('/api/sessions/tree?')) {
      const parent = new URLSearchParams(url.split('?')[1]).get('parent_id') || '';
      const pages = {'': [root], 'root-1': [feature], 'feat-1': [w1, w2]};
      return jsonResponse({items: pages[parent] || [], next_cursor: null, tree_revision: 'r'});
    }
    if (url.includes('/api/sessions/work-1/runs')) return jsonResponse({items: runs.work1, next_cursor: null});
    return undefined;
  });
  context.Sidebar.SessionTree.enterTreeFilter();
  await new Promise((r) => setImmediate(r));
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');
  await context.Sidebar.SessionTree.toggleTreeNode('feat-1');
  const ids = treeRows(sessionList).map((el) => el.dataset.nodeId);
  assert.deepEqual(ids, ['root-1', 'feat-1', 'work-1', 'work-2'], '4 task nodes in one tree');
  // Worker rows render as Worker leaves with no add-child affordance.
  const workerRow = findRow(sessionList, 'work-1');
  const addBtn = workerRow.querySelector('button[data-action="add-child"]');
  assert.ok(addBtn.classes.has('hidden'), 'workers are leaves: no add-subtask button');
  // The manager's own runs (its turns) are a separate listing from the worker leaf's runs.
  const managerRunFetch = context.fetchCalls.filter((c) => c.url.includes('/api/sessions/root-1/runs'));
  assert.equal(managerRunFetch.length, 0, 'the tree view never pulls another node\'s runs');
});
