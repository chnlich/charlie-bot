const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse, row} = require('./task_ui_context_stub');

// The running-feedback regression: tree rows render the sidebar's shared
// spinner/gear from task/Run facts, launch and terminal outcomes reach them
// without further input, and legacy probes never override the tree's facts.
// Module order mirrors the page (page-timers -> status -> tree -> websocket).
function build(overrides = {}) {
  const context = buildContext(overrides);
  context.showStreaming = () => {};
  context.hideStreaming = () => {};
  context.appendMessageObject = () => {};
  context.pollActiveSessionView = () => {};
  loadModules(context, ['page-timers.js', 'sidebar/status.js', 'sidebar/session-tree.js', 'websocket.js']);
  const doc = context.__doc;
  doc.register(doc.createElement('div'));
  // The thinking-indicator elements the page always carries; the legacy
  // running_changed path touches them for the selected session.
  for (const id of ['thinking', 'send-btn']) {
    const el = doc.createElement('div');
    el.id = id;
    doc.body.appendChild(el);
  }
  const sessionList = doc.createElement('nav');
  sessionList.id = 'session-list';
  doc.body.appendChild(sessionList);
  return {context, doc, sessionList};
}

async function flush(rounds = 12) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

// A mutable three-node server: root(m) -> feat(m) -> {w1, w2}. Tests mutate the
// served rows the way the backend's own durable writes do, then deliver the
// same task_tree_changed notification the control-event sink emits.
function serverState() {
  const state = {levels: {}};
  state.putLevel = (parent, rows) => { state.levels[parent || ''] = rows; };
  state.treeHandler = () => (url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const parent = new URLSearchParams(m[1]).get('parent_id') || '';
    // The wire is a copy boundary: a client cache never aliases server state,
    // so a stale cached row stays stale until a refresh actually lands.
    const items = (state.levels[parent] || []).map((r) => JSON.parse(JSON.stringify(r)));
    return jsonResponse({items, next_cursor: null, tree_revision: 'rev'});
  };
  return state;
}

function defaultTree() {
  const root = row({id: 'root-1', name: 'Program', child_count: 1, open_descendant_count: 2});
  const feat = row({id: 'feat-1', name: 'Feature', task_parent_id: 'root-1', child_count: 2, open_descendant_count: 2});
  const w1 = row({id: 'work-1', name: 'Worker One', profile: 'worker', task_parent_id: 'feat-1'});
  const w2 = row({id: 'work-2', name: 'Worker Two', profile: 'worker', task_parent_id: 'feat-1'});
  return {root, feat, w1, w2};
}

async function setup(overrides = {}) {
  const built = build(overrides);
  const server = serverState();
  const nodes = defaultTree();
  server.putLevel('', [nodes.root]);
  server.putLevel('root-1', [nodes.feat]);
  server.putLevel('feat-1', [nodes.w1, nodes.w2]);
  built.context.fetchHandlers.push(server.treeHandler(built.context));
  built.context.Sidebar.SessionTree.enterTreeFilter();
  await flush(2);
  await built.context.Sidebar.SessionTree.toggleTreeNode('root-1');
  await built.context.Sidebar.SessionTree.toggleTreeNode('feat-1');
  built.server = server;
  built.nodes = nodes;
  return built;
}

function spinnerEl(doc, id) { return doc.getElementById('spinner-' + id); }
function gearEl(doc, id) { return doc.getElementById('worker-indicator-' + id); }

test('a running task row shows the shared spinner adjacent to its name; idle rows show neither cue', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();

  const spinner = spinnerEl(doc, 'work-1');
  const gear = gearEl(doc, 'work-1');
  assert.ok(spinner, 'the running row renders the shared spinner element');
  assert.ok(!spinner.classList.contains('hidden'), 'the spinner is visible for a running task');
  assert.ok(spinner.getAttribute('class').includes('animate-spin text-yellow-400'),
    'the spinner carries the established activity visual language');
  assert.equal(spinner.getAttribute('title'), 'Task is running');
  assert.ok(gear && gear.classList.contains('hidden'), 'the delegated gear stays hidden for own work');

  // The cue sits directly beside the session name in the row layout.
  const rowEl = doc.getElementById('tree-node-work-1');
  const inner = rowEl.firstElementChild;
  const children = inner.children;
  const nameIdx = children.indexOf(rowEl.querySelector('.session-name'));
  const activityIdx = children.indexOf(rowEl.querySelector('.tree-activity'));
  assert.equal(activityIdx, nameIdx - 1, 'the activity cue is immediately adjacent to the name');

  const idleSpinner = spinnerEl(doc, 'work-2');
  const idleGear = gearEl(doc, 'work-2');
  assert.ok(idleSpinner.classList.contains('hidden') && idleGear.classList.contains('hidden'),
    'an idle row shows no activity cue');
});

