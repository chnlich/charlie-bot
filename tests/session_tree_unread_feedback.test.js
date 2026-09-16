const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse, row} = require('./task_ui_context_stub');

// The unread-feedback regression: task-tree rows render the sidebar's shared
// unread dot from the SessionManager-owned unread flag (SessionRow.has_unread),
// the flag stays independent of work_state (an active run hides the dot
// without discarding it; a finished run reveals it), the existing mark-read
// path clears it across clients, and neither a stale tree-page reply nor a
// stale legacy-status reply can resurrect or erase a newer unread state.
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

// A deferred fetch handler parked ahead of the ordinary handlers: tests issue
// the request, apply a newer fact, then release a reply that was snapshotted
// at request time — the stale-reply ordering the gate must survive. The tree
// variant snapshots each level when the request arrives (the server's own
// semantics) and releases every parked level; level fetches run sequentially,
// so a release round may park the next one.
function deferredFetch(context, urlPart) {
  const pending = [];
  context.fetchHandlers.unshift((url) => {
    if (!url.includes(urlPart)) return undefined;
    return new Promise((resolve) => pending.push(resolve));
  });
  return {
    release(body) {
      assert.ok(pending.length, 'no deferred ' + urlPart + ' request in flight');
      pending.shift()(jsonResponse(body));
    },
  };
}

function deferredTreeFetch(context, server, parentId) {
  // Park only the level named by parentId (the one carrying the node under
  // test); every other level answers through the ordinary handler. The parked
  // level snapshots at request time — the server's own semantics — so a later
  // release replays a reply that was true when requested, stale by reply time.
  const pending = [];
  context.fetchHandlers.unshift((url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const parent = new URLSearchParams(m[1]).get('parent_id') || '';
    if (parent !== parentId) return undefined;
    const items = (server.levels[parent] || []).map((r) => JSON.parse(JSON.stringify(r)));
    return new Promise((resolve) => pending.push(() => resolve(
        jsonResponse({items, next_cursor: null, tree_revision: 'rev'}))));
  });
  return {
    count: () => pending.length,
    releaseAll: () => { while (pending.length) pending.shift()(); },
  };
}

async function drain(deferred) {
  for (let i = 0; i < 8; i++) {
    await flush(2);
    if (!deferred.count()) break;
    deferred.releaseAll();
  }
  await flush();
}

function serverState() {
  const state = {levels: {}};
  state.putLevel = (parent, rows) => { state.levels[parent || ''] = rows; };
  state.treeHandler = () => (url) => {
    const m = url.match(/\/api\/sessions\/tree\?(.*)$/);
    if (!m) return undefined;
    const parent = new URLSearchParams(m[1]).get('parent_id') || '';
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

function dotEl(doc, id) { return doc.getElementById('unread-' + id); }
function spinnerEl(doc, id) { return doc.getElementById('spinner-' + id); }

function unreadEvent(context, sid, hasUnread) {
  context.handleWSEvent({type: 'unread_changed', session_id: sid, has_unread: hasUnread},
                        context._sessionId, 0);
}

test('an unread idle task row shows the familiar shared dot; a read row hides it', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.has_unread = true;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();

  const dot = dotEl(doc, 'work-1');
  assert.ok(dot, 'the tree row renders the shared unread-dot element');
  assert.equal(dot.id, 'unread-work-1', 'the dot reuses the legacy pinned element id');
  for (const cls of ['w-2', 'h-2', 'rounded-full', 'bg-yellow-400', 'animate-pulse-dot', 'flex-shrink-0']) {
    assert.ok(dot.classes.has(cls), 'the dot carries the established visual language: ' + cls);
  }
  assert.ok(!dot.classList.contains('hidden'), 'an unread idle task shows the dot');
  assert.equal(dot.getAttribute('title'), 'Unread reply', 'the dot carries an English label');
  assert.ok(dotEl(doc, 'work-2').classList.contains('hidden'), 'a read task shows no dot');

  // The tree fetch fed the global unread map: legacy consumers and
  // setSessionIndicator agree with the row.
  assert.equal(context.sessionUnread['work-1'], true, 'the fetched row fact lands in the shared map');
  assert.equal(context.sessionUnread['work-2'], false);

  // The dot sits in the same activity cluster as the spinner and gear.
  const rowEl = doc.getElementById('tree-node-work-1');
  const cluster = rowEl.querySelector('.tree-activity');
  const ids = cluster.children.map((c) => c.id);
  assert.deepEqual(ids, ['spinner-work-1', 'worker-indicator-work-1', 'unread-work-1'],
                   'spinner, gear and unread dot share one indicator cluster');
});

test('unread held behind activity is kept, then shown when the run terminates', async () => {
  const {context, doc, nodes} = await setup();
  // The run starts: the row spins and the dot hides.
  nodes.w1.work_state = 'running';
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(!spinnerEl(doc, 'work-1').classList.contains('hidden'));

  // A reply lands while the run is active: the dot must hide WITHOUT
  // discarding the flag (activity outranks the dot; the flag waits).
  unreadEvent(context, 'work-1', true);
  await flush();
  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'), 'activity hides the dot');
  assert.equal(context.sessionUnread['work-1'], true, 'the unread flag is kept, not cleared');

  // The run finishes: the terminal fact flips the row to idle, and the
  // waiting dot becomes visible from the same refresh.
  nodes.w1.work_state = 'idle';
  nodes.w1.has_unread = true;
  context.handleWSEvent({type: 'task_tree_changed', session_id: 'work-1', fact_type: 'run_finished'},
                        context._sessionId, 0);
  await flush();
  assert.ok(spinnerEl(doc, 'work-1').classList.contains('hidden'), 'the spinner cleared with the terminal fact');
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'),
            'the unread reply shows once the activity ends');
});

