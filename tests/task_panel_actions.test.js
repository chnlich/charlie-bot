const assert = require('node:assert/strict');
const test = require('node:test');

const {buildContext, loadModules, jsonResponse} = require('./task_ui_context_stub');

const DETAIL = {
  id: 'node-1',
  name: 'Feature manager',
  profile: 'manager',
  task_parent_id: 'root-1',
  task_state: 'open',
  work_state: 'running',
  archived: false,
  automation_paused: false,
  presentation: 'auto',
  ancestors: [{id: 'root-1', name: 'Program'}],
  prompt_rules: {subtree: {ref: null, source: null, chars: 0, text: null}, node: {ref: null, source: null, chars: 0, text: null}, affected_descendants: 2},
  task: {
    goal: 'Deliver the feature',
    acceptance: ['tests pass'],
    context_refs: ['docs/spec.md'],
    repo_path: '/repo/x',
    base_branch: 'main',
    task_type: 'implement',
    keep_worktree: false,
  },
};

function build(overrides = {}) {
  const context = buildContext(overrides);
  loadModules(context, ['task-panel.js']);
  const doc = context.__doc;
  const tab = doc.createElement('div');
  tab.id = 'tab-task';
  doc.body.appendChild(tab);
  return {context, doc, tab};
}

async function flush(rounds = 10) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

function shownPatchBodies(context) {
  return context.fetchCalls
    .filter((c) => c.opts.method === 'PATCH')
    .map((c) => JSON.parse(c.opts.body));
}

test('the panel renders the canonical task object into the editor', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager', name: 'Feature manager'});
  await flush();
  assert.equal(document_activeValue(context, 'task-goal-input'), 'Deliver the feature');
  assert.equal(document_activeValue(context, 'task-acceptance-input'), 'tests pass');
  assert.equal(document_activeValue(context, 'task-context-refs-input'), 'docs/spec.md');
  assert.equal(document_activeValue(context, 'task-repo-input'), '/repo/x');
  assert.equal(document_activeValue(context, 'task-type-select'), 'implement');
  assert.ok(tab.textContent.includes('Manager'));
  assert.ok(tab.textContent.includes('Program'), 'the parent chain names the parent');
  assert.ok(tab.textContent.includes('running'));
});

function document_activeValue(context, id) {
  return context.__doc.getElementById(id).value;
}

test('Save task PATCHes the canonical task record and clears the draft', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const goal = context.__doc.getElementById('task-goal-input');
  goal.value = 'Deliver the feature, revised';
  context.TaskPanel.refresh(); // not needed; direct save below
  const saveBtn = context.__doc.getElementById('task-save-btn');
  saveBtn.dispatch('click');
  await flush();
  const patch = shownPatchBodies(context).find((b) => b.task);
  assert.ok(patch, 'a task PATCH was sent');
  assert.equal(patch.task.goal, 'Deliver the feature, revised');
  assert.deepEqual(patch.task.acceptance, ['tests pass']);
  assert.equal(patch.task.repo_path, '/repo/x');
  assert.equal(patch.task.base_branch, 'main');
  assert.equal(patch.task.task_type, 'implement');
  assert.equal(patch.task.keep_worktree, false);
  assert.equal(context.localStorage.store.get('charliebot-task-draft-node-1'), undefined,
    'a saved task leaves no draft behind');
});

test('a refused save keeps the draft and surfaces the blockers', async () => {
  const {context, tab} = build();
  let detailReads = 0;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') {
      detailReads++;
      return jsonResponse(DETAIL);
    }
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const goal = context.__doc.getElementById('task-goal-input');
  goal.value = 'edited goal not yet saved';
  // The editor's input event persists the draft.
  goal.dispatch('input', {target: goal});
  assert.ok(context.localStorage.store.get('charliebot-task-draft-node-1'), 'the draft persisted');

  context.fetchHandlers.length = 1; // keep only the detail handler? no: rebuild below
  context.fetchHandlers.splice(0, context.fetchHandlers.length);
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/node-1' && (!opts || opts.method !== 'PATCH')) return jsonResponse(DETAIL);
    if (url === '/api/sessions/node-1' && opts.method === 'PATCH') {
      return {ok: false, status: 409, json: async () => ({detail: {message: 'task has an active run', blockers: ['node-1: run run-1 is running']}})};
    }
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  const saveBtn = context.__doc.getElementById('task-save-btn');
  saveBtn.dispatch('click');
  await flush();
  assert.ok(tab.textContent.includes('node-1: run run-1 is running'), 'blockers are listed');
  // The refused edit is still in the editor and still stored as the draft.
  assert.equal(context.__doc.getElementById('task-goal-input').value, 'edited goal not yet saved');
  assert.ok(context.localStorage.store.get('charliebot-task-draft-node-1'), 'the draft survived the refusal');
});