test('a manager with only running descendants shows the delegated gear; own running wins', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.work_state = 'running';
  nodes.feat.work_state = 'idle';
  nodes.feat.running_descendant_count = 1;
  nodes.root.running_descendant_count = 1;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();

  const featGear = gearEl(doc, 'feat-1');
  assert.ok(featGear && !featGear.classList.contains('hidden'),
    'the collapsed-children manager shows the delegated-work cue');
  assert.ok(spinnerEl(doc, 'feat-1').classList.contains('hidden'),
    'the manager is not spinning its own work');
  assert.equal(featGear.getAttribute('title'), 'Delegated work running in subtasks');
  const rootGear = gearEl(doc, 'root-1');
  assert.ok(rootGear && !rootGear.classList.contains('hidden'),
    'the grandparent sees the running descendant too');

  // The node's own running Run outranks the delegated cue (legacy precedence).
  nodes.feat.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('feat-1');
  await flush();
  assert.ok(!spinnerEl(doc, 'feat-1').classList.contains('hidden'), 'own spinner shows');
  assert.ok(gearEl(doc, 'feat-1').classList.contains('hidden'), 'the gear yields to own work');
});

test('queued (waiting), attention and idle stay distinguishable and show no spinner', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.work_state = 'waiting';
  nodes.w2.work_state = 'attention';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  context.Sidebar.SessionTree.onTreeChanged('work-2');
  await flush();

  for (const id of ['work-1', 'work-2']) {
    assert.ok(spinnerEl(doc, id).classList.contains('hidden'), id + ' queued/attention shows no spinner');
    assert.ok(gearEl(doc, id).classList.contains('hidden'), id + ' queued/attention shows no gear');
  }
  const waitingRow = doc.getElementById('tree-node-work-1');
  const waitingBadge = waitingRow.textContent;
  assert.ok(waitingBadge.includes('waiting'), 'the queued state keeps its readable label');
  const attentionRow = doc.getElementById('tree-node-work-2');
  assert.ok(attentionRow.textContent.includes('attention'), 'the attention state keeps its readable label');
  const idleRow = doc.getElementById('tree-node-root-1');
  assert.ok(idleRow.textContent.includes('idle'), 'idle keeps its label');
});

test('a delayed launch repaints the queued row into a spinner with no further input or reload', async () => {
  const {context, doc, server, nodes} = await setup();
  // The queued render completed first (the row painted waiting).
  nodes.w1.work_state = 'waiting';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(spinnerEl(doc, 'work-1').classList.contains('hidden'), 'queued before the launch: no spinner');

  // The child process starts: the server fact moves and the launch notification
  // rides the same task_tree_changed route the control-event sink emits.
  nodes.w1.work_state = 'running';
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-1', fact_type: 'run_launched'},
                        context._sessionId, 0);
  await flush();
  const spinner = spinnerEl(doc, 'work-1');
  assert.ok(spinner && !spinner.classList.contains('hidden'),
    'the spinner appears from the launch notification alone');
  assert.ok(doc.getElementById('tree-node-work-1').textContent.includes('running'),
    'the row label reads running');
});

test('success, failure and stop each clear the spinner through the same route', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'));

  // Success: the task stays open and becomes idle.
  nodes.w1.work_state = 'idle';
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-1', fact_type: 'run_finished'},
                        context._sessionId, 0);
  await flush();
  assert.ok(spinnerEl(doc, 'work-1').classList.contains('hidden'), 'success clears the spinner');

  // Failure: attention, not running.
  nodes.w1.work_state = 'attention';
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-1', fact_type: 'run_finished'},
                        context._sessionId, 0);
  await flush();
  assert.ok(spinnerEl(doc, 'work-1').classList.contains('hidden'), 'failure clears the spinner');
  assert.ok(doc.getElementById('tree-node-work-1').textContent.includes('attention'),
    'the failed run reads as attention');

  // A durable stop request flips the row out of running before the exit lands.
  nodes.w2.work_state = 'running';
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-2', fact_type: 'run_launched'},
                        context._sessionId, 0);
  await flush();
  assert.ok(!spinnerEl(doc, 'work-2').classList.contains('hidden'));
  nodes.w2.work_state = 'attention';
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-2', fact_type: 'run_stop_requested'},
                        context._sessionId, 0);
  await flush();
  assert.ok(spinnerEl(doc, 'work-2').classList.contains('hidden'), 'stop clears the spinner');
});

