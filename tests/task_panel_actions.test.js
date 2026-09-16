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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: runs, next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    if (url === '/api/sessions/node-1/complete') {
      return {ok: false, status: 409, json: async () => ({detail: {message: 'cannot complete', blockers: ['node-1: has an active run run-live']}})};
    }
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  const overlay = context.__doc.getElementById('task-complete-modal');
  const boxes = overlay.querySelectorAll('input[type="checkbox"]');
  assert.equal(boxes.length, 1, 'only the finished run carries an evidence checkbox');
  assert.ok(overlay.textContent.includes('run-live') && overlay.textContent.includes('not delivery evidence'),
    'the active run is visible but marked not deliverable');
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

test('the completion dialog pages runs newest-first and keeps the selection across loads', async () => {
  const {context} = build();
  const recent = [
    {id: 'nw2', kind: 'work', state: 'success', ended_at: '2026-01-02T00:00:00Z'},
    {id: 'nw1', kind: 'work', state: 'success', ended_at: '2026-01-01T12:00:00Z'},
  ];
  const older = [{id: 'old1', kind: 'work', state: 'success', ended_at: '2026-01-01T00:00:00Z'}];
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.includes('/runs?') && url.includes('order=desc')) {
      call++;
      // Newest-first first page; the cursor page returns the early history.
      if (call === 1) return jsonResponse({items: recent, next_cursor: 'cur-older'});
      return jsonResponse({items: older, next_cursor: null});
    }
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    if (url.endsWith('/complete')) return jsonResponse({id: 'node-1', task_state: 'completed'});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  let overlay = context.__doc.getElementById('task-complete-modal');
  let text = overlay.textContent;
  assert.ok(text.includes('nw2'), 'the newest run renders on the first (desc) page');
  assert.ok(!text.includes('old1'), 'the early history is not silently fetched yet');
  const more = overlay.querySelectorAll('button').find((b) => b.textContent.startsWith('Load older runs'));
  assert.ok(more, 'a visible continuation offers the older page');
  // Select a recent run, then page older: the selection and the input text survive.
  const boxes = overlay.querySelectorAll('input[type="checkbox"]');
  boxes[0].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  overlay.querySelector('#task-complete-summary').value = 'delivery summary draft';
  overlay.querySelector('#task-complete-refs').value = 'evidence/ref.md';
  more.dispatch('click');
  await flush();
  overlay = context.__doc.getElementById('task-complete-modal');
  text = overlay.textContent;
  assert.ok(text.includes('old1'), 'the older page appended under the newer rows');
  assert.ok(text.includes('Selected evidence (1)'), 'the selection stays visible across the page load');
  assert.equal(overlay.querySelector('#task-complete-summary').value, 'delivery summary draft', 'the summary draft survived the page load');
  assert.equal(overlay.querySelector('#task-complete-refs').value, 'evidence/ref.md', 'the refs draft survived the page load');
  // The early run is now checkable too; submit exactly the two picked ids.
  const allBoxes = overlay.querySelectorAll('input[type="checkbox"]');
  assert.equal(allBoxes.length, 3, 'both pages render their eligible runs');
  allBoxes[allBoxes.length - 1].checked = true;
  allBoxes[allBoxes.length - 1].dispatch('change', {target: allBoxes[allBoxes.length - 1]});
  overlay.querySelectorAll('button').find((b) => b.textContent === 'Complete task').dispatch('click');
  await flush();
  const completeCall = context.fetchCalls.find((c) => c.url.endsWith('/complete'));
  const body = JSON.parse(completeCall.opts.body);
  assert.deepEqual(body.run_ids, ['nw2', 'old1'], 'exactly the selected ids are submitted, one from each page');
  assert.equal(body.summary, 'delivery summary draft');
});

