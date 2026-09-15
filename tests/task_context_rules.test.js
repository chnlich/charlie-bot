const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse} = require('./task_ui_context_stub');

const DETAIL = {
  id: 'node-1',
  name: 'Feature manager',
  profile: 'manager',
  task_state: 'open',
  work_state: 'idle',
  archived: false,
  ancestors: [],
  task: {goal: 'g'},
  prompt_rules: {
    node: {ref: 'node-ref-1', source: '/home/prompt_bodies/node-ref-1.md', chars: 11, text: 'node local'},
    subtree: {ref: 'sub-ref-1', source: '/home/prompt_bodies/sub-ref-1.md', chars: 15, text: 'subtree shared'},
    affected_descendants: 3,
  },
};

const PREVIEW = {
  kind: 'manager_turn',
  prompt_hash: 'h'.repeat(64),
  char_count: 42,
  overlay: null,
  blocks: [
    {text: 'base template text', delivery: 'full', sources: [{scope: 'base', source_ref: 'base:manager_turn', source_session_id: null}]},
    {text: 'memory index text', delivery: 'index', sources: [{scope: 'memory', source_ref: 'memory:alpha', source_session_id: null}]},
    {text: 'subtree shared', delivery: 'full', sources: [{scope: 'subtree', source_ref: 'prompt_bodies/sub-ref-1.md', source_session_id: 'node-1'}]},
    {text: 'node local', delivery: 'full', sources: [{scope: 'node', source_ref: 'prompt_bodies/node-ref-1.md', source_session_id: 'node-1'}]},
  ],
};

function build(overrides = {}) {
  const context = buildContext(overrides);
  loadModules(context, ['task-context.js']);
  const doc = context.__doc;
  const tab = doc.createElement('div');
  tab.id = 'tab-task-context';
  doc.body.appendChild(tab);
  return {context, doc, tab};
}

async function flush(rounds = 12) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

function install(context, overrides = {}) {
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(overrides.detail || DETAIL);
    if (url.startsWith('/api/sessions/node-1/effective-prompt')) return jsonResponse(overrides.preview || PREVIEW);
    // Current-run selection is the server's newest-first page: the rows the
    // run owner calls the latest. (overrides.runs keeps naming the latest
    // started run or runs the server would open a descending page with.)
    if (url.includes('/runs?') && url.includes('order=desc')) {
      return jsonResponse({items: overrides.runs || [], next_cursor: null});
    }
    if (url.includes('/runs/') && url.endsWith('/context')) return jsonResponse(overrides.runContext || {snapshot: null, legacy_prompt: null});
    return undefined;
  });
}

test('the preview lists every source with scope, owner, delivery and measured chars', async () => {
  const {context, tab} = build();
  install(context);
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('base:manager_turn'));
  assert.ok(text.includes('memory:alpha'));
  assert.ok(text.includes('index only'), 'an index-delivered memory source is labelled as index, never as full text');
  assert.ok(text.includes('owner node-1'), 'a local rule names its owning node');
  assert.ok(text.includes('42 chars'));
  assert.ok(text.includes('hhhhhhhhhhhh'), 'the prompt hash is shown');
  // Identical-text sources stay individually inspectable: each source row lists its own ref.
  assert.ok(text.includes('prompt_bodies/sub-ref-1.md'));
  assert.ok(text.includes('prompt_bodies/node-ref-1.md'));
});

test('advanced disclosure shows the exact assembled text and full hash', async () => {
  const {context, tab} = build();
  install(context);
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const adv = tab.textContent;
  assert.ok(adv.includes('Advanced'), 'an advanced disclosure exists');
  assert.ok(adv.includes('h'.repeat(64)), 'the full hash is disclosed');
});

