const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse} = require('./task_ui_context_stub');

const DETAIL = {
  id: 'node-1', name: 'Long-lived task', profile: 'worker', task_state: 'open',
  work_state: 'idle', archived: false, ancestors: [], task: {goal: 'g'},
  prompt_rules: {node: {ref: null, source: null, chars: 0, text: null},
                 subtree: {ref: null, source: null, chars: 0, text: null},
                 affected_descendants: 0},
};
const DETAIL2 = Object.assign({}, DETAIL, {id: 'node-2', name: 'Other task'});

const PREVIEW = {
  kind: 'work', prompt_hash: 'p'.repeat(64), char_count: 10, overlay: null,
  blocks: [{text: 'preview block', delivery: 'full',
            sources: [{scope: 'base', source_ref: 'base:work', source_session_id: null}]},
  ],
};

// A page of the ASCENDING /runs pagination (the chronological default, queued
// reservations first): 100 launched runs is exactly one full page — the
// original defect consumed only this page and picked its last snapshot row.
function runRow(i) {
  return {id: 'run-' + String(i).padStart(4, '0'), kind: 'work', state: 'success',
          started_at: '2026-01-01T00:' + String(i % 60).padStart(2, '0') + ':00Z',
          prompt_snapshot_ref: '/x/prompt_snapshot.json'};
}
const ASC_PAGE_100 = {
  items: Array.from({length: 100}, (_v, i) => runRow(i + 1)),
  next_cursor: 'cursor-for-run-0100',
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

function snapshotFor(text) {
  return {prompt_hash: 'h'.repeat(64), char_count: text.length,
          blocks: [{text, delivery: 'full',
                    sources: [{scope: 'node', source_ref: 'prompt_bodies/x.md', source_session_id: 'node-1'}]}]};
}

// Installs fetch handlers that answer BOTH the original one-page shape
// (limit=100, ascending) and the authoritative newest-first shape
// (limit=1&order=desc), so the panel's actual request decides what it shows.
function install(context, opts = {}) {
  const descItems = opts.descItems !== undefined ? opts.descItems : [Object.assign(runRow(150), {})];
  const contexts = opts.contexts || {};
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(opts.detail || DETAIL);
    if (url === '/api/sessions/node-2') return jsonResponse(DETAIL2);
    if (url.startsWith('/api/sessions/node-1/effective-prompt') ||
        url.startsWith('/api/sessions/node-2/effective-prompt')) {
      return jsonResponse(opts.previewError ? {ok: false, status: 500, json: async () => ({detail: 'preview failed'})} : PREVIEW);
    }
    if (url.includes('/runs?') && url.includes('order=desc')) {
      return jsonResponse({items: descItems, next_cursor: null});
    }
    if (url.includes('/runs?limit=100')) return jsonResponse(ASC_PAGE_100);
    const ctx = url.match(/\/runs\/([^/]+)\/context/);
    if (ctx) {
      const answer = contexts[ctx[1]] || {snapshot: snapshotFor('generic snapshot'), legacy_prompt: null};
      if (answer instanceof Object && answer.__status) {
        return {ok: false, status: answer.__status, json: async () => ({detail: answer.detail})};
      }
      return jsonResponse(answer);
    }
    return undefined;
  });
}