test('a failed runs read surfaces as a fetch error with retry, never "No finished runs"', async () => {
  const {context} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.includes('/runs?') && url.includes('order=desc')) return {ok: false, status: 500, json: async () => ({detail: 'boom'})};
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  const overlay = context.__doc.getElementById('task-complete-modal');
  assert.ok(overlay.textContent.includes('Failed to load runs'), 'the failure is named');
  assert.ok(!overlay.textContent.includes('No finished runs yet.'), 'an error is never misreported as an empty history');
  const retry = overlay.querySelectorAll('button').find((b) => b.textContent === 'Retry');
  assert.ok(retry, 'a retry action is offered');
  context.fetchHandlers.splice(0, context.fetchHandlers.length);
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [{id: 'run-ok', kind: 'work', state: 'success'}], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  retry.dispatch('click');
  await flush();
  assert.ok(context.__doc.getElementById('task-complete-modal').textContent.includes('run-ok'),
    'the retry recovers into the real list');
});

test('a paged-out selected run stays visibly selected (chip) across a live refresh', async () => {
  const {context} = build();
  const page1 = [{id: 'run-recent', kind: 'work', state: 'success'}];
  const older = [{id: 'run-far', kind: 'work', state: 'success'}];
  let call = 0;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(DETAIL);
    if (url.includes('/runs?') && url.includes('order=desc')) {
      call++;
      if (call === 1) return jsonResponse({items: page1, next_cursor: 'c2'});
      return jsonResponse({items: older, next_cursor: null});
    }
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  let overlay = context.__doc.getElementById('task-complete-modal');
  overlay.querySelectorAll('button').find((b) => b.textContent.startsWith('Load older runs')).dispatch('click');
  await flush();
  overlay = context.__doc.getElementById('task-complete-modal');
  // Select the OLD run (last checkbox), then take a fresh first page (the
  // live-event path): the old run leaves the first page but stays selected.
  const boxes = overlay.querySelectorAll('input[type="checkbox"]');
  boxes[boxes.length - 1].checked = true;
  boxes[boxes.length - 1].dispatch('change', {target: boxes[boxes.length - 1]});
  await context.TaskPanel.refresh();
  await flush();
  overlay = context.__doc.getElementById('task-complete-modal');
  assert.ok(overlay.textContent.includes('Selected evidence (1)'), 'the chip row still shows the selection');
  assert.ok(overlay.textContent.includes('run-far'.slice(0, 8)), 'the paged-out run id remains visible');
  assert.ok(!overlay.textContent.includes('No finished runs yet.'), 'the refreshed list is not emptied by the merge');
});

