(function() {
// ---------------------------------------------------------------------------
// Task panel — the v2 task node's canonical task object and operator actions
// ---------------------------------------------------------------------------
// Edits GET/PATCH /api/sessions/{id}'s task record (goal, acceptance,
// context_refs, repo/base/task_type/keep_worktree) and drives the structural
// actions (child create, move, role change, pause/resume, complete, cancel,
// reopen, presentation, input acknowledgement). Every mutation carries a
// stable per-action request id; every response is tied to the session it was
// issued for, so a late answer for a prior node can never land here.

const DRAFT_PREFIX = 'charliebot-task-draft-';

const panel = {
  sessionId: null,
  gen: 0,             // per-session request generation
  detail: null,       // latest SessionDetailResponse
  runs: [],           // latest runs page (for the completion evidence picker)
  pendingInputs: [],  // latest pending-input projection
  selection: {
    childParentId: null,
    moveTargetId: null,
    ackIds: new Set(),
    completeRunIds: new Set(),
  },
  actionRequests: new Map(), // actionKey -> request_id (stable per logical action)
};

// The panel binds to the session it was shown for: every fetch carries that
// binding and a generation, so a late response for a prior node can never
// replace the active node's editor, run list or header.
function boundSessionId() {
  return panel.sessionId;
}

function isStale(flight) {
  return flight.gen !== panel.gen || flight.sessionId !== panel.sessionId;
}

function requestIdFor(actionKey) {
  // One logical user action keeps one request id across retries and replays:
  // a double-click or a retry-after-409 replays the same operation instead of
  // creating a second one. A NEW user action (button re-enabled after success)
  // starts a fresh id.
  if (!panel.actionRequests.has(actionKey)) {
    panel.actionRequests.set(actionKey, (crypto.randomUUID ? crypto.randomUUID() : String(Date.now()) + '-' + Math.random()));
  }
  return panel.actionRequests.get(actionKey);
}

function clearRequestId(actionKey) {
  panel.actionRequests.delete(actionKey);
}

// -- drafts ------------------------------------------------------------------

function draftKey(sessionId) {
  return DRAFT_PREFIX + sessionId;
}

function loadDraft(sessionId) {
  try {
    const raw = localStorage.getItem(draftKey(sessionId));
    return raw ? JSON.parse(raw) : null;
  } catch (_err) {
    return null;
  }
}

function saveDraft(sessionId, draft) {
  try {
    if (draft) localStorage.setItem(draftKey(sessionId), JSON.stringify(draft));
    else localStorage.removeItem(draftKey(sessionId));
  } catch (err) {
    console.error('saveDraft failed:', err);
  }
}

function readEditorDraft() {
  const get = (id) => {
    const el = document.getElementById(id);
    return el ? el.value : null;
  };
  const lines = (id) => (get(id) || '').split('\n').map((s) => s.trim()).filter(Boolean);
  const draft = {
    goal: get('task-goal-input') || '',
    acceptance: lines('task-acceptance-input'),
    context_refs: lines('task-context-refs-input'),
    repo_path: get('task-repo-input') || '',
    base_branch: get('task-base-branch-input') || '',
    task_type: get('task-type-select') || '',
    keep_worktree: !!(document.getElementById('task-keep-worktree') || {}).checked,
  };
  return draft;
}

function draftDiffersFromDetail(draft) {
  const task = (panel.detail && panel.detail.task) || {};
  const norm = (v) => (v || '');
  return draft.goal !== norm(task.goal)
    || JSON.stringify(draft.acceptance) !== JSON.stringify(task.acceptance || [])
    || JSON.stringify(draft.context_refs) !== JSON.stringify(task.context_refs || [])
    || draft.repo_path !== norm(task.repo_path)
    || draft.base_branch !== norm(task.base_branch)
    || draft.task_type !== norm(task.task_type)
    || draft.keep_worktree !== !!task.keep_worktree;
}

// -- data --------------------------------------------------------------------

async function refresh() {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const flight = {sessionId: panel.sessionId, gen: ++panel.gen};
  try {
    const res = await fetch('/api/sessions/' + sessionId);
    if (!res.ok) throw new Error('task detail failed: ' + res.status);
    const detail = await res.json();
    if (isStale(flight)) return; // a late answer never replaces the active node's editor
    panel.detail = detail;
    render();
    // The runs page and pending inputs feed the completion/acknowledgement
    // pickers; both are read-only projections of server facts.
    fetch('/api/sessions/' + sessionId + '/runs?limit=100').then((r) => (r.ok ? r.json() : {items: []}))
      .then((page) => { if (!isStale(flight)) { panel.runs = page.items || []; renderRunsPicker(); } })
      .catch((err) => console.error('runs fetch failed:', err));
    fetch('/api/sessions/' + sessionId + '/task-inputs/pending').then((r) => (r.ok ? r.json() : {items: []}))
      .then((page) => { if (!isStale(flight)) { panel.pendingInputs = page.items || []; renderPendingInputs(); } })
      .catch((err) => console.error('pending inputs fetch failed:', err));
  } catch (err) {
    if (isStale(flight)) return;
    renderError('Failed to load task: ' + (err && err.message ? err.message : err));
  }
}

function renderError(message) {
  const container = document.getElementById('tab-task');
  if (!container) return;
  container.textContent = '';
  const box = document.createElement('div');
  box.className = 'mx-auto max-w-2xl p-4';
  const err = document.createElement('div');
  err.className = 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-sm px-4 py-3';
  err.textContent = message;
  box.appendChild(err);
  container.appendChild(box);
}

// -- rendering ---------------------------------------------------------------

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function sectionTitle(text) {
  return el('h3', 'text-xs font-semibold uppercase tracking-wide text-slate-400 mb-2', text);
}

function badge(text, className) {
  return el('span', 'text-[11px] rounded px-1.5 py-0.5 border whitespace-nowrap ' + (className || 'border-slate-600 text-slate-300'), text);
}

function stateBadgeClass(value) {
  if (value === 'running') return 'border-blue-500/50 text-blue-300 bg-blue-500/10';
  if (value === 'attention') return 'border-red-500/50 text-red-300 bg-red-500/10';
  if (value === 'waiting') return 'border-amber-500/50 text-amber-300 bg-amber-500/10';
  if (value === 'completed') return 'border-green-500/50 text-green-300 bg-green-500/10';
  if (value === 'cancelled') return 'border-slate-500 text-slate-400 bg-slate-500/10';
  return 'border-slate-600 text-slate-300';
}

function render() {
  const container = document.getElementById('tab-task');
  if (!container) return;
  const detail = panel.detail;
  if (!detail) return;
  container.textContent = '';
  const wrap = el('div', 'mx-auto max-w-3xl p-4 space-y-4');

  wrap.appendChild(renderHeader(detail));
  wrap.appendChild(renderBlockers());
  wrap.appendChild(renderTaskForm(detail));
  wrap.appendChild(renderActions(detail));
  // Pending inputs insert themselves after the actions box (or no-op when
  // none); the runs picker refreshes the completion modal in place.
  renderPendingInputs();
  container.appendChild(wrap);
}

function renderHeader(detail) {
  const head = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-2');
  const titleRow = el('div', 'flex items-center gap-2 flex-wrap');
  titleRow.appendChild(el('span', 'text-base font-semibold text-slate-100 break-all', detail.name));
  titleRow.appendChild(badge(detail.profile === 'manager' ? 'Manager' : 'Worker', 'border-blue-500/40 text-blue-300'));
  titleRow.appendChild(badge('task: ' + detail.task_state, stateBadgeClass(detail.task_state)));
  titleRow.appendChild(badge('work: ' + detail.work_state, stateBadgeClass(detail.work_state)));
  if (detail.archived) titleRow.appendChild(badge('archived', 'border-slate-600 text-slate-400'));
  if (detail.automation_paused) titleRow.appendChild(badge('automation paused', 'border-amber-500/50 text-amber-300'));
  if (detail.presentation && detail.presentation !== 'auto') {
    titleRow.appendChild(badge('presentation: ' + detail.presentation, 'border-slate-600 text-slate-400'));
  }
  head.appendChild(titleRow);

  const ancestors = detail.ancestors || [];
  if (ancestors.length) {
    const path = el('div', 'text-xs text-slate-400 flex flex-wrap items-center gap-1');
    path.appendChild(el('span', undefined, 'Parent:'));
    for (const a of ancestors) {
      const link = el('a', 'text-blue-400 hover:text-blue-300', a.name);
      link.href = '/?session=' + encodeURIComponent(a.id);
      link.addEventListener('click', (e) => { e.preventDefault(); switchSession(a.id); });
      path.appendChild(link);
      path.appendChild(el('span', 'text-slate-600', '·'));
    }
    path.appendChild(el('span', 'text-slate-500', detail.name));
    head.appendChild(path);
  } else {
    head.appendChild(el('div', 'text-xs text-slate-500', 'Root task'));
  }
  return head;
}

function renderBlockers() {
  const box = document.getElementById('task-blockers');
  if (box) box.remove();
  const blockers = panel.lastBlockers || [];
  if (!blockers.length) return document.createDocumentFragment();
  const wrap = el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-sm px-4 py-3 space-y-1', '');
  wrap.id = 'task-blockers';
  wrap.appendChild(el('p', 'font-semibold', 'The last action was refused:'));
  for (const b of blockers) wrap.appendChild(el('p', 'break-words', b));
  return wrap;
}

function fieldRow(labelText, inputEl) {
  const wrap = el('div');
  const label = el('label', 'block text-xs text-slate-400 mb-1', labelText);
  label.htmlFor = inputEl.id;
  wrap.appendChild(label);
  wrap.appendChild(inputEl);
  return wrap;
}

function inputEl(id, value, placeholder, multiline) {
  const node = multiline ? el('textarea') : el('input');
  if (!multiline) node.type = 'text';
  node.id = id;
  node.value = value || '';
  node.placeholder = placeholder || '';
  node.className = 'w-full bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200 placeholder-slate-500 focus:outline-none focus:border-blue-500' + (multiline ? ' resize-y' : '');
  if (multiline) node.rows = 3;
  node.addEventListener('input', onEditorInput);
  return node;
}

function renderTaskForm(detail) {
  const task = detail.task || {};
  const draft = loadDraft(detail.id);
  const values = draft || {
    goal: task.goal || '',
    acceptance: (task.acceptance || []).join('\n'),
    context_refs: (task.context_refs || []).join('\n'),
    repo_path: task.repo_path || '',
    base_branch: task.base_branch || '',
    task_type: task.task_type || '',
    keep_worktree: !!task.keep_worktree,
  };
  const box = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-3');

  const goal = inputEl('task-goal-input', values.goal, 'Task goal', true);
  const acceptance = inputEl('task-acceptance-input', values.acceptance, 'One acceptance condition per line', true);
  const refs = inputEl('task-context-refs-input', values.context_refs, 'One background material reference per line', true);
  const repo = inputEl('task-repo-input', values.repo_path, 'Repository path (optional)');
  const base = inputEl('task-base-branch-input', values.base_branch, 'Base branch (optional)');
  const typeSelect = el('select');
  typeSelect.id = 'task-type-select';
  typeSelect.className = 'bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200 focus:outline-none focus:border-blue-500';
  for (const opt of [['', 'None'], ['implement', 'implement'], ['quick-edit', 'quick-edit'], ['script-run', 'script-run'], ['verify', 'verify']]) {
    const o = el('option', undefined, opt[1]);
    o.value = opt[0];
    if (values.task_type === opt[0]) o.selected = true;
    typeSelect.appendChild(o);
  }
  typeSelect.addEventListener('change', onEditorInput);
  const keep = el('input');
  keep.type = 'checkbox';
  keep.id = 'task-keep-worktree';
  keep.className = 'accent-blue-500';
  keep.checked = !!values.keep_worktree;
  keep.addEventListener('change', onEditorInput);
  const keepLabel = el('label', 'flex items-center gap-2 text-xs text-slate-400 cursor-pointer select-none');
  keepLabel.appendChild(keep);
  keepLabel.appendChild(el('span', undefined, 'Keep worktree after delivery'));

  box.appendChild(sectionTitle('Task'));
  box.appendChild(fieldRow('Goal', goal));
  box.appendChild(fieldRow('Acceptance conditions', acceptance));
  box.appendChild(fieldRow('Context references', refs));

  const grid = el('div', 'grid grid-cols-1 sm:grid-cols-2 gap-3');
  grid.appendChild(fieldRow('Repo', repo));
  grid.appendChild(fieldRow('Base branch', base));
  grid.appendChild(fieldRow('Task type', typeSelect));
  box.appendChild(grid);
  box.appendChild(keepLabel);

  const footer = el('div', 'flex items-center gap-3 flex-wrap');
  const save = el('button', 'px-4 py-2 rounded-lg bg-blue-600 hover:bg-blue-500 text-sm font-medium text-white transition-colors', 'Save task');
  save.id = 'task-save-btn';
  save.addEventListener('click', () => saveTask());
  footer.appendChild(save);
  const hint = el('span', 'text-xs text-slate-500', 'Task text is separate from prompts; agents read it at launch.');
  footer.appendChild(hint);
  if (draft) footer.appendChild(el('span', 'text-xs text-amber-300', 'Unsaved draft restored'));
  box.appendChild(footer);
  return box;
}

function onEditorInput() {
  // Persist the unsaved task draft per node so tab switches and node switches
  // keep it (restored on render).
  if (!panel.detail) return;
  if (draftDiffersFromDetail(readEditorDraft())) saveDraft(panel.detail.id, readEditorDraft());
  else saveDraft(panel.detail.id, null);
}

async function saveTask() {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const flight = {sessionId, gen: panel.gen};
  const draft = readEditorDraft();
  const body = {task: {
    goal: draft.goal,
    acceptance: draft.acceptance,
    context_refs: draft.context_refs,
    repo_path: draft.repo_path || null,
    base_branch: draft.base_branch || null,
    task_type: draft.task_type || null,
    keep_worktree: draft.keep_worktree,
  }};
  const res = await fetch('/api/sessions/' + sessionId, {
    method: 'PATCH', headers: JSON_HEADERS, body: JSON.stringify(body),
  });
  if (isStale(flight)) return; // the node changed mid-request: never write its result here
  await handleMutationResponse(res, 'Save task', () => saveDraft(sessionId, null));
}

// -- actions -----------------------------------------------------------------

function actionButton(label, className, handler, id) {
  const btn = el('button', 'px-3 py-1.5 rounded-lg text-sm font-medium transition-colors ' + className, label);
  if (id) btn.id = id;
  btn.addEventListener('click', handler);
  return btn;
}

function renderActions(detail) {
  const box = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-3');
  box.id = 'task-actions-box';
  box.appendChild(sectionTitle('Actions'));
  const row = el('div', 'flex flex-wrap gap-2');

  row.appendChild(actionButton('New subtask', 'bg-blue-600/80 hover:bg-blue-500 text-white', () => openChildModal(detail.id), 'task-action-child'));

  // Safe parent move: the picker offers open manager ancestors/excluding own subtree via the server guards.
  row.appendChild(actionButton('Move', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => openMoveModal(detail), 'task-action-move'));

  // Idle role change (worker→manager promotion / manager→worker demotion) through the existing PATCH guards.
  if (detail.profile === 'worker' || detail.profile === 'manager') {
    const toProfile = detail.profile === 'worker' ? 'manager' : 'worker';
    row.appendChild(actionButton(
      toProfile === 'manager' ? 'Promote to manager' : 'Demote to worker',
      'border border-slate-600 text-slate-300 hover:bg-slate-700',
      () => changeRole(toProfile), 'task-action-role'));
  }

  if (detail.automation_paused) {
    row.appendChild(actionButton('Resume automation', 'border border-green-600/60 text-green-300 hover:bg-green-900/30', () => patchPaused(false), 'task-action-pause'));
  } else {
    row.appendChild(actionButton('Pause automation', 'border border-amber-600/60 text-amber-300 hover:bg-amber-900/30', () => patchPaused(true), 'task-action-pause'));
  }

  if (detail.task_state === 'open') {
    row.appendChild(actionButton('Complete…', 'bg-green-700 hover:bg-green-600 text-white', () => openCompleteModal(), 'task-action-complete'));
    row.appendChild(actionButton('Cancel task…', 'border border-red-600/60 text-red-300 hover:bg-red-900/30', () => openReasonModal('cancel'), 'task-action-cancel'));
  } else {
    row.appendChild(actionButton('Reopen…', 'border border-blue-500/60 text-blue-300 hover:bg-blue-900/30', () => openReasonModal('reopen'), 'task-action-reopen'));
  }

  // Presentation preference (hide/show/auto).
  const presSelect = el('select');
  presSelect.id = 'task-presentation-select';
  presSelect.className = 'bg-slate-900 border border-slate-600 rounded-lg px-2 py-1.5 text-xs text-slate-300';
  presSelect.setAttribute('aria-label', 'Presentation');
  for (const opt of [['auto', 'Auto'], ['shown', 'Shown'], ['hidden', 'Hidden']]) {
    const o = el('option', undefined, opt[1]);
    o.value = opt[0];
    if ((detail.presentation || 'auto') === opt[0]) o.selected = true;
    presSelect.appendChild(o);
  }
  presSelect.addEventListener('change', () => patchPresentation(presSelect.value));
  const presWrap = el('label', 'flex items-center gap-1.5 text-xs text-slate-400');
  presWrap.appendChild(el('span', undefined, 'Show as'));
  presWrap.appendChild(presSelect);
  row.appendChild(presWrap);

  box.appendChild(row);
  box.appendChild(el('p', 'text-xs text-slate-500',
    'Completion and cancellation are explicit operator actions with server-checked blockers; stopping a Run is separate (Runs tab).'));
  return box;
}

async function patchTaskFields(body, actionKey) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const flight = {sessionId, gen: panel.gen};
  const res = await fetch('/api/sessions/' + sessionId, {
    method: 'PATCH', headers: JSON_HEADERS, body: JSON.stringify(body),
  });
  if (isStale(flight)) return;
  await handleMutationResponse(res, actionKey, () => {});
}

function patchPaused(paused) {
  patchTaskFields({automation_paused: paused}, 'pause');
}

function patchPresentation(mode) {
  patchTaskFields({presentation: mode}, 'presentation');
}

function changeRole(toProfile) {
  patchTaskFields({profile: toProfile}, 'role');
}

async function handleMutationResponse(res, label, onSuccess) {
  if (res.ok || res.status === 202) {
    panel.lastBlockers = [];
    clearRequestId(label);
    let body = null;
    try { body = await res.json(); } catch (_err) { body = null; }
    if (body && body.status === 'pending_run_finish') {
      showToast('Completion request saved: it re-evaluates after the current Run finishes.', false);
    }
    await refresh();
    if (typeof Sidebar !== 'undefined' && Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
    onSuccess();
    return true;
  }
  const detailBody = await res.json().catch(() => ({}));
  const blockers = (detailBody.detail && detailBody.detail.blockers) || [];
  panel.lastBlockers = blockers.length ? blockers : [detailBody.detail?.message || detailBody.detail || ('HTTP ' + res.status)];
  // Unsaved edits stay exactly as they were — only the error surfaces.
  render();
  showToast(label + ' refused: ' + panel.lastBlockers.join(' '), true);
  return false;
}

// -- child creation ----------------------------------------------------------

function openChildModal(parentId) {
  const isRoot = !parentId;
  const overlay = el('div', 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center');
  overlay.id = 'task-child-modal';
  const dialog = el('div', 'bg-slate-800 rounded-xl border border-slate-700 shadow-xl w-full max-w-md mx-4 p-5 space-y-3');
  dialog.appendChild(el('h3', 'text-sm font-semibold text-slate-100', isRoot ? 'New task' : 'New subtask'));

  const parentLabel = el('p', 'text-xs text-slate-400');
  parentLabel.textContent = isRoot
    ? 'Root task (no parent). A v2 manager is created with empty local rules.'
    : 'Under: ' + ((panel.detail && panel.detail.id === parentId && panel.detail.name) || parentId);
  dialog.appendChild(parentLabel);

  const profileSelect = el('select');
  profileSelect.id = 'task-child-profile';
  profileSelect.className = 'w-full bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200';
  for (const p of [['manager', 'Manager (organizes subtasks)'], ['worker', 'Worker (executes and delivers)']]) {
    const o = el('option', undefined, p[1]);
    o.value = p[0];
    profileSelect.appendChild(o);
  }
  dialog.appendChild(fieldRow('Profile', profileSelect));

  const nameInput = inputEl('task-child-name', '', 'Task name');
  dialog.appendChild(fieldRow('Name', nameInput));
  const goalInput = inputEl('task-child-goal', '', 'Goal (required)', true);
  dialog.appendChild(fieldRow('Goal', goalInput));
  const backendSelect = el('select');
  backendSelect.id = 'task-child-backend';
  backendSelect.className = 'w-full bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200';
  const inherit = el('option', undefined, 'Inherit parent backend');
  inherit.value = '';
  backendSelect.appendChild(inherit);
  for (const id of Object.keys(BACKEND_OPTIONS || {})) {
    const o = el('option', undefined, BACKEND_OPTIONS[id]);
    o.value = id;
    backendSelect.appendChild(o);
  }
  dialog.appendChild(fieldRow('Backend', backendSelect));

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const create = actionButton('Create subtask', 'bg-blue-600 hover:bg-blue-500 text-white', async () => {
    const body = {
      request_id: requestIdFor('create-child:' + parentId),
      task_parent_id: parentId,
      profile: profileSelect.value,
      name: nameInput.value.trim() || null,
      task: {goal: goalInput.value, acceptance: [], context_refs: []},
      backend: backendSelect.value || null,
    };
    if (!body.task.goal.trim()) {
      errBox.textContent = 'Goal is required.';
      errBox.classList.remove('hidden');
      return;
    }
    create.disabled = true;
    try {
      const res = await fetch('/api/sessions/', {method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body)});
      if (!res.ok) {
        const detailBody = await res.json().catch(() => ({}));
        errBox.textContent = detailBody.detail?.message || detailBody.detail || ('HTTP ' + res.status);
        errBox.classList.remove('hidden');
        create.disabled = false;
        return;
      }
      const meta = await res.json();
      clearRequestId('create-child:' + parentId);
      overlay.remove();
      if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
      await switchSession(meta.id);
    } catch (err) {
      errBox.textContent = String(err);
      errBox.classList.remove('hidden');
      create.disabled = false;
    }
  });
  buttons.appendChild(create);
  dialog.appendChild(buttons);
  overlay.appendChild(dialog);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
  document.body.appendChild(overlay);
  goalInput.focus();
}

// -- move --------------------------------------------------------------

function openMoveModal(detail) {
  const overlay = el('div', 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center');
  overlay.id = 'task-move-modal';
  const dialog = el('div', 'bg-slate-800 rounded-xl border border-slate-700 shadow-xl w-full max-w-md mx-4 p-5 space-y-3');
  dialog.appendChild(el('h3', 'text-sm font-semibold text-slate-100', 'Move task'));
  dialog.appendChild(el('p', 'text-xs text-slate-400', 'Target must be an open manager (or none for a root). The server refuses cycles, closed targets, and moving a subtree with active work.'));

  const select = el('select');
  select.id = 'task-move-target';
  select.className = 'w-full bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200';
  const rootOpt = el('option', undefined, 'No parent (make it a root task)');
  rootOpt.value = '';
  select.appendChild(rootOpt);
  dialog.appendChild(fieldRow('New parent', select));

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const move = actionButton('Move', 'bg-blue-600 hover:bg-blue-500 text-white', async () => {
    move.disabled = true;
    try {
      const res = await fetch('/api/sessions/' + detail.id, {
        method: 'PATCH', headers: JSON_HEADERS,
        body: JSON.stringify({task_parent_id: select.value || null}),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        const blockers = (body.detail && body.detail.blockers) || [body.detail?.message || ('HTTP ' + res.status)];
        errBox.textContent = blockers.join('\n');
        errBox.classList.remove('hidden');
        move.disabled = false;
        return;
      }
      overlay.remove();
      if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
      await refresh();
    } catch (err) {
      errBox.textContent = String(err);
      errBox.classList.remove('hidden');
      move.disabled = false;
    }
  });
  buttons.appendChild(move);
  dialog.appendChild(buttons);

  // Candidates come from the server's own tree rows: open managers only,
  // excluding the moving subtree (which the server also refuses).
  fetch('/api/sessions/tree?include_archived=false&limit=500').then((r) => (r.ok ? r.json() : {items: []}))
    .then((page) => {
      const exclude = new Set([detail.id]);
      for (const row of page.items || []) {
        if (row.profile !== 'manager' || row.task_state !== 'open' || exclude.has(row.id)) continue;
        const o = el('option', undefined, row.name + ' (' + row.id.slice(0, 8) + ')');
        o.value = row.id;
        if (detail.task_parent_id === row.id) o.selected = true;
        select.appendChild(o);
      }
    })
    .catch((err) => console.error('move candidates fetch failed:', err));

  overlay.appendChild(dialog);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
  document.body.appendChild(overlay);
}

// -- reason modal (cancel / reopen) ---------------------------------------------

function openReasonModal(kind) {
  const overlay = el('div', 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center');
  overlay.id = 'task-reason-modal';
  const dialog = el('div', 'bg-slate-800 rounded-xl border border-slate-700 shadow-xl w-full max-w-md mx-4 p-5 space-y-3');
  dialog.appendChild(el('h3', 'text-sm font-semibold text-slate-100', kind === 'cancel' ? 'Cancel task' : 'Reopen task'));

  const reason = inputEl('task-reason-input', '', kind === 'cancel' ? 'Reason for cancelling' : 'Reason for reopening', true);
  dialog.appendChild(fieldRow('Reason', reason));

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const confirm = actionButton(kind === 'cancel' ? 'Cancel task' : 'Reopen task',
    kind === 'cancel' ? 'bg-red-700 hover:bg-red-600 text-white' : 'bg-blue-600 hover:bg-blue-500 text-white',
    async () => {
      confirm.disabled = true;
      const sessionId = boundSessionId();
      const url = '/api/sessions/' + sessionId + '/' + (kind === 'cancel' ? 'cancel' : 'reopen');
      const body = kind === 'cancel'
        ? {request_id: requestIdFor('cancel'), reason: reason.value}
        : {request_id: requestIdFor('reopen'), reason: reason.value};
      try {
        const res = await fetch(url, {method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body)});
        if (!res.ok) {
          const detailBody = await res.json().catch(() => ({}));
          const blockers = (detailBody.detail && detailBody.detail.blockers) || [detailBody.detail?.message || ('HTTP ' + res.status)];
          errBox.textContent = blockers.join('\n');
          errBox.classList.remove('hidden');
          confirm.disabled = false;
          return;
        }
        clearRequestId(kind);
        overlay.remove();
        if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
        await refresh();
      } catch (err) {
        errBox.textContent = String(err);
        errBox.classList.remove('hidden');
        confirm.disabled = false;
      }
    });
  buttons.appendChild(confirm);
  dialog.appendChild(buttons);
  overlay.appendChild(dialog);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
  document.body.appendChild(overlay);
  reason.focus();
}

// -- completion ------------------------------------------------------------

function renderRunsPicker() {
  // Called when the runs page lands; if the completion modal is open, refresh
  // its checkbox list in place.
  const list = document.getElementById('task-complete-runs');
  if (!list) return;
  list.textContent = '';
  for (const run of eligibleCompletionRuns()) {
    const label = el('label', 'flex items-center gap-2 text-xs text-slate-300 cursor-pointer');
    const box = el('input');
    box.type = 'checkbox';
    box.value = run.id;
    box.className = 'accent-blue-500';
    box.checked = panel.selection.completeRunIds.has(run.id);
    box.addEventListener('change', () => {
      if (box.checked) panel.selection.completeRunIds.add(run.id);
      else panel.selection.completeRunIds.delete(run.id);
    });
    label.appendChild(box);
    label.appendChild(el('span', 'font-mono', run.id.slice(0, 8)));
    label.appendChild(el('span', 'text-slate-500', run.kind + (run.ended_at ? ' · ended' : '')));
    list.appendChild(label);
  }
  if (!list.children.length) list.appendChild(el('p', 'text-xs text-slate-500', 'No finished runs yet.'));
}

function eligibleCompletionRuns() {
  // Runs with a recorded terminal fact are the evidence candidates; active or
  // queued runs are not deliverable evidence.
  return (panel.runs || []).filter((r) => r.state && r.state !== 'queued' && r.state !== 'running' && r.state !== 'stopped' && r.state !== 'attention');
}

function openCompleteModal() {
  panel.selection.completeRunIds = new Set();
  const overlay = el('div', 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center');
  overlay.id = 'task-complete-modal';
  const dialog = el('div', 'bg-slate-800 rounded-xl border border-slate-700 shadow-xl w-full max-w-lg mx-4 p-5 space-y-3');
  dialog.appendChild(el('h3', 'text-sm font-semibold text-slate-100', 'Complete task'));
  dialog.appendChild(el('p', 'text-xs text-slate-400',
    'The server checks pending inputs, active Runs and open children; blockers are reported, never bypassed. Newly arrived inputs are NOT auto-acknowledged.'));

  const summary = inputEl('task-complete-summary', '', 'Delivery summary', true);
  dialog.appendChild(fieldRow('Summary', summary));

  const refs = inputEl('task-complete-refs', '', 'Evidence references, one per line (paths, run ids, links)', true);
  dialog.appendChild(fieldRow('Evidence references', refs));

  dialog.appendChild(el('p', 'text-xs text-slate-400 mt-1', 'Run references (delivered evidence):'));
  const runsList = el('div', 'space-y-1 max-h-40 overflow-y-auto border border-slate-700 rounded-lg p-2');
  runsList.id = 'task-complete-runs';
  dialog.appendChild(runsList);
  renderRunsPicker();

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const confirm = actionButton('Complete task', 'bg-green-700 hover:bg-green-600 text-white', async () => {
    confirm.disabled = true;
    const sessionId = boundSessionId();
    const body = {
      request_id: requestIdFor('complete'),
      summary: summary.value,
      result_refs: refs.value.split('\n').map((s) => s.trim()).filter(Boolean),
      run_ids: [...panel.selection.completeRunIds],
    };
    try {
      const res = await fetch('/api/sessions/' + sessionId + '/complete', {
        method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body),
      });
      if (!res.ok) {
        const detailBody = await res.json().catch(() => ({}));
        const blockers = (detailBody.detail && detailBody.detail.blockers) || [detailBody.detail?.message || ('HTTP ' + res.status)];
        errBox.textContent = blockers.join('\n');
        errBox.classList.remove('hidden');
        confirm.disabled = false;
        return;
      }
      const payload = await res.json().catch(() => null);
      clearRequestId('complete');
      overlay.remove();
      if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
      await refresh();
      if (payload && payload.status === 'pending_run_finish') {
        showToast('Completion re-evaluates after the current Run finishes.', false);
      }
    } catch (err) {
      errBox.textContent = String(err);
      errBox.classList.remove('hidden');
      confirm.disabled = false;
    }
  });
  buttons.appendChild(confirm);
  dialog.appendChild(buttons);
  overlay.appendChild(dialog);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
  document.body.appendChild(overlay);
  summary.focus();
}

// -- pending inputs -----------------------------------------------------------

function renderPendingInputs() {
  // Updates the live panel in place (the fetch lands after the first render);
  // render() also calls this, so both orders converge on the same DOM.
  document.getElementById('task-pending-inputs')?.remove();
  if (!panel.pendingInputs.length) return;
  const box = el('div', 'rounded-xl border border-amber-600/50 bg-amber-900/10 p-4 space-y-2');
  box.id = 'task-pending-inputs';
  box.appendChild(sectionTitle('Pending inputs (' + panel.pendingInputs.length + ')'));
  box.appendChild(el('p', 'text-xs text-slate-400',
    'Unprocessed inputs block completion. Acknowledge only inputs you actually handled elsewhere; each id is acknowledged individually.'));
  const list = el('div', 'space-y-1');
  for (const input of panel.pendingInputs) {
    const label = el('label', 'flex items-start gap-2 text-xs text-slate-300 cursor-pointer');
    const check = el('input');
    check.type = 'checkbox';
    check.className = 'accent-blue-500 mt-0.5';
    check.dataset.inputId = input.id;
    check.setAttribute('aria-label', 'Acknowledge input ' + input.id);
    check.addEventListener('change', () => syncAckSelection());
    label.appendChild(check);
    const textWrap = el('span', 'min-w-0');
    const meta = el('span', 'block text-slate-500');
    meta.textContent = (input.type || 'input') + (input.from_session_name ? ' · from ' + input.from_session_name : '') + ' · ' + input.id.slice(0, 8);
    textWrap.appendChild(meta);
    const text = el('span', 'block break-words');
    const content = input.text || '';
    text.textContent = content.length > 200 ? content.slice(0, 200) + '…' : content;
    textWrap.appendChild(text);
    label.appendChild(textWrap);
    list.appendChild(label);
  }
  box.appendChild(list);
  const note = inputEl('task-ack-note', '', 'Optional note (what was done with these inputs)');
  box.appendChild(fieldRow('Note', note));
  const ackBtn = actionButton('Acknowledge selected', 'border border-amber-500/60 text-amber-200 hover:bg-amber-900/30',
    () => acknowledgeSelected(), 'task-ack-btn');
  ackBtn.disabled = true;
  box.appendChild(ackBtn);
  const actions = document.getElementById('task-actions-box');
  if (actions && actions.parentElement) actions.after(box);
  return box;
}

function syncAckSelection() {
  panel.selection.ackIds = new Set();
  document.querySelectorAll('#task-pending-inputs input[type="checkbox"]').forEach((c) => {
    if (c.checked) panel.selection.ackIds.add(c.dataset.inputId);
  });
  const btn = document.getElementById('task-ack-btn');
  if (btn) {
    btn.disabled = panel.selection.ackIds.size === 0;
    btn.textContent = 'Acknowledge selected (' + panel.selection.ackIds.size + ')';
  }
}

async function acknowledgeSelected() {
  const sessionId = boundSessionId();
  if (!sessionId || !panel.selection.ackIds.size) return;
  const flight = {sessionId, gen: panel.gen};
  const noteEl = document.getElementById('task-ack-note');
  const body = {
    request_id: requestIdFor('ack'),
    input_ids: [...panel.selection.ackIds],
    note: noteEl ? noteEl.value : '',
  };
  const res = await fetch('/api/sessions/' + sessionId + '/task-inputs/acknowledge', {
    method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body),
  });
  if (isStale(flight)) return;
  await handleMutationResponse(res, 'ack', () => {
    panel.selection.ackIds = new Set();
  });
}

// -- lifecycle -----------------------------------------------------------

function onSessionChanged(session) {
  panel.detail = null;
  panel.runs = [];
  panel.pendingInputs = [];
  panel.lastBlockers = null;
  panel.selection.ackIds = new Set();
  panel.actionRequests.clear();
  if (!session || !session.profile) {
    panel.sessionId = null;
    return;
  }
  panel.sessionId = session.id;
  refresh();
}

function onTreeChanged(sessionIds) {
  if (!panel.sessionId) return;
  if (sessionIds.includes(panel.sessionId)) refresh();
}

function onTabShown() {
  if (panel.sessionId === boundSessionId() && panel.detail) return;
  refresh();
}

globalThis.TaskPanel = {
  onSessionChanged,
  onTabShown,
  onTreeChanged,
  openChildModal,
  refresh,
};
})();
