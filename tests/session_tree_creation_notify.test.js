const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse, row} = require('./task_ui_context_stub');

// Load the tree owner exactly as the page does, plus websocket.js so the
// notification path under test is the real event entry point.
function build(overrides = {}) {
  const context = buildContext(overrides);
  context.showStreaming = () => {};
  context.hideStreaming = () => {};
  context.appendMessageObject = () => {};
  context.pollActiveSessionView = () => {};
  // Page order: status.js (the indicator owner the tree shares) precedes the tree owner.
  loadModules(context, ['sidebar/status.js', 'sidebar/session-tree.js', 'websocket.js']);
  const doc = context.__doc;
  const sessionList = doc.createElement('nav');
  sessionList.id = 'session-list';
  doc.body.appendChild(sessionList);
  return {context, doc, sessionList};
}

async function flush(rounds = 12) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

// A mutable synthetic server: tree pages per level, node rows, and the detail
// reads the unknown-node path uses. createNode mutates the served facts the
// way the backend's own publication does.
function serverState() {
  const state = {
    levels: {},     // parentId ('' roots) -> [row, ...]
    detailCache: {},// id -> {ancestors: [...]}
  };
  state.putLevel = (parent, rows) => { state.levels[parent || ''] = rows; };
  state.registerDetail = (r, chainIds) => {
    state.detailCache[r.id] = {ancestors: chainIds.map((id) => ({id, name: 'anc-' + id}))};
  };
  state.treeHandler = (context) => (url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const parent = new URLSearchParams(m[1]).get('parent_id') || '';
    const rows = state.levels[parent] || [];
    return jsonResponse({items: rows, next_cursor: null, tree_revision: 'rev'});
  };
  state.detailHandler = (context) => (url) => {
    const m = url.match(/\/api\/sessions\/([^/?]+)$/);
    if (!m) return undefined;
    const detail = state.detailCache[m[1]];
    if (!detail) return undefined;
    return jsonResponse(Object.assign({id: m[1], name: 'node', profile: 'manager'}, detail));
  };
  return state;
}

function findRows(sessionList, id) {
  return [...sessionList.querySelectorAll('.tree-row')].filter((el) => el.dataset.nodeId === id);
}

function rowIds(sessionList) {
  return [...sessionList.querySelectorAll('.tree-row')].map((el) => el.dataset.nodeId);
}

test('a creation notification for a node absent from the cache refreshes exactly its path levels', async () => {
  const {context, sessionList} = build();
  const server = serverState();
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 1});
  const m1 = row({id: 'm1', name: 'Manager one', task_parent_id: 'root-1', child_count: 0, open_descendant_count: 0});
  server.putLevel('', [root]);
  server.putLevel('root-1', [m1]);
  server.registerDetail(m1, ['root-1']);
  context.fetchHandlers.push(server.treeHandler(context), server.detailHandler(context));
  context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  await context.Sidebar.SessionTree.toggleTreeNode('root-1'); // m1 visible, m1's children never fetched
  const fetchesBefore = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;

  // Another client creates w1 under m1 (m1 collapsed; w1 unknown to the cache).
  const w1 = row({id: 'w1', name: 'New worker', profile: 'worker', task_parent_id: 'm1'});
  server.putLevel('m1', [w1]);
  server.registerDetail(w1, ['m1', 'root-1']);
  root.child_count = 1; root.open_descendant_count = 2;
  m1.child_count = 1; m1.open_descendant_count = 1;
  server.putLevel('', [root]);         // ancestor facts refreshed server-side
  server.putLevel('root-1', [m1]);

  context.handleWSEvent({type: 'task_tree_changed', session_id: 'w1', fact_type: 'task_created'},
                        context._sessionId, 0);
  await flush();
  // Bounded work: only the path levels ('', root-1, m1) were refetched.
  const treeFetches = context.fetchCalls.slice(fetchesBefore).filter((c) => c.url.includes('/tree?'));
  const levels = treeFetches.map((c) => new URLSearchParams(c.url.split('?')[1]).get('parent_id') || '');
  assert.deepEqual([...new Set(levels)].sort(), ['', 'm1', 'root-1'],
    'exactly the creation path levels were refetched');
  // m1 stays collapsed (no children container), but its row gained the count.
  const m1Row = findRows(sessionList, 'm1')[0];
  assert.ok(m1Row, 'the collapsed parent row renders');
  assert.ok(!document_ids(sessionList).includes('tree-children-m1'), 'the collapsed level stays collapsed');
  assert.ok(m1Row.textContent.includes('1 open'), 'the collapsed parent gained the correct count');
  // Expanding the collapsed parent reveals the new child from the fetched level.
  await context.Sidebar.SessionTree.toggleTreeNode('m1');
  assert.equal(findRows(sessionList, 'w1').length, 1, 'the idle new task appears without any other event');
});

function document_ids(sessionList) {
  return [...sessionList.querySelectorAll('[id]')].map((el) => el.id);
}