test('current run vs next run: changed sources are outlined from server facts', async () => {
  const {context, tab} = build();
  const currentSnapshot = {
    prompt_hash: 'c'.repeat(64),
    char_count: 30,
    blocks: [
      {text: 'base template text', delivery: 'full', sources: [{scope: 'base', source_ref: 'base:manager_turn', source_session_id: null}]},
      {text: 'old subtree rule', delivery: 'full', sources: [{scope: 'subtree', source_ref: 'prompt_bodies/old.md', source_session_id: 'node-1'}]},
    ],
  };
  install(context, {
    runs: [{id: 'run-1', kind: 'manager_turn', state: 'success', started_at: '2026-01-01T00:00:00Z',
            prompt_snapshot_ref: '/x/prompt_snapshot.json'}],
    runContext: {snapshot: currentSnapshot, legacy_prompt: null},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('Current run'), 'the current-run section renders');
  assert.ok(text.includes('run-1'.slice(0, 8)) || text.includes('run-1'), 'the current run is named');
  assert.ok(text.includes('Green outline') || text.includes('removed since the current run'),
    'the changed-source explanation is present');
});

test('the rule editor loads the authoritative text and PATCHes the chosen scope', async () => {
  const {context, tab} = build();
  install(context);
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const editor = context.__doc.getElementById('task-rule-editor');
  assert.ok(editor, 'the editor exists');
  assert.equal(editor.value, 'node local', 'existing text comes from the authoritative rule body');
  // The node scope states its reach; the subtree scope reports the affected
  // descendant count and includes self.
  assert.ok(tab.textContent.includes('descendants never inherit it'));
  const subtreeRadio = tab.querySelectorAll('input[name="task-rule-scope"]')[1];
  subtreeRadio.checked = true;
  subtreeRadio.dispatch('change', {target: subtreeRadio});
  await flush(2);
  assert.equal(context.__doc.getElementById('task-rule-editor').value, 'subtree shared');
  assert.ok(tab.textContent.includes('3 descendant task(s)'), 'the affected count is shown');
  assert.ok(tab.textContent.includes('This task is included'));
  // Save a subtree edit through PATCH.
  const editor2 = context.__doc.getElementById('task-rule-editor');
  editor2.value = 'subtree shared v2';
  editor2.dispatch('input', {target: editor2});
  await flush(1);
  assert.ok(tab.textContent.includes('Unsaved draft'), 'unsaved text is flagged as a draft');
  const saveBtn = tab.querySelectorAll('button').find((b) => b.textContent.includes('Save'));
  saveBtn.dispatch('click');
  await flush();
  const patch = context.fetchCalls.find((c) => c.opts.method === 'PATCH');
  assert.ok(patch, 'a PATCH was sent');
  assert.deepEqual(JSON.parse(patch.opts.body), {subtree_prompt: 'subtree shared v2'});
});

test('Clear rule PATCHes null and the editor refuses to invent text', async () => {
  const {context, tab} = build();
  install(context);
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const clearBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Clear rule');
  clearBtn.dispatch('click');
  await flush();
  const patch = context.fetchCalls.find((c) => c.opts.method === 'PATCH');
  assert.deepEqual(JSON.parse(patch.opts.body), {node_prompt: null});
});

test('a missing/corrupt rule surfaces an actionable error, not a silent empty prompt', async () => {
  const {context, tab} = build();
  install(context, {
    preview: {ok: false},
  });
  context.fetchHandlers.length = 0;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.startsWith('/api/sessions/node-1/effective-prompt')) {
      return {ok: false, status: 500, json: async () => ({detail: 'local rule body missing at prompt_bodies/gone.md: file not found'})};
    }
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    return undefined;
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('local rule body missing'), 'the server error is surfaced');
  assert.ok(text.includes('Retry'), 'a retry action is offered');
  // The rule editor remains reachable for an edit/clear path.
  assert.ok(context.__doc.getElementById('task-rule-editor'), 'the editor stays available');
});