test('current run is the authoritative latest started run, selected in one bounded request', async () => {
  const {context, tab} = build();
  const latest = Object.assign(runRow(150), {prompt_snapshot_ref: '/x/prompt_snapshot.json'});
  install(context, {
    descItems: [latest],
    contexts: {'run-0150': {snapshot: snapshotFor('startup instructions generation 0150'), legacy_prompt: null},
               'run-0100': {snapshot: snapshotFor('page-tail generation 0100'), legacy_prompt: null}},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('generation 0150'), 'the whole-history latest run is the current run');
  assert.ok(!text.includes('generation 0100'), 'the ascending first page tail is never presented as current');
  const runsFetches = context.fetchCalls.filter((c) => c.url.includes('/runs?'));
  assert.equal(runsFetches.length, 1, 'selection costs one runs request');
  assert.ok(runsFetches[0].url.includes('order=desc') && runsFetches[0].url.includes('limit=1'),
    'the request is the newest-first one-row read: ' + runsFetches[0].url);
  // A finished latest run is labeled truthfully from its row's derived state.
  assert.ok(text.includes('run-0150') && /run-0150[^\n]*success/.test(text.replace(/\s+/g, ' ')) ||
            text.includes('· success'),
            'the finished latest run carries its outcome label');
});

test('a queued-only session reports no launch and never fetches a snapshot', async () => {
  const {context, tab} = build();
  install(context, {descItems: [{id: 'run-queued', kind: 'work', state: 'queued', started_at: null,
                                 prompt_snapshot_ref: null}]});
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  assert.ok(tab.textContent.includes('No run has started yet'), 'a queued reservation is truthfully not a launch');
  assert.equal(context.fetchCalls.filter((c) => c.url.includes('/context')).length, 0,
    'no snapshot is fetched for a session where nothing has started');
});

test('a never-launched reservation does not replace a real current launch', async () => {
  const {context, tab} = build();
  install(context, {descItems: [Object.assign(runRow(150), {})],
                    contexts: {'run-0150': {snapshot: snapshotFor('startup instructions generation 0150'), legacy_prompt: null}}});
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  assert.ok(tab.textContent.includes('generation 0150'),
    'the latest launched run stays current; queued rows sort behind every launch');
});

test('the latest run with only raw legacy evidence shows limited evidence, never an older snapshot', async () => {
  const {context, tab} = build();
  install(context, {
    descItems: [Object.assign(runRow(150), {prompt_snapshot_ref: '/x/launch_prompt.md'})],
    contexts: {'run-0150': {snapshot: null, legacy_prompt: {
      ref: '/x/launch_prompt.md', sha256: 'abc',
      note: 'raw launch text recorded before the context stage: the managed instructions are visible but per-source provenance was not recorded, so this is limited evidence, not a full snapshot'}},
      'run-0100': {snapshot: snapshotFor('older intact snapshot'), legacy_prompt: null}},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('Limited historical evidence'), 'the latest run owns its evidence class rendering');
  assert.ok(text.includes('limited evidence, not a full snapshot'));
  assert.ok(!text.includes('older intact snapshot'),
    'the panel never substitutes an older well-formed snapshot');
  assert.equal(context.fetchCalls.filter((c) => c.url.includes('/runs/run-0100/context')).length, 0);
});

test('the latest corrupt new snapshot is an explicit error, never an older intact snapshot', async () => {
  const {context, tab} = build();
  install(context, {
    descItems: [Object.assign(runRow(150), {})],
    contexts: {'run-0150': {__status: 500, detail: 'stored prompt snapshot unreadable at /x/prompt_snapshot.json: not json'},
               'run-0100': {snapshot: snapshotFor('older intact snapshot'), legacy_prompt: null}},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('stored prompt snapshot unreadable'), 'the explicit server error surfaces');
  assert.ok(!text.includes('older intact snapshot'), 'no fallback to an older run');
  assert.ok(text.includes('Retry'), 'an actionable retry path is offered');
  assert.equal(context.fetchCalls.filter((c) => c.url.includes('/runs/run-0100/context')).length, 0);
});

test('a prior session error does not leak into a different, no-run session', async () => {
  const {context, tab} = build();
  install(context, {
    descItems: [Object.assign(runRow(150), {})],
    contexts: {'run-0150': {__status: 500, detail: 'stored prompt snapshot unreadable at /x/prompt_snapshot.json: boom'}},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  assert.ok(tab.textContent.includes('stored prompt snapshot unreadable'));
  // Switch to node-2: the desc handler must answer per-session; reuse the
  // generic install by adding a node-2-specific handler ahead of it.
  context.fetchHandlers.unshift((url) => {
    if (url.includes('/api/sessions/node-2/runs') && url.includes('order=desc')) {
      return jsonResponse({items: [{id: 'q-2', kind: 'work', state: 'queued', started_at: null}], next_cursor: null});
    }
    return undefined;
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-2', profile: 'worker'});
  await flush();
  const text = tab.textContent;
  assert.ok(text.includes('No run has started yet'), 'the no-run session says so truthfully');
  assert.ok(!text.includes('stored prompt snapshot unreadable'), 'the prior error is reset');
});

test('the historical run the user selected survives unrelated live updates', async () => {
  const {context, tab} = build();
  install(context, {
    contexts: {'run-old': {snapshot: snapshotFor('selected historical snapshot'), legacy_prompt: null}},
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush();
  await context.TaskContextPanel.showHistoricalRun('run-old');
  await flush();
  assert.ok(tab.textContent.includes('selected historical snapshot'));
  // An unrelated live update for this node (e.g. a rename or sibling event)
  // refreshes the panel data but must not discard the user's selection.
  context.TaskContextPanel.onTreeChanged(['node-1']);
  await flush();
  assert.ok(tab.textContent.includes('Historical run'), 'the historical view survives the refresh');
  assert.ok(tab.textContent.includes('selected historical snapshot'));
});

test('a late runs response from a prior selection never replaces the active view', async () => {
  const {context, tab} = build();
  let resolveSlow;
  const slowDesc = new Promise((resolve) => { resolveSlow = resolve; });
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url === '/api/sessions/node-2') return jsonResponse(DETAIL2);
    if (url.startsWith('/api/sessions/node-1/effective-prompt') ||
        url.startsWith('/api/sessions/node-2/effective-prompt')) return jsonResponse(PREVIEW);
    if (url.includes('/api/sessions/node-1/runs') && url.includes('order=desc')) {
      return slowDesc.then(() => jsonResponse({items: [Object.assign(runRow(150), {})], next_cursor: null}));
    }
    if (url.includes('/api/sessions/node-2/runs') && url.includes('order=desc')) {
      return jsonResponse({items: [{id: 'q-2', kind: 'work', state: 'queued', started_at: null}], next_cursor: null});
    }
    if (url.includes('/runs/run-0150/context')) return jsonResponse({snapshot: snapshotFor('stale latest'), legacy_prompt: null});
    return undefined;
  });
  context.TaskContextPanel.onSessionChanged({id: 'node-1', profile: 'worker'});
  await flush(2);
  context.TaskContextPanel.onSessionChanged({id: 'node-2', profile: 'worker'});
  await flush(3);
  resolveSlow();
  await flush(6);
  const text = tab.textContent;
  assert.ok(text.includes('No run has started yet'), 'the active node truth holds');
  assert.ok(!text.includes('stale latest'), 'the late prior-selection response never lands');
});