test('a creation deeper than one level works for nodes absent from the cache', async () => {
  const {context, sessionList} = build();
  const server = serverState();
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 1});
  const m1 = row({id: 'm1', name: 'Manager one', task_parent_id: 'root-1', child_count: 0, open_descendant_count: 0});
  server.putLevel('', [root]);
  server.putLevel('root-1', [m1]);
  server.registerDetail(m1, ['root-1']);
  context.fetchHandlers.push(server.treeHandler(context), server.detailHandler(context));
  context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  await context.Sidebar.SessionTree.toggleTreeNode('root-1');

  // Two out-of-band creations: m2 under m1, then w1 under m2 — both deeper
  // than the observer's fetched levels.
  const m2 = row({id: 'm2', name: 'Manager two', task_parent_id: 'm1', child_count: 0, open_descendant_count: 0});
  const w1 = row({id: 'w1', name: 'Deep worker', profile: 'worker', task_parent_id: 'm2'});
  server.putLevel('m1', [m2]);
  server.putLevel('m2', [w1]);
  server.registerDetail(m2, ['m1', 'root-1']);
  server.registerDetail(w1, ['m2', 'm1', 'root-1']);
  m1.child_count = 1; m1.open_descendant_count = 2;
  root.child_count = 1; root.open_descendant_count = 3;
  server.putLevel('', [root]);
  server.putLevel('root-1', [m1]);

  context.Sidebar.SessionTree.onTreeChanged('m2');
  await flush();
  context.Sidebar.SessionTree.onTreeChanged('w1');
  await flush();
  const rootRow = findRows(sessionList, 'root-1')[0];
  assert.ok(rootRow.textContent.includes('3 open'), 'the root row counts the deep creations');
  const m1Row = findRows(sessionList, 'm1')[0];
  assert.ok(m1Row.textContent.includes('2 open'), 'the middle manager counts its new subtree');
  // The whole new chain is reachable by expansion. Expansion force-refreshes
  // the level it opens (one fetch each), so what it renders is the current
  // server truth even if a notification was missed while collapsed.
  const fetchesBeforeExpand = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;
  await context.Sidebar.SessionTree.toggleTreeNode('m1');
  assert.equal(findRows(sessionList, 'm2').length, 1, 'the new middle manager renders');
  await context.Sidebar.SessionTree.toggleTreeNode('m2');
  assert.equal(findRows(sessionList, 'w1').length, 1, 'the deep new worker renders');
  const expandFetches = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length - fetchesBeforeExpand;
  assert.equal(expandFetches, 2, 'each expansion re-reads exactly the level it opens');
});

test('duplicate creation notifications never duplicate rows or levels', async () => {
  const {context, sessionList} = build();
  const server = serverState();
  const root = row({id: 'root-1', name: 'Program', child_count: 0, open_descendant_count: 0});
  server.putLevel('', [root]);
  server.registerDetail(root, []);
  context.fetchHandlers.push(server.treeHandler(context), server.detailHandler(context));
  context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  const newRoot = row({id: 'root-2', name: 'Remote root', child_count: 0, open_descendant_count: 0});
  server.putLevel('', [root, newRoot]);
  server.registerDetail(newRoot, []);
  root.child_count = 0; root.open_descendant_count = 0;
  for (let i = 0; i < 3; i++) {
    context.Sidebar.SessionTree.onTreeChanged('root-2');
    await flush();
  }
  assert.equal(findRows(sessionList, 'root-2').length, 1, 'the new root renders exactly once');
  assert.equal(rowIds(sessionList).filter((id) => id === 'root-1').length, 1);
});

test('panel hooks still fire while another sidebar filter owns the list', async () => {
  const {context, sessionList} = build();
  const server = serverState();
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 1});
  const m1 = row({id: 'm1', name: 'Manager one', task_parent_id: 'root-1', child_count: 0, open_descendant_count: 0});
  server.putLevel('', [root]);
  server.putLevel('root-1', [m1]);
  server.registerDetail(m1, ['root-1']);
  context.fetchHandlers.push(server.treeHandler(context), server.detailHandler(context));
  context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  await context.Sidebar.SessionTree.toggleTreeNode('root-1'); // m1 rendered, m1's level never fetched
  // Another filter painted #session-list with its own rows.
  sessionList.textContent = '';
  const foreign = context.__doc.createElement('div');
  foreign.className = 'session-row';
  foreign.textContent = 'legacy session row';
  sessionList.appendChild(foreign);
  context.currentFilter = 'all';

  const panelCalls = {task: [], runs: [], context: []};
  context.TaskPanel = {onTreeChanged: (ids) => panelCalls.task.push(ids)};
  context.TaskRunsPanel = {onTreeChanged: (ids) => panelCalls.runs.push(ids)};
  context.TaskContextPanel = {onTreeChanged: (ids) => panelCalls.context.push(ids)};

  const w1 = row({id: 'w1', name: 'New worker', profile: 'worker', task_parent_id: 'm1'});
  server.putLevel('m1', [w1]);
  server.registerDetail(w1, ['m1', 'root-1']);
  m1.child_count = 1; m1.open_descendant_count = 1;
  root.open_descendant_count = 2;
  server.putLevel('', [root]);
  server.putLevel('root-1', [m1]);

  context.handleWSEvent({type: 'task_tree_changed', session_id: 'w1', fact_type: 'task_created'},
                        context._sessionId, 0);
  await flush();
  // The ids array is produced inside the module's vm realm: compare by content.
  assert.equal(panelCalls.task.length, 1, 'the open task panel still learns about the change');
  assert.equal(JSON.stringify(panelCalls.task[0]), JSON.stringify(['w1']));
  assert.equal(JSON.stringify(panelCalls.runs[0]), JSON.stringify(['w1']));
  assert.equal(JSON.stringify(panelCalls.context[0]), JSON.stringify(['w1']));
  assert.equal(sessionList.querySelectorAll('.session-row').length, 1,
    'the active filter keeps its rendered list');
  assert.equal(sessionList.querySelectorAll('.tree-row').length, 0,
    'the tree never repaints into another filter view');
  // The data refresh still happened: the cached levels are fresh for the return.
  context.currentFilter = 'tasks';
  context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  await context.Sidebar.SessionTree.toggleTreeNode('m1');
  assert.equal(findRows(sessionList, 'w1').length, 1,
    'returning to the tree shows the creation that arrived under another filter');
});