test('unsaved drafts are restored on re-render (tab switch / node switch back)', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const goal = context.__doc.getElementById('task-goal-input');
  goal.value = 'draft across switches';
  goal.dispatch('input', {target: goal});
  // Leave (different node) and come back.
  context.TaskPanel.onSessionChanged(null);
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  assert.equal(context.__doc.getElementById('task-goal-input').value, 'draft across switches');
  assert.ok(context.__doc.getElementById('tab-task').textContent.includes('Unsaved draft restored'));
});

test('child creation posts the structural API with a stable request id across rapid clicks', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    if (url === '/api/sessions/') {
      return jsonResponse({id: 'child-9', name: 'Child', profile: 'worker'});
    }
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  context.TaskPanel.openChildModal('node-1');
  const overlay = context.__doc.getElementById('task-child-modal');
  assert.ok(overlay, 'the child modal opened');
  const goal = overlay.querySelector('#task-child-goal') || findIn(overlay, 'task-child-goal');
  goal.value = 'child goal';
  const profileSelect = overlay.querySelector('#task-child-profile') || findIn(overlay, 'task-child-profile');
  profileSelect.value = 'worker';
  const createBtn = findCreateButton(overlay);
  createBtn.dispatch('click');
  await flush();
  const post = context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST');
  assert.equal(post.length, 1, 'one create request');
  const body = JSON.parse(post[0].opts.body);
  assert.equal(body.task_parent_id, 'node-1');
  assert.equal(body.profile, 'worker');
  assert.equal(body.task.goal, 'child goal');
  assert.ok(body.request_id, 'the create carries a request id');
});

test('child creation refuses an empty goal client-side without a request', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  context.TaskPanel.openChildModal('node-1');
  const overlay = context.__doc.getElementById('task-child-modal');
  const createBtn = findCreateButton(overlay);
  createBtn.dispatch('click');
  await flush();
  const posts = context.fetchCalls.filter((c) => c.url === '/api/sessions/');
  assert.equal(posts.length, 0, 'no request without a goal');
  assert.ok(overlay.textContent.includes('Goal is required.'));
});

function findIn(root, id) {
  if (root.id === id) return root;
  for (const child of root.children) {
    const hit = findIn(child, id);
    if (hit) return hit;
  }
  return null;
}

function findCreateButton(overlay) {
  return overlay.querySelectorAll('button').find((b) => b.textContent === 'Create subtask');
}

test('pending inputs: acknowledgement sends the exact selected ids with the note', async () => {
  const {context, tab} = build();
  const pending = [
    {id: 'input-aaa', type: 'user', timestamp: '2026-01-01T00:00:00Z', actor: 'user', text: 'first instruction'},
    {id: 'input-bbb', type: 'agent_message', timestamp: '2026-01-01T00:01:00Z', actor: 'agent', from_session_name: 'Child task', text: 'child report body'},
  ];
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: pending});
    if (url.endsWith('/task-inputs/acknowledge')) return jsonResponse({acknowledged: ['input-aaa']});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  assert.ok(tab.textContent.includes('first instruction'));
  assert.ok(tab.textContent.includes('from Child task'));
  const ackBtn = context.__doc.getElementById('task-ack-btn');
  assert.equal(ackBtn.disabled, true, 'acknowledge is disabled without a selection');
  const boxes = findIn(tab, 'task-pending-inputs').querySelectorAll('input[type="checkbox"]');
  boxes[0].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  assert.equal(ackBtn.disabled, false);
  ackBtn.dispatch('click');
  await flush();
  const ackCall = context.fetchCalls.find((c) => c.url.endsWith('/task-inputs/acknowledge'));
  assert.ok(ackCall, 'the acknowledgement was sent');
  const body = JSON.parse(ackCall.opts.body);
  assert.deepEqual(body.input_ids, ['input-aaa'], 'exactly the selected ids');
  assert.ok(typeof body.request_id === 'string' && body.request_id);
});

