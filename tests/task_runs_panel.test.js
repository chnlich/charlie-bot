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

test('pagination loads newer pages on demand and never duplicates rows', async () => {
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
  assert.ok(tab.textContent.includes('Load newer runs'), 'the cursor offers the next chronological page');
  const moreBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Load newer runs');
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

test('the child list pages past its first page and never duplicates a row', async () => {
  const {context, tab} = build();
  const first = Array.from({length: 3}, (_, i) => ({id: 'ch-' + i, name: 'Child ' + i, profile: 'worker', work_state: 'idle', archived: false}));
  const second = [{id: 'ch-2', name: 'Child 2', profile: 'worker', work_state: 'idle', archived: false}, {id: 'ch-late', name: 'Late child', profile: 'worker', work_state: 'idle', archived: false}];
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) {
      assert.ok(url.includes('parent_id=node-1') && url.includes('include_archived=true'), 'children ride the tree API');
      call++;
      if (call === 1) return jsonResponse({items: first, next_cursor: 'c2', tree_revision: 'r'});
      return jsonResponse({items: second, next_cursor: null, tree_revision: 'r'});
    }
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  context.TaskRunsPanel.refresh();
  await flush();
  assert.ok(tab.textContent.includes('Load more children'), 'a visible continuation is offered');
  const before = context.fetchCalls.filter((c) => c.url.includes('/tree?')).length;
  tab.querySelectorAll('button').find((b) => b.textContent.startsWith('Load more children')).dispatch('click');
  await flush();
  assert.equal(context.fetchCalls.filter((c) => c.url.includes('/tree?')).length, before + 1, 'the continuation fetched exactly one more page');
  const linkIds = tab.querySelectorAll('a').filter((a) => a.href && a.href.includes('session=ch-')).map((a) => a.href);
  assert.equal(linkIds.length, 4, 'all children are linked');
  assert.deepEqual([...new Set(linkIds)].length, 4, 'the row repeated across pages never duplicates');
  assert.ok(tab.textContent.includes('Late child'), 'the child beyond the first page is reachable');
  assert.ok(!tab.textContent.includes('Load more children'), 'the exhausted list stops offering a continuation');
});

test('a tree change during child pagination reloads the section coherently', async () => {
  const {context, tab} = build();
  const first = [{id: 'ch-1', name: 'Child 1', profile: 'worker', work_state: 'idle', archived: false}];
  const fresh = [
    {id: 'ch-1', name: 'Child 1', profile: 'worker', work_state: 'idle', archived: false},
    {id: 'ch-new', name: 'Child created mid-paging', profile: 'worker', work_state: 'idle', archived: false},
  ];
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) {
      call++;
      if (call === 1) return jsonResponse({items: first, next_cursor: 'stale-cursor', tree_revision: 'rev-1'});
      if (call === 2) {
        // The cursor was minted under rev-1; the tree moved since: the server refuses.
        return {ok: false, status: 409, json: async () => ({detail: {message: 'task tree changed during pagination; refresh and re-paginate'}})};
      }
      return jsonResponse({items: fresh, next_cursor: null, tree_revision: 'rev-2'});
    }
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  context.TaskRunsPanel.refresh();
  await flush();
  tab.querySelectorAll('button').find((b) => b.textContent.startsWith('Load more children')).dispatch('click');
  await flush();
  const links = tab.querySelectorAll('a').filter((a) => a.href && a.href.includes('session=ch-'));
  assert.deepEqual(links.map((a) => a.href.match(/session=([^&]+)/)[1]).sort(), ['ch-1', 'ch-new'],
    'the reloaded section has every child exactly once');
  assert.ok(tab.textContent.includes('task tree changed'), 'the revision change is explained');
  assert.ok(!tab.textContent.includes('Load more children'), 'the reloaded exhausted list closes the continuation');
});

test('a failed children read surfaces with retry instead of a false "No child tasks"', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) return {ok: false, status: 500, json: async () => ({detail: 'boom'})};
    return undefined;
  });
  context.TaskRunsPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  context.TaskRunsPanel.refresh();
  await flush();
  assert.ok(tab.textContent.includes('Failed to load child tasks'), 'the failure is named');
  assert.ok(!tab.textContent.includes('No child tasks.'), 'an error is never misreported as an empty list');
  const retry = tab.querySelectorAll('button').find((b) => b.textContent === 'Retry');
  assert.ok(retry, 'a retry action is offered');
  context.fetchHandlers.splice(0, context.fetchHandlers.length);
  context.fetchHandlers.push((url) => {
    if (url.includes('/runs?limit=50')) return jsonResponse({items: [], next_cursor: null});
    if (url.includes('/api/sessions/tree?')) return jsonResponse({items: [{id: 'ch-ok', name: 'Child ok', profile: 'worker', work_state: 'idle', archived: false}], next_cursor: null, tree_revision: 'r'});
    return undefined;
  });
  retry.dispatch('click');
  await flush();
  assert.ok(tab.textContent.includes('Child ok'), 'the retry recovers into the real list');
});