test('the move chooser browses open managers across root and child pages and submits the chosen id', async () => {
  const {context} = build();
  // 30 roots, so the roots level needs more than one page of 25.
  const roots = [];
  for (let i = 1; i <= 30; i++) roots.push({id: 'root-' + i, name: 'Root ' + i, profile: 'manager', task_state: 'open', task_parent_id: null, child_count: 0});
  roots[0].child_count = 2; // root-1 has manager children (its own level/pages)
  const kids = [
    {id: 'mid-a', name: 'Mid A', profile: 'manager', task_state: 'open', task_parent_id: 'root-1', child_count: 0},
    {id: 'mid-b', name: 'Mid B', profile: 'manager', task_state: 'open', task_parent_id: 'root-1', child_count: 0},
  ];
  const detail = Object.assign({}, DETAIL, {id: 'mover-1', name: 'Mover', task_parent_id: 'root-2', ancestors: [{id: 'root-2', name: 'Root 2'}]});
  let rootsCall = 0;
  let kidsCall = 0;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/mover-1' && (!url.includes('PATCH'))) return jsonResponse(detail);
    if (url.startsWith('/api/sessions/tree?')) {
      const params = new URLSearchParams(url.split('?')[1]);
      const parent = params.get('parent_id');
      const cursor = params.get('cursor');
      if (!parent) {
        rootsCall++;
        if (rootsCall === 1) return jsonResponse({items: roots.slice(0, 25), next_cursor: 'r2', tree_revision: 'rev'});
        return jsonResponse({items: roots.slice(25), next_cursor: null, tree_revision: 'rev'});
      }
      if (parent === 'root-1') {
        kidsCall++;
        if (!cursor) return jsonResponse({items: kids.slice(0, 1), next_cursor: 'k2', tree_revision: 'rev'});
        return jsonResponse({items: kids.slice(1), next_cursor: null, tree_revision: 'rev'});
      }
      return jsonResponse({items: [], next_cursor: null, tree_revision: 'rev'});
    }
    if (url === '/api/sessions/mover-1' ) return jsonResponse(detail);
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'mover-1', profile: 'manager'});
  await flush();
  context.__doc.getElementById('task-action-move').dispatch('click');
  await flush();
  let overlay = context.__doc.getElementById('task-move-modal');
  assert.ok(overlay, 'the move dialog opened');
  const confirm = overlay.querySelector('#task-move-confirm');
  assert.equal(confirm.disabled, true, 'Move starts disabled: no silent root default while candidates load');
  // Page 1 of the roots is rendered; the rest needs the continuation.
  assert.ok(overlay.textContent.includes('Root 25'), 'the first page renders');
  assert.ok(!overlay.textContent.includes('Root 26'), 'later roots are not silently fetched');
  const more = overlay.querySelectorAll('button').find((b) => b.textContent.startsWith('Load more'));
  assert.ok(more, 'a visible continuation exists for the roots level');
  more.dispatch('click');
  await flush();
  overlay = context.__doc.getElementById('task-move-modal');
  assert.ok(overlay.textContent.includes('Root 30'), 'the later roots page arrived');
  // Expand root-1 (a later interaction): its manager children come as their
  // own paged chain.
  overlay.querySelectorAll('button').find((b) => (b.getAttribute('aria-label') || '').includes("Expand Root 1")).dispatch('click', {stopPropagation: () => {}});
  await flush();
  overlay = context.__doc.getElementById('task-move-modal');
  assert.ok(overlay.textContent.includes('Mid A'), 'the first child page rendered');
  overlay.querySelectorAll('button').find((b) => b.textContent.startsWith('Load more')).dispatch('click');
  await flush();
  overlay = context.__doc.getElementById('task-move-modal');
  assert.ok(overlay.textContent.includes('Mid B'), 'the child level continued past its first page');
  // Choose the intermediate manager Mid B and submit.
  overlay.querySelectorAll('[role="button"]').find((r) => r.textContent.includes('Mid B')).dispatch('click');
  await flush();
  overlay = context.__doc.getElementById('task-move-modal');
  assert.ok(overlay.querySelector('#task-move-chosen').textContent.includes('Mid B'), 'the choice is stated');
  assert.equal(overlay.querySelector('#task-move-confirm').disabled, false, 'an explicit choice enables Move');
  overlay.querySelector('#task-move-confirm').dispatch('click');
  await flush();
  const patch = context.fetchCalls.find((c) => c.opts.method === 'PATCH');
  assert.equal(JSON.parse(patch.opts.body).task_parent_id, 'mid-b', 'the submitted target id is the chosen manager');
  assert.equal(patch.url, '/api/sessions/mover-1', 'the PATCH targets the task the dialog opened for');
});

