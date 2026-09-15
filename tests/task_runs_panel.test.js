const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse} = require('./task_ui_context_stub');

function build(overrides = {}) {
  const context = buildContext(overrides);
  loadModules(context, ['task-runs-panel.js']);
  const doc = context.__doc;
  const tab = doc.createElement('div');
  tab.id = 'tab-runs';
  doc.body.appendChild(tab);
  return {context, doc, tab};
}

async function flush(rounds = 12) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

function run(overrides = {}) {
  return Object.assign({
    id: 'run-' + Math.random().toString(16).slice(2, 8),
    session_id: 'node-1',
    kind: 'work',
    state: 'queued',
    stop_requested: false,
    pid: null,
    exit_code: null,
    started_at: null,
    ended_at: null,
    input_event_ids: [],
    raw_log_ref: null,
    events_ref: null,
    result_ref: null,
  }, overrides);
}

test('runs render with fact state, timing and N/A for unavailable measurements', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) {
      return jsonResponse({items: [
        run({id: 'run-live', state: 'running', started_at: '2026-01-01T00:00:00Z', model: 'glm-4.7'}),
        run({id: 'run-done', state: 'success', started_at: '2026-01-01T00:00:00Z', ended_at: '2026-01-01T00:02:05Z', exit_code: 0, raw_log_ref: '/h/runs/x/raw.log'}),
      ], next_cursor: null});
    }
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'r'});
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('running'));
  assert.ok(text.includes('success'));
  assert.ok(text.includes('2m5s'), 'the measured duration renders');
  assert.ok(text.includes('N/A'), 'unavailable measurements show N/A, never fabricated zeros');
  assert.ok(text.includes('raw log'), 'evidence links render');
  const link = tab.querySelectorAll('a').find((a) => a.textContent === 'raw log');
  assert.ok(link && link.href.startsWith('/files/'), 'evidence rides the file server');
});

test('Stop targets one specific run through its identity-aware endpoint', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) {
      return jsonResponse({items: [
        run({id: 'run-a', state: 'running'}),
        run({id: 'run-b', state: 'queued'}),
        run({id: 'run-c', state: 'success'}),
      ], next_cursor: null});
    }
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'r'});
    if (url.endsWith('/runs/run-a/cancel')) return jsonResponse({outcome: 'interrupted'});
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush();
  const stopBtns = tab.querySelectorAll('button').filter((b) => b.textContent === 'Stop');
  assert.equal(stopBtns.length, 2, 'stop is offered for the running and the queued run only');
  stopBtns[0].dispatch('click');
  await flush();
  const cancelCall = context.fetchCalls.find((c) => c.opts.method === 'POST');
  assert.ok(cancelCall.url.endsWith('/runs/run-a/cancel'), 'the cancel hits that run\'s endpoint');
  const body = JSON.parse(cancelCall.opts.body);
  assert.ok(body.request_id, 'the stop carries a request id');
  assert.ok(!tab.textContent.includes('Complete'), 'stopping a run is not labelled as completing the task');
});

test('Retry is explicit on a failed run and posts the run-scoped retry', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) {
      return jsonResponse({items: [run({id: 'run-f', state: 'failed', started_at: '2026-01-01T00:00:00Z'})], next_cursor: null});
    }
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'r'});
    if (url.endsWith('/retry')) return jsonResponse({run_id: 'run-new'});
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush();
  const retryBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Retry');
  assert.ok(retryBtn, 'a failed run offers Retry');
  retryBtn.dispatch('click');
  await flush();
  const retryCall = context.fetchCalls.find((c) => c.url.endsWith('/retry'));
  assert.ok(retryCall, 'retry posts the retry endpoint');
  const body = JSON.parse(retryCall.opts.body);
  assert.equal(body.run_id, 'run-f');
  assert.ok(body.request_id);
});

test('pagination loads older pages on demand and never duplicates rows', async () => {
  const {context, tab} = build();
  const pageOne = [run({id: 'r1'}), run({id: 'r2'})];
  const pageTwo = [run({id: 'r1'}), run({id: 'r3'})]; // r1 repeated across pages
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (!url.includes('/runs?limit=50')) return undefined;
    call++;
    if (call === 1) return jsonResponse({items: pageOne, next_cursor: 'cur-2'});
    return jsonResponse({items: pageTwo, next_cursor: null});
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush();
  assert.ok(tab.textContent.includes('Load older runs'), 'the cursor offers older pages');
  const moreBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Load older runs');
  moreBtn.dispatch('click');
  await flush();
  const rowIds = tab.querySelectorAll('span')
    .map((s) => s._text)
    .filter((t) => /^r[0-9]$/.test(t));
  assert.deepEqual([...new Set(rowIds)].sort(), ['r1', 'r2', 'r3'], 'appends without duplicates');
});

test('direct child tasks list with links; archived children included', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) {
      assert.ok(url.includes('parent_id=node-1'), 'children are queried for this node');
      assert.ok(url.includes('include_archived=true'), 'archived children stay linkable');
      return jsonResponse({items: [
        {id: 'child-1', name: 'Child task', profile: 'worker', work_state: 'idle', archived: false},
        {id: 'child-2', name: 'Done child', profile: 'worker', work_state: 'idle', archived: true},
      ], next_cursor: null, tree_revision: 'r'});
    }
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  context.TaskRunsPanel.refresh();
  await flush();
  const links = tab.querySelectorAll('a').filter((a) => a.href && a.href.includes('session=child'));
  assert.equal(links.length, 2, 'both children link to their node pages');
  assert.ok(tab.textContent.includes('archived'), 'the archived child is labelled');
});

test('a late runs response for a prior node never replaces the active panel', async () => {
  const {context, tab} = build();
  let resolveSlow;
  const slow = new Promise((resolve) => { resolveSlow = resolve; });
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/node-1/runs?limit=50') return slow.then(() => jsonResponse({items: [run({id: 'old-node-run'})], next_cursor: null}));
    if (url === '/api/sessions/node-2/runs?limit=50') return jsonResponse({items: [run({id: 'new-node-run'})], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'r'});
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush(2);
  context.TaskRunsPanel.onSessionChanged({id: 'node-2', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush(4);
  assert.ok(tab.textContent.includes('new-node'), 'the active node\'s rows render (ids truncate at 8 chars)');
  resolveSlow();
  await flush(8);
  assert.ok(tab.textContent.includes('new-node'), 'the active node\'s rows remain');
  assert.ok(!tab.textContent.includes('old-node'), 'the late prior-node response never lands');
});

test('run context selection hands the run to the Context panel', async () => {
  const {context, tab} = build();
  let historicalCalls = [];
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [run({id: 'run-h', state: 'success'})], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'r'});
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  context.TaskRunsPanel.refresh();
  await flush();
  context.TaskContextPanel = {showHistoricalRun: async (id) => { historicalCalls.push(id); }};
  const ctxBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Context');
  ctxBtn.dispatch('click');
  await flush(2);
  assert.deepEqual(historicalCalls, ['run-h'], 'the Context panel receives the exact run');
});