test('opening a task clears its dot through the read broadcast; another task stays unread', async () => {
  const {context, doc, nodes} = await setup();
  nodes.w1.has_unread = true;
  nodes.w2.has_unread = true;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  context.Sidebar.SessionTree.onTreeChanged('work-2');
  await flush();
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'));
  assert.ok(!dotEl(doc, 'work-2').classList.contains('hidden'));

  // The user opens work-1 elsewhere: the mark-read broadcast clears exactly
  // that row; the sibling stays unread.
  unreadEvent(context, 'work-1', false);
  await flush();
  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'), 'the opened task was cleared');
  assert.ok(!dotEl(doc, 'work-2').classList.contains('hidden'), 'the other task remains unread');

  // A new reply after the prior read re-marks the same task unread.
  unreadEvent(context, 'work-1', true);
  await flush();
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'), 'a new unread reply shows again');

  // Ordinary navigation and refreshes never mark anything read client-side:
  // only the server's own mark-read path flips the flag.
  context.Sidebar.SessionTree.onTreeChanged('work-2');
  await context.Sidebar.SessionTree.reconcileRenderedActivity();
  const methods = context.fetchCalls.map((c) => c.opts.method || 'GET');
  assert.ok(methods.every((m) => m === 'GET'), 'sidebar refreshes issue no read/write requests');
});

test('a stale status reply cannot erase or resurrect a newer unread state', async () => {
  const {context, doc} = await setup();
  const status = deferredFetch(context, '/api/sessions/status');

  // The poll is issued, THEN the broadcast lands, THEN the stale reply
  // (snapshotted before the flip) arrives: the reply must be refused.
  const poll = context.pollSessionStatus();
  await flush(2);
  unreadEvent(context, 'work-1', true);
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'), 'the idle row shows the unread dot');
  status.release({'work-1': {has_unread: false, has_running_tasks: false, thinking_since: null}});
  await poll;
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'),
            'the stale reply did not erase the newer unread');
  assert.equal(context.sessionUnread['work-1'], true, 'the stale reply did not erase the newer unread');

  // The inverse: a reply captured before a read broadcast must not resurrect.
  const poll2 = context.pollSessionStatus();
  await flush(2);
  unreadEvent(context, 'work-1', false);
  status.release({'work-1': {has_unread: true, has_running_tasks: false, thinking_since: null}});
  await poll2;
  assert.equal(context.sessionUnread['work-1'], false, 'the stale reply did not resurrect the cleared flag');

  // A poll issued AFTER the fact applies its (current) value normally.
  const poll3 = context.pollSessionStatus();
  await flush(2);
  status.release({'work-1': {has_unread: true, has_running_tasks: false, thinking_since: null}});
  await poll3;
  assert.equal(context.sessionUnread['work-1'], true, 'a current reply is applied');
});