test('Complete collects the selected finished run ids and the server blockers surface', async () => {
  const {context} = build();
  const runs = [
    {id: 'run-done-1', kind: 'work', state: 'success', started_at: '2026-01-01T00:00:00Z', ended_at: '2026-01-01T00:01:00Z', input_event_ids: ['i1']},
    {id: 'run-live', kind: 'work', state: 'running', input_event_ids: []},
  ];
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: runs, next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    if (url === '/api/sessions/node-1/complete') {
      return {ok: false, status: 409, json: async () => ({detail: {message: 'cannot complete', blockers: ['node-1: has an active run run-live']}})};
    }
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  const overlay = context.__doc.getElementById('task-complete-modal');
  const boxes = overlay.querySelectorAll('input[type="checkbox"]');
  assert.equal(boxes.length, 1, 'only the finished run is offered as evidence');
  boxes[0].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  const confirm = overlay.querySelectorAll('button').find((b) => b.textContent === 'Complete task');
  confirm.dispatch('click');
  await flush();
  const completeCall = context.fetchCalls.find((c) => c.url.endsWith('/complete'));
  const body = JSON.parse(completeCall.opts.body);
  assert.deepEqual(body.run_ids, ['run-done-1'], 'the picked run id rides the completion claim');
  // The refusal surfaces in the still-open modal with the concrete blocker,
  // and the modal (and its unsent draft) stays up.
  assert.ok(overlay.textContent.includes('run-live'), 'the 409 blocker names the active run');
  assert.ok(context.__doc.getElementById('task-complete-modal'), 'the modal stays open on refusal');
});

function openComplete(context) {
  const tab = context.__doc.getElementById('tab-task');
  const btn = tab.querySelectorAll('button').find((b) => b.textContent === 'Complete…');
  btn.dispatch('click');
}

test('a late detail response for a prior node never replaces the active node\'s editor', async () => {
  const {context} = build();
  let resolveSlow;
  const slowDetail = new Promise((resolve) => { resolveSlow = resolve; });
  const otherDetail = Object.assign({}, DETAIL, {id: 'node-2', name: 'Other node'});
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return slowDetail.then(() => jsonResponse(DETAIL));
    if (url === '/api/sessions/node-2') return jsonResponse(otherDetail);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  // Switch to node-1 (slow), then rapidly to node-2 (fast).
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush(2);
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush(3);
  assert.equal(document_activeValue(context, 'task-goal-input'), 'Deliver the feature');
  resolveSlow();
  await flush(5);
  // The slow node-1 response landed after the switch: the editor still shows node-2.
  assert.equal(context.__doc.getElementById('task-goal-input').value, 'Deliver the feature');
  assert.ok(context.__doc.getElementById('tab-task').textContent.includes('Other node'));
});

test('pause/resume, presentation and role actions PATCH their specific fields', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const tab = context.__doc.getElementById('tab-task');
  const pauseBtn = tab.querySelectorAll('button').find((b) => b.textContent === 'Pause automation');
  pauseBtn.dispatch('click');
  await flush();
  const patch = shownPatchBodies(context).find((b) => b.automation_paused === true);
  assert.ok(patch, 'pause PATCHes automation_paused');
  const promote = tab.querySelectorAll('button').find((b) => b.textContent === 'Demote to worker');
  promote.dispatch('click');
  await flush();
  assert.ok(shownPatchBodies(context).some((b) => b.profile === 'worker'), 'role change PATCHes profile');
});

test('a refused action render keeps the pending-inputs box (the ack path) visible', async () => {
  const {context, tab} = build();
  const pending = [{id: 'input-aaa', type: 'user', timestamp: '2026-01-01T00:00:00Z', actor: 'user', text: 'pending work'}];
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/node-1' && (!opts || opts.method !== 'PATCH')) return jsonResponse(DETAIL);
    if (url === '/api/sessions/node-1' && opts.method === 'PATCH') {
      return {ok: false, status: 409, json: async () => ({detail: {message: 'refused', blockers: ['node-1: run run-1 is running']}})};
    }
    if (url.endsWith('/runs?limit=100')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: pending});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager', name: 'Feature manager'});
  await flush();
  assert.ok(context.__doc.getElementById('task-pending-inputs'), 'pending inputs render after the initial load');
  // A refused action re-renders from the same facts without healing fetches:
  // the pending-inputs box (the actionable ack path) must survive the render.
  const goal = context.__doc.getElementById('task-goal-input');
  goal.value = 'another unsaved edit';
  goal.dispatch('input', {target: goal});
  context.__doc.getElementById('task-save-btn').dispatch('click');
  await flush();
  assert.ok(tab.textContent.includes('run run-1 is running'), 'the refusal surfaces');
  assert.ok(context.__doc.getElementById('task-pending-inputs'), 'the ack path stays visible after a refused-action render');
  assert.ok(tab.textContent.includes('pending work'), 'the pending input is still listed');
});