test('the move chooser makes root an explicit choice and a refusal keeps the intended selection', async () => {
  const {context} = build();
  const detail = Object.assign({}, DETAIL, {id: 'mover-2', name: 'Mover two', task_parent_id: 'root-2', ancestors: [{id: 'root-2', name: 'Root 2'}]});
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/mover-2' && (!opts || opts.method !== 'PATCH')) return jsonResponse(detail);
    if (url.startsWith('/api/sessions/tree?')) {
      const params = new URLSearchParams(url.split('?')[1]);
      if (!params.get('parent_id')) return jsonResponse({items: [{id: 'root-1', name: 'Root 1', profile: 'manager', task_state: 'open', task_parent_id: null, child_count: 0}], next_cursor: null, tree_revision: 'rev'});
      return jsonResponse({items: [], next_cursor: null, tree_revision: 'rev'});
    }
    if (url === '/api/sessions/mover-2' && opts.method === 'PATCH') {
      return {ok: false, status: 409, json: async () => ({detail: {message: 'refused', blockers: ['mover-2: subtree has an active run']}})};
    }
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'mover-2', profile: 'manager'});
  await flush();
  context.__doc.getElementById('task-action-move').dispatch('click');
  await flush();
  const overlay = context.__doc.getElementById('task-move-modal');
  overlay.querySelectorAll('[role="button"]').find((r) => r.textContent.includes('make it a root task')).dispatch('click');
  await flush();
  assert.ok(overlay.querySelector('#task-move-chosen').textContent.includes('root'), 'root is a stated, explicit choice');
  overlay.querySelector('#task-move-confirm').dispatch('click');
  await flush();
  const patch = context.fetchCalls.find((c) => c.opts.method === 'PATCH');
  assert.equal(JSON.parse(patch.opts.body).task_parent_id, null, 'the explicit root choice submits a null parent');
  assert.ok(overlay.textContent.includes('subtree has an active run'), 'the refusal explains itself');
  assert.ok(context.__doc.getElementById('task-move-modal'), 'the dialog stays open on refusal');
  assert.ok(overlay.querySelector('#task-move-chosen').textContent.includes('root'),
    'the intended choice is retained for correction');
  assert.equal(overlay.querySelector('#task-move-confirm').disabled, false, 'the corrected resubmit stays possible');
});

test('search reaches a manager anywhere and marks ineligible hits truthfully', async () => {
  const {context} = build();
  const detail = Object.assign({}, DETAIL, {id: 'mover-3', name: 'Mover three', task_parent_id: null, ancestors: []});
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/mover-3') return jsonResponse(detail);
    if (url.startsWith('/api/sessions/tree?')) return jsonResponse({items: [], next_cursor: null, tree_revision: 'rev'});
    if (url.includes('/tree/search?')) {
      return jsonResponse({items: [
        {row: {id: 'deep-mgr', name: 'Deep manager', profile: 'manager', task_state: 'open', child_count: 0}, ancestors: [{id: 'r', name: 'Root'}, {id: 'm', name: 'Mid'}]},
        {row: {id: 'w-9', name: 'Some worker', profile: 'worker', task_state: 'open', child_count: 0}, ancestors: [{id: 'r', name: 'Root'}]},
        {row: {id: 'mover-3', name: 'Mover three', profile: 'manager', task_state: 'open', child_count: 0}, ancestors: []},
      ], tree_revision: 'rev'});
    }
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'mover-3', profile: 'manager'});
  await flush();
  context.__doc.getElementById('task-action-move').dispatch('click');
  await flush();
  const overlay = context.__doc.getElementById('task-move-modal');
  overlay.querySelector('#task-move-search').value = 'deep';
  overlay.querySelectorAll('button').find((b) => b.textContent === 'Search').dispatch('click');
  await flush();
  const text = overlay.textContent;
  assert.ok(text.includes('Root › Mid › Deep manager'), 'the hit shows its full ancestry path');
  assert.ok(text.includes('worker task'), 'an ineligible hit says why');
  assert.ok(text.includes('this task'), 'the moving task itself is marked');
  overlay.querySelectorAll('[role="button"]').find((r) => r.textContent.includes('Deep manager')).dispatch('click');
  overlay.querySelector('#task-move-confirm').dispatch('click');
  await flush();
  const patch = context.fetchCalls.find((c) => c.opts.method === 'PATCH');
  assert.equal(JSON.parse(patch.opts.body).task_parent_id, 'deep-mgr', 'a search hit submits its real id');
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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
    if (url.includes('/runs?') && url.includes('order=desc')) return jsonResponse({items: [], next_cursor: null});
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

// -- dialog binding: a dialog opened for A never acts on B -------------------

function detailFor(id, name) {
  return Object.assign({}, DETAIL, {id, name, task_parent_id: null, ancestors: []});
}