test('a historical run view shows snapshot facts or honest legacy evidence', async () => {
  const {context, tab} = build();
  install(context, {
    runContext: {
      snapshot: null,
      legacy_prompt: {ref: '/runs/r1/launch_prompt.md', sha256: 'abc', note: 'raw launch text recorded before the context stage: limited evidence'},
    },
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  await context.TaskContextPanel.showHistoricalRun('run-legacy');
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('Limited historical evidence'), 'legacy evidence is labelled limited');
  assert.ok(text.includes('raw launch text recorded before the context stage'), 'the honest note is shown');
  assert.ok(!text.includes('managed instructions snapshot of record'), 'no snapshot provenance is fabricated');
});

test('a late response for a prior node never replaces the active context panel', async () => {
  const {context, tab} = build();
  let resolveSlow;
  const slowDetail = new Promise((resolve) => { resolveSlow = resolve; });
  const otherDetail = Object.assign({}, DETAIL, {id: 'node-2', name: 'Other', prompt_rules: {
    node: {ref: 'n2', source: '/x2', chars: 8, text: 'other node rule'},
    subtree: {ref: null, source: null, chars: 0, text: null},
    affected_descendants: 0,
  }});
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return slowDetail.then(() => jsonResponse(DETAIL));
    if (url === '/api/sessions/node-2') return jsonResponse(otherDetail);
    if (url.startsWith('/api/sessions/node-1/effective-prompt') || url.startsWith('/api/sessions/node-2/effective-prompt')) return jsonResponse(PREVIEW);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    return undefined;
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush(2);
  context.TaskContextPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush(3);
  assert.equal(context.__doc.getElementById('task-rule-editor').value, 'other node rule',
    'the active node owns the editor');
  resolveSlow();
  await flush(6);
  // The slow node-1 response landed after the switch: the panel still shows
  // node-2's authoritative rule, never the prior node's.
  assert.equal(context.__doc.getElementById('task-rule-editor').value, 'other node rule');
  assert.ok(tab.textContent.includes('n2.md'), 'the displayed refs belong to the active node');
  assert.ok(!tab.textContent.includes('n1.md'));
});

test('editing a rule never promotes chat text and never writes a second store', async () => {
  const {context, tab} = build();
  install(context);
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const editor = context.__doc.getElementById('task-rule-editor');
  editor.value = 'typed rule text';
  editor.dispatch('input', {target: editor});
  await flush(2);
  // The only persistence is the in-memory draft flag; no second store, no PATCH.
  const writes = context.fetchCalls.filter((c) => c.opts.method === 'PATCH');
  assert.equal(writes.length, 0, 'typing alone never issues a PATCH');
  assert.ok(tab.textContent.includes('Unsaved draft'));
});

test('the current-run section is the server-selected latest started run, from one bounded request', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.startsWith('/api/sessions/node-1/effective-prompt')) return jsonResponse(PREVIEW);
    // The ascending chronological page (the pagination default) holds only
    // the oldest 100 runs — the panel must never consume it for selection.
    if (url.includes('/runs?limit=100')) return jsonResponse({
      items: [
        {id: 'run-old-0001', kind: 'manager_turn', state: 'success', started_at: '2026-01-01T00:00:00Z',
         prompt_snapshot_ref: '/x/prompt_snapshot.json'},
        {id: 'run-new-0002', kind: 'manager_turn', state: 'success', started_at: '2026-01-01T00:01:00Z',
         prompt_snapshot_ref: '/y/prompt_snapshot.json'},
      ],
      next_cursor: 'cursor-101',
    });
    // The run owner's newest-first read opens with the actual latest launch.
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({
      items: [{id: 'run-new-0002', kind: 'manager_turn', state: 'success',
               started_at: '2026-01-01T00:01:00Z', prompt_snapshot_ref: '/y/prompt_snapshot.json'}],
      next_cursor: null,
    });
    if (url.includes('/runs/run-new-0002/context')) return jsonResponse({snapshot: {
      prompt_hash: 'n'.repeat(64), char_count: 21,
      blocks: [{text: 'newest snapshot block', delivery: 'full',
                sources: [{scope: 'node', source_ref: 'prompt_bodies/new.md', source_session_id: 'node-1'}]}],
    }, legacy_prompt: null});
    if (url.includes('/runs/run-old-0001/context')) return jsonResponse({snapshot: {
      prompt_hash: 'o'.repeat(64), char_count: 20,
      blocks: [{text: 'oldest snapshot block', delivery: 'full',
                sources: [{scope: 'node', source_ref: 'prompt_bodies/old.md', source_session_id: 'node-1'}]}],
    }, legacy_prompt: null});
    return undefined;
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('Run run-new'), 'the latest started run is the current run');
  assert.ok(text.includes('newest snapshot block'), 'its snapshot is the one rendered');
  assert.ok(!text.includes('oldest snapshot block'), 'the oldest run is not presented as current');
  const runsFetches = context.fetchCalls.filter((c) => c.url.includes('/runs?'));
  assert.equal(runsFetches.length, 1, 'selection is one server request, not a page walk');
  assert.ok(runsFetches[0].url.includes('order=desc'), 'the request is the newest-first read');
});