test('a stale legacy running_changed probe never clears a true task Run spinner', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'));

  // The legacy thread walk cannot see v2 worker Runs: its probe reports idle.
  context.handleWSEvent({type: 'running_changed', session_id: 'work-1',
    thinking_since: null, has_running_tasks: false}, context._sessionId, 0);
  await flush();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'),
    'the tree facts win over the legacy probe');

  // The same protection on the selected session's row.
  nodes.root.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('root-1');
  await flush();
  context.handleWSEvent({type: 'running_changed', session_id: 'root-1',
    thinking_since: null, has_running_tasks: false}, context._sessionId, 0);
  await flush();
  assert.ok(!spinnerEl(doc, 'root-1').classList.contains('hidden'),
    'the active session row keeps its true Run spinner');
});

test('a non-selected running session shows its spinner', async () => {
  const {context, doc, nodes} = await setup({sessionId: 'root-1'});
  assert.equal(context.SESSION_ID, 'root-1');
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'),
    'a background task row shows its own activity');
});

test('drift reconciliation re-reads exactly the rendered levels and repaints only on change', async () => {
  const {context, doc, server, nodes} = await setup();
  const treeFetchUrls = () => context.fetchCalls.filter((c) => c.url.includes('/api/sessions/tree?'))
    .map((c) => new URLSearchParams(c.url.split('?')[1]).get('parent_id') || '');
  const rowBefore = doc.getElementById('tree-node-work-1');
  const fetchesBefore = context.fetchCalls.length;

  // Steady state: the pass re-reads the rendered levels (roots + the two
  // expanded levels) and, with unchanged facts, repaints nothing.
  await context.Sidebar.SessionTree.reconcileRenderedActivity();
  const reconciled = treeFetchUrls().slice(-3);
  assert.deepEqual([...reconciled].sort(), ['', 'feat-1', 'root-1'],
    'the pass is bounded to the rendered levels');
  assert.equal(context.fetchCalls.length - fetchesBefore, 3, 'no requests beyond the rendered levels');
  assert.equal(doc.getElementById('tree-node-work-1'), rowBefore,
    'unchanged facts never rebuild the tree under the user');

  // A missed notification is healed: the server fact moved while the socket
  // frame was lost, and the next pass repaints the row from the fresh facts.
  nodes.w1.work_state = 'running';
  await context.Sidebar.SessionTree.reconcileRenderedActivity();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'),
    'the reconciliation restores the current activity state');
  assert.notEqual(doc.getElementById('tree-node-work-1'), rowBefore, 'the changed row was rebuilt');
});

test('reconnect reconciles the rendered levels once', async () => {
  const {context, doc, server, nodes} = await setup();
  const fetchesBefore = context.fetchCalls.length;
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onReconnected();
  await flush();
  const treeFetches = context.fetchCalls.slice(fetchesBefore).filter((c) => c.url.includes('/api/sessions/tree?'));
  const levels = treeFetches.map((c) => new URLSearchParams(c.url.split('?')[1]).get('parent_id') || '');
  assert.deepEqual([...new Set(levels)].sort(), ['', 'feat-1', 'root-1'],
    'the reconnect pass covers exactly the rendered levels');
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'),
    'state missed while the socket was down is picked up');
});

test('filter entry force-refreshes so a switch shows current activity', async () => {
  const {context, doc, server, nodes} = await setup();
  // While another filter was showing, a run started and finished; the cached
  // rows never saw it.
  nodes.w1.work_state = 'attention';
  server.putLevel('feat-1', [nodes.w1, nodes.w2]);
  context.currentFilter = 'chat';
  context.Sidebar.SessionTree.enterTreeFilter();
  context.currentFilter = 'tasks';
  await flush();
  const rowText = doc.getElementById('tree-node-work-1').textContent;
  assert.ok(rowText.includes('attention'), 'the switch renders the current state, not the cached one');
});