test('a cancel dialog opened for A is dismissed by a session switch and cannot submit against B', async () => {
  const {context, tab} = build();
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(detailFor('node-1', 'Task A'));
    if (url === '/api/sessions/node-2') return jsonResponse(detailFor('node-2', 'Task B'));
    if (url.includes('/runs?')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  tab.querySelectorAll('button').find((b) => b.textContent === 'Cancel task…').dispatch('click');
  assert.ok(context.__doc.getElementById('task-reason-modal'), 'the reason modal is open for A');
  // The app's real session-switch path calls onSessionChanged for the new node.
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush();
  assert.ok(!context.__doc.getElementById('task-reason-modal'), 'the stale dialog is dismissed, not left aiming at B');
  const posts = context.fetchCalls.filter((c) => c.opts.method === 'POST');
  assert.equal(posts.length, 0, 'no request left the dismissed dialog');
  assert.ok(tab.textContent.includes('Task B'), 'the panel now shows B');
});

test('a late cancel success after a session switch is dropped, and the draft comes back with A', async () => {
  const {context, tab} = build();
  let resolveCancel;
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/node-1' && (!opts || opts.method !== 'POST')) return jsonResponse(detailFor('node-1', 'Task A'));
    if (url === '/api/sessions/node-2') return jsonResponse(detailFor('node-2', 'Task B'));
    if (url === '/api/sessions/node-1/cancel') {
      return new Promise((resolve) => { resolveCancel = () => resolve(jsonResponse({id: 'node-1', task_state: 'cancelled'})); });
    }
    if (url.includes('/runs?')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  tab.querySelectorAll('button').find((b) => b.textContent === 'Cancel task…').dispatch('click');
  const overlay = context.__doc.getElementById('task-reason-modal');
  const reason = overlay.querySelector('#task-reason-input');
  reason.value = 'work is obsolete';
  reason.dispatch('input', {target: reason});
  const confirm = overlay.querySelectorAll('button').find((b) => b.textContent === 'Cancel task');
  confirm.dispatch('click');
  await flush(2);
  // The switch happens while the cancel POST is still in flight.
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush(2);
  assert.ok(!context.__doc.getElementById('task-reason-modal'), 'the modal died with the switch');
  const postsBefore = context.fetchCalls.filter((c) => c.opts.method === 'POST').length;
  resolveCancel();
  await flush(4);
  const postsAfter = context.fetchCalls.filter((c) => c.opts.method === 'POST').length;
  assert.equal(postsAfter, postsBefore, 'the late success triggered no follow-up request');
  assert.equal(context.fetchCalls.filter((c) => c.url.endsWith('/api/sessions/node-2/cancel')).length, 0,
    'no cancel ever targeted B');
  assert.ok(!context._toasts.some((t) => t.includes('refused')), 'no stale refusal toast landed on B');
  // Back on A the draft is restored for the same action.
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  tab.querySelectorAll('button').find((b) => b.textContent === 'Cancel task…').dispatch('click');
  const restored = context.__doc.getElementById('task-reason-modal').querySelector('#task-reason-input');
  assert.equal(restored.value, 'work is obsolete', 'the intended draft returns with its task');
});

test('the completion dialog binds to its task: dismissal on switch, draft/selection restore, B submits to B', async () => {
  const {context} = build();
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/node-1' && (!opts || opts.method !== 'POST')) return jsonResponse(detailFor('node-1', 'Task A'));
    if (url === '/api/sessions/node-2' && (!opts || opts.method !== 'POST')) return jsonResponse(detailFor('node-2', 'Task B'));
    if (url.includes('/runs?')) return jsonResponse({items: [{id: 'run-ev1', kind: 'work', state: 'success'}], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    if (url.endsWith('/complete')) return jsonResponse({id: url.includes('node-2') ? 'node-2' : 'node-1', task_state: 'completed'});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  let overlay = context.__doc.getElementById('task-complete-modal');
  const aSummary = overlay.querySelector('#task-complete-summary');
  aSummary.value = 'A delivery draft';
  aSummary.dispatch('input', {target: aSummary});
  const aRefs = overlay.querySelector('#task-complete-refs');
  aRefs.value = 'a/ref.md';
  aRefs.dispatch('input', {target: aRefs});
  const boxes = overlay.querySelectorAll('input[type="checkbox"]');
  boxes[0].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush();
  assert.ok(!context.__doc.getElementById('task-complete-modal'), 'the completion dialog died with the switch');
  // Reopen on B: no inherited selection, and a submit targets B exactly.
  openComplete(context);
  await flush();
  overlay = context.__doc.getElementById('task-complete-modal');
  assert.equal(overlay.querySelectorAll('input[type="checkbox"]').filter((c) => c.checked).length, 0, 'no evidence carried from A to B');
  assert.equal(overlay.querySelector('#task-complete-summary').value, '', 'no summary carried from A to B');
  overlay.querySelector('#task-complete-summary').value = 'B delivery';
  overlay.querySelector('#task-complete-refs').value = 'b/ref.md';
  overlay.querySelectorAll('button').find((b) => b.textContent === 'Complete task').dispatch('click');
  await flush();
  const complete = context.fetchCalls.find((c) => c.url.endsWith('/complete'));
  assert.equal(complete.url, '/api/sessions/node-2/complete', 'the confirm submits against the dialog\'s own task');
  // Back on A, the draft and the selected evidence return.
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush();
  overlay = context.__doc.getElementById('task-complete-modal');
  assert.equal(overlay.querySelector('#task-complete-summary').value, 'A delivery draft', 'A\'s summary draft is restored');
  assert.ok(overlay.textContent.includes('Selected evidence (1)'), 'A\'s evidence selection is restored');
  assert.ok(overlay.textContent.includes('run-ev1'.slice(0, 8)), 'the selected run id is named');
});

test('the acknowledgement selection and note survive a switch away and back', async () => {
  const {context, tab} = build();
  const pending = [{id: 'input-aaa', type: 'user', text: 'handle me'}, {id: 'input-bbb', type: 'user', text: 'and me'}];
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(detailFor('node-1', 'Task A'));
    if (url === '/api/sessions/node-2') return jsonResponse(detailFor('node-2', 'Task B'));
    if (url.includes('/runs?')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: pending});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const boxes = context.__doc.querySelectorAll('#task-pending-inputs input[type="checkbox"]');
  boxes[0].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  const note = context.__doc.getElementById('task-ack-note');
  note.value = 'handled in the terminal';
  note.dispatch('input', {target: note});
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush();
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const restored = context.__doc.querySelectorAll('#task-pending-inputs input[type="checkbox"]');
  assert.equal(restored.length, 2, 'the pending list re-rendered');
  assert.equal(restored[0].checked, true, 'the selected input is still checked');
  assert.equal(restored[1].checked, false, 'the unselected input stayed unselected');
  const btn = context.__doc.getElementById('task-ack-btn');
  assert.ok(!btn.disabled && btn.textContent.includes('(1)'), 'the ack button kept its enabled count');
  assert.equal(context.__doc.getElementById('task-ack-note').value, 'handled in the terminal', 'the note text survived');
});

test('a pending input that left the pending set is pruned from the selection, not silently submitted', async () => {
  const {context, tab} = build();
  let pending = [{id: 'input-aaa', type: 'user', text: 'handle me'}, {id: 'input-bbb', type: 'user', text: 'and me'}];
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(detailFor('node-1', 'Task A'));
    if (url.includes('/runs?')) return jsonResponse({items: [], next_cursor: null});
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: pending});
    if (url.endsWith('/task-inputs/acknowledge')) return jsonResponse({acknowledged: ['input-bbb']});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  const boxes = context.__doc.querySelectorAll('#task-pending-inputs input[type="checkbox"]');
  boxes[0].checked = true; boxes[1].checked = true;
  boxes[0].dispatch('change', {target: boxes[0]});
  boxes[1].dispatch('change', {target: boxes[1]});
  // A live refresh reports input-aaa no longer pending (handled elsewhere).
  pending = [{id: 'input-bbb', type: 'user', text: 'and me'}];
  await context.TaskPanel.refresh();
  await flush();
  const ackBtn = context.__doc.getElementById('task-ack-btn');
  assert.ok(ackBtn.textContent.includes('(1)'), 'the vanished input left the selection');
  ackBtn.dispatch('click');
  await flush();
  const ackCall = context.fetchCalls.find((c) => c.url.endsWith('/task-inputs/acknowledge'));
  assert.deepEqual(JSON.parse(ackCall.opts.body).input_ids, ['input-bbb'], 'only the still-pending id is submitted');
});

test('a queued stale evidence page issues no cross-session read or render after a session switch', async () => {
  // The completion chain serializes pages: a live refresh fires item #2 while
  // page #1 is still in flight. If the switch lands before #1 resolves, #2
  // dequeues with the old flight — it must bail at the head: no read for the
  // new task, no render of the orphaned collection into the new dialog.
  const {context} = build();
  let resolveRuns;
  context.fetchHandlers.push((url) => {
    if (url === '/api/sessions/node-1') return jsonResponse(detailFor('node-1', 'Task A'));
    if (url === '/api/sessions/node-2') return jsonResponse(detailFor('node-2', 'Task B'));
    if (url.includes('/runs?')) {
      return new Promise((resolve) => { resolveRuns = () => resolve(jsonResponse({items: [], next_cursor: null})); });
    }
    if (url.endsWith('/task-inputs/pending')) return jsonResponse({items: []});
    return undefined;
  });
  context.TaskPanel.onSessionChanged({id: 'node-1', profile: 'manager'});
  await flush();
  openComplete(context);
  await flush(2);
  // Live refresh while page #1 is in flight queues chain item #2.
  context.TaskPanel.refresh();
  await flush(2);
  // The switch lands before page #1 resolves.
  context.TaskPanel.onSessionChanged({id: 'node-2', profile: 'manager'});
  await flush(2);
  const runsBefore = context.fetchCalls.filter((c) => c.url.includes('/runs?')).length;
  resolveRuns();
  await flush(8);
  const runsAfter = context.fetchCalls.filter((c) => c.url.includes('/runs?')).length;
  assert.equal(runsAfter, runsBefore, 'the queued stale page issued no read after the switch');
  assert.ok(!context.__doc.getElementById('task-complete-modal'), 'the old dialog stays dismissed');
});

// ---------------------------------------------------------------------------
// The primary New Task action's direct root create (no form)
// ---------------------------------------------------------------------------

test('createRootTask posts the direct root create: empty task, default name and backend', async () => {
  const {context} = build();
  let created = 0;
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      created++;
      return jsonResponse({id: 'root-' + created, name: 'New manager task', profile: 'manager'});
    }
    return undefined;
  });
  const first = await context.TaskPanel.createRootTask();
  const second = await context.TaskPanel.createRootTask();
  const posts = context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST');
  assert.equal(posts.length, 2);
  const b1 = JSON.parse(posts[0].opts.body);
  const b2 = JSON.parse(posts[1].opts.body);
  assert.equal(b1.task_parent_id, null, 'the primary action creates one root manager');
  assert.equal(b1.profile, 'manager');
  assert.deepEqual(b1.task, {goal: '', acceptance: [], context_refs: []}, 'empty task instructions');
  assert.equal(b1.name, undefined, 'no name override: the server default title applies');
  assert.equal(b1.backend, undefined, 'no backend override: the server default resolution applies');
  assert.ok(b1.request_id, 'the create carries a request id');
  assert.notEqual(b2.request_id, b1.request_id, 'a completed create never leaks its id into the next action');
  assert.equal(first.id, 'root-1');
  assert.equal(second.id, 'root-2');
});