test('a stale tree-page reply cannot erase a newer unread state', async () => {
  const {context, doc, server, nodes} = await setup();
  const deferredTree = deferredTreeFetch(context, server, 'feat-1');

  // The refresh is issued; the page snapshots the row while it still carries
  // the unread fact. The read broadcast lands while the reply is in flight.
  nodes.w1.has_unread = true;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush(2);
  unreadEvent(context, 'work-1', false);
  await drain(deferredTree);

  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'),
            'the re-rendered row shows the newer read state, not the stale page fact');
  assert.equal(context.sessionUnread['work-1'], false, 'the shared map keeps the newer fact');

  // A page issued after the flip applies its own fact: unread again.
  nodes.w1.has_unread = true;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await drain(deferredTree);
  assert.equal(context.sessionUnread['work-1'], true, 'a current page is applied');
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'));
});

test('a missed broadcast is healed by reconnect and filter entry from server facts', async () => {
  const {context, doc, nodes} = await setup();
  // The socket was down while the reply landed; the client never saw the
  // broadcast. The reconnect pass re-reads the rendered levels and repaints.
  nodes.w1.has_unread = true;
  context.Sidebar.SessionTree.onReconnected();
  await flush();
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'), 'reconnect shows the missed unread');

  // Back to read, missed again, then a filter switch re-fetches: the same
  // bounded pass serves the current facts.
  nodes.w1.has_unread = false;
  context.currentFilter = 'chat';
  context.Sidebar.SessionTree.enterTreeFilter();
  context.currentFilter = 'tasks';
  await flush();
  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'), 'filter entry reflects the current server facts');
});

test('unread drift alone triggers the bounded reconciliation repaint', async () => {
  const {context, doc, nodes} = await setup();
  const rowBefore = doc.getElementById('tree-node-work-1');
  nodes.w1.has_unread = true;
  await context.Sidebar.SessionTree.reconcileRenderedActivity();
  assert.notEqual(doc.getElementById('tree-node-work-1'), rowBefore,
                  'an unread-only fact change repaints the row');
  assert.ok(!dotEl(doc, 'work-1').classList.contains('hidden'));

  // Unchanged facts still repaint nothing.
  const rowAfter = doc.getElementById('tree-node-work-1');
  await context.Sidebar.SessionTree.reconcileRenderedActivity();
  assert.equal(doc.getElementById('tree-node-work-1'), rowAfter, 'unchanged facts never rebuild the tree');
});

test('a broadcast for the selected session updates the shared map without touching the row', async () => {
  const {context, doc, nodes} = await setup({sessionId: 'work-1'});
  assert.equal(context.SESSION_ID, 'work-1');
  // Activity hides the dot first; the selected session's broadcast must not
  // toggle the row (the viewed session's dot is owned by the view's mark-read
  // path), but the fact is stamped for later renders.
  nodes.w1.work_state = 'running';
  nodes.w1.has_unread = true;
  context.Sidebar.SessionTree.onTreeChanged('work-1');
  await flush();
  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'), 'activity hides the dot');
  unreadEvent(context, 'work-1', true);
  await flush();
  assert.equal(context.sessionUnread['work-1'], true,
               'the selected session\'s fact is stamped for later renders');
  assert.ok(dotEl(doc, 'work-1').classList.contains('hidden'),
            'the broadcast handler left the selected row untouched');
});

test('reduced-motion styles stop every activity animation in the row', () => {
  const css = fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'css', 'styles.css'), 'utf8');
  const rule = css.split('@media (prefers-reduced-motion: reduce)')[1] || '';
  assert.ok(rule.includes('.animate-spin'), 'the spinner is covered');
  assert.ok(rule.includes('.animate-\\[spin_3s_linear_infinite\\]'), 'the delegated gear is covered');
  assert.ok(rule.includes('.animate-pulse'), 'the running badge pulse is covered');
  assert.ok(rule.includes('.animate-pulse-dot'), 'the unread dot pulse is covered');
  assert.ok(/animation:\s*none\s*!important/.test(rule),
            'the rule beats tailwind.css, which loads after styles.css');
});