test('the creation toolbar selected model is captured with the request id and never switches on retry', async () => {
  const {context, doc} = build();
  const backendSelect = doc.createElement('select');
  backendSelect.id = 'new-session-backend';
  backendSelect.value = 'clc-second';
  doc.register(backendSelect);
  let attempts = 0;
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      attempts++;
      if (attempts === 1) {
        return {ok: false, status: 500, json: async () => ({detail: 'provider down'})};
      }
      if (attempts === 2) {
        return jsonResponse({id: 'root-chosen', profile: 'manager', backend: 'clc-second'});
      }
      return jsonResponse({id: 'root-next', profile: 'manager', backend: 'clc-third'});
    }
    return undefined;
  });
  await assert.rejects(() => context.TaskPanel.createRootTask(), /provider down/);
  // The user drags the dropdown elsewhere while the failed action is pending.
  backendSelect.value = 'clc-third';
  const meta = await context.TaskPanel.createRootTask();
  const posts = context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST');
  assert.equal(posts.length, 2);
  const b1 = JSON.parse(posts[0].opts.body);
  const b2 = JSON.parse(posts[1].opts.body);
  assert.equal(b1.backend, "clc-second", "the dropdown value rides the create");
  assert.equal(b2.backend, "clc-second", "the retry replays the captured choice, not the dropdown current one");
  assert.equal(b2.request_id, b1.request_id, 'one pending action, one request id');
  assert.equal(meta.backend, 'clc-second');
  // The next action starts fresh with the dropdown's current value.
  const next = await context.TaskPanel.createRootTask();
  const third = JSON.parse(
    context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST')[2].opts.body);
  assert.equal(third.backend, 'clc-third', 'a completed action starts a fresh capture');
  assert.notEqual(third.request_id, b1.request_id);
  assert.equal(next.backend, 'clc-third');
});

test('without the dropdown in the page the create carries no backend field', async () => {
  const {context} = build();
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      return jsonResponse({id: 'root-plain', profile: 'manager'});
    }
    return undefined;
  });
  await context.TaskPanel.createRootTask();
  const body = JSON.parse(
    context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST')[0].opts.body);
  assert.equal(body.backend, undefined, "the server default resolution applies");
});

test('rapid create calls share one request id while the first is in flight', async () => {
  const {context} = build();
  const resolvers = [];
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      return new Promise((resolve) => resolvers.push(() => resolve(jsonResponse({id: 'root-shared', profile: 'manager'}))));
    }
    return undefined;
  });
  const first = context.TaskPanel.createRootTask();
  const second = context.TaskPanel.createRootTask(); // fires before the first resolves
  await flush(2);
  const posts = context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST');
  assert.equal(posts.length, 2);
  const b1 = JSON.parse(posts[0].opts.body);
  const b2 = JSON.parse(posts[1].opts.body);
  assert.equal(b2.request_id, b1.request_id,
    'the pending action keeps one request id: the server binds (parent, request_id) to one node');
  for (const resolve of resolvers) resolve();
  const [r1, r2] = await Promise.all([first, second]);
  assert.equal(r1.id, 'root-shared');
  assert.equal(r2.id, 'root-shared', 'the replay returns the original product');
});

test('a failed create keeps the request id and surfaces the server detail; the retry replays it', async () => {
  const {context} = build();
  let attempts = 0;
  context.fetchHandlers.push((url, opts) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      attempts++;
      if (attempts === 1) {
        return {ok: false, status: 409, json: async () => ({detail: {message: 'task tree changed', blockers: ['b1']}})};
      }
      return jsonResponse({id: 'root-retry', profile: 'manager'});
    }
    return undefined;
  });
  await assert.rejects(() => context.TaskPanel.createRootTask(), /task tree changed/);
  const meta = await context.TaskPanel.createRootTask();
  const posts = context.fetchCalls.filter((c) => c.url === '/api/sessions/' && c.opts.method === 'POST');
  assert.equal(posts.length, 2);
  const b1 = JSON.parse(posts[0].opts.body);
  const b2 = JSON.parse(posts[1].opts.body);
  assert.equal(b2.request_id, b1.request_id, 'the retry replays the failed attempt\u2019s request id');
  assert.equal(meta.id, 'root-retry');
});
