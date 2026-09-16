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
  pendingInputs: [],  // latest pending-input projection
  selection: {
    ackIds: new Set(),          // pending inputs the operator marked handled
    ackNote: '',                // acknowledgement note text (survives re-renders)
    completeRunIds: new Set(),  // selected completion evidence run ids
    completeSummary: '',        // completion dialog summary draft
    completeRefs: '',           // completion dialog evidence-references draft
    moveChosen: null,           // move dialog's explicit target choice
  },
  actionRequests: new Map(), // actionKey -> request_id (stable per logical action)
  completeRuns: null,  // the completion dialog's paged evidence collection
};

// Per-session operator drafts the action dialogs hold: reason text, the
// completion summary/refs, the selected evidence run ids and the pending-input
// acknowledgement selection. A session switch stashes the current selection
// here and restores it when the same task comes back, so a switch never
// destroys an operator's half-written action.
const dialogDrafts = new Map();

// The panel binds to the session it was shown for: every fetch carries that
// binding and a generation, so a late response for a prior node can never
// replace the active node's editor, run list or header.
function boundSessionId() {
  return panel.sessionId;
}

function isStale(flight) {
  return flight.gen !== panel.gen || flight.sessionId !== panel.sessionId;
}

// Dialog-scoped staleness: a dialog (and its in-flight responses) dies with
// its session, not with the panel's refresh generation — a live data refresh
// must never invalidate an open dialog's pending page.
function dialogStale(flight) {
  return flight.sessionId !== panel.sessionId;
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
    const res = await fetch('/api/sessions/' + sessionId, {cache: 'no-store'});
    if (!res.ok) throw new Error('task detail failed: ' + res.status);
    const detail = await res.json();
    if (isStale(flight)) return; // a late answer never replaces the active node's editor
    panel.detail = detail;
    render();
    // Pending inputs feed the acknowledgement box (a read-only projection of
    // server facts). The completion dialog owns its own paged evidence
    // collection (openCompleteModal) — the panel no longer reads one capped
    // runs page here, which could silently omit the actual delivery from a
    // long history.
    fetch('/api/sessions/' + sessionId + '/task-inputs/pending', {cache: 'no-store'}).then((r) => (r.ok ? r.json() : {items: []}))
      .then((page) => { if (!isStale(flight)) { panel.pendingInputs = page.items || []; renderPendingInputs(); } })
      .catch((err) => console.error('pending inputs fetch failed:', err));
    refreshCompleteRunsIfOpen(flight);
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
  container.appendChild(wrap);
  // Pending inputs insert themselves after the actions box (or no-op when
  // none) — the anchor exists only once wrap is attached. The completion
  // dialog refreshes its own list in place.
  renderPendingInputs();
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

function fieldRow(labelText, inputElNode) {
  const wrap = el('div');
  const label = el('label', 'block text-xs text-slate-400 mb-1', labelText);
  label.htmlFor = inputElNode.id;
  wrap.appendChild(label);
  wrap.appendChild(inputElNode);
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
  await handleMutationResponse(res, 'Save task', () => {
    // Clear the draft only when the editor still holds exactly what was just
    // saved; edits typed while the save was in flight stay as the draft.
    if (JSON.stringify(readEditorDraft()) === JSON.stringify(draft)) saveDraft(sessionId, null);
  });
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

  // Safe parent move: the chooser browses the server's open managers across
  // all pages and depths; the server refuses invalid targets.
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

// -- dialog lifecycle --------------------------------------------------------
// Every action dialog binds to the task it opened for: the origin's session id
// rides every confirm handler, a session switch dismisses the open dialogs
// (after stashing their drafts), and a response that lands after the switch is
// dropped. A dialog opened for A can never cancel/complete/reopen/move B.

function modalShell(id, title, maxWidth) {
  const overlay = el('div', 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center');
  overlay.id = id;
  const dialog = el('div', 'bg-slate-800 rounded-xl border border-slate-700 shadow-xl w-full ' + (maxWidth || 'max-w-lg')
    + ' mx-4 p-5 space-y-3 max-h-[90vh] overflow-y-auto');
  dialog.appendChild(el('h3', 'text-sm font-semibold text-slate-100', title));
  return {overlay, dialog};
}

function dismissTaskModals() {
  for (const id of ['task-child-modal', 'task-move-modal', 'task-reason-modal', 'task-complete-modal']) {
    const m = document.getElementById(id);
    if (m) m.remove();
  }
}

function snapshotSelection() {
  return {
    ackIds: [...panel.selection.ackIds],
    ackNote: panel.selection.ackNote,
    completeRunIds: [...panel.selection.completeRunIds],
    completeSummary: panel.selection.completeSummary,
    completeRefs: panel.selection.completeRefs,
    moveChosen: panel.selection.moveChosen,
  };
}

function applySelection(snapshot) {
  const s = snapshot || {};
  panel.selection = {
    ackIds: new Set(s.ackIds || []),
    ackNote: s.ackNote || '',
    completeRunIds: new Set(s.completeRunIds || []),
    completeSummary: s.completeSummary || '',
    completeRefs: s.completeRefs || '',
    moveChosen: s.moveChosen || null,
  };
}

function stashDialogSelection(sessionId) {
  // Merge with what the session already stashed (e.g. a reason draft): the
  // dialog fields replace their own keys, never the whole entry.
  if (!sessionId) return;
  const previous = dialogDrafts.get(sessionId) || {};
  dialogDrafts.set(sessionId, Object.assign({}, previous, snapshotSelection()));
}

// -- direct root create -------------------------------------------------------
// The primary New Task action's create call: one root manager with empty task
// instructions and the server's default backend resolution — no form, no
// blocking model call. Name/Goal/Profile/Backend stay editable later on the
// Task tab. One pending create action keeps one request id across rapid clicks
// and retries: the server binds (parent, request_id) to one stable node, so a
// replayed request returns the original product and can never mint a second
// node. The id clears on success — the next New Task click starts a fresh
// action; a failed attempt keeps it, making the retry a replay of the same
// operation (visible failure, current view and drafts untouched).
let rootCreateRequestId = null;

function newCreateRequestId() {
  return crypto.randomUUID ? crypto.randomUUID() : String(Date.now()) + '-' + Math.random();
}

async function createRootTask() {
  if (!rootCreateRequestId) rootCreateRequestId = newCreateRequestId();
  const requestId = rootCreateRequestId;
  const res = await fetch('/api/sessions/', {
    method: 'POST', headers: JSON_HEADERS,
    body: JSON.stringify({
      request_id: requestId,
      task_parent_id: null,
      profile: 'manager',
      task: {goal: '', acceptance: [], context_refs: []},
    }),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    const detail = body.detail && (body.detail.message || body.detail);
    throw new Error(typeof detail === 'string' ? detail : ('HTTP ' + res.status));
  }
  rootCreateRequestId = null;
  return await res.json();
}

// -- child creation ----------------------------------------------------------

function openChildModal(parentId) {
  // The explicit child-creation form. A root manager has no form: the primary
  // New Task action creates it directly (createRootTask) and every field stays
  // editable on the Task tab afterwards.
  const originSession = panel.sessionId;
  const {overlay, dialog} = modalShell('task-child-modal', 'New subtask', 'max-w-md');

  const parentLabel = el('p', 'text-xs text-slate-400');
  parentLabel.textContent = 'Under: ' + ((panel.detail && panel.detail.id === parentId && panel.detail.name) || parentId);
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
      // The dialog was opened for one task: it only moves the view to the new
      // child while that task is still the active one.
      if (panel.sessionId === originSession) await switchSession(meta.id);
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

const MOVE_PAGE_LIMIT = 25;

function openMoveModal(detail) {
  const originSession = panel.sessionId;
  if (!originSession) return;
  const {overlay, dialog} = modalShell('task-move-modal', 'Move task');

  const ancestors = detail.ancestors || [];
  const currentParent = ancestors.length ? ancestors[ancestors.length - 1] : null;
  const intro = el('p', 'text-xs text-slate-400');
  intro.textContent = 'Moving "' + detail.name + '". Current parent: '
    + (currentParent ? currentParent.name + ' (' + currentParent.id.slice(0, 8) + ')' : 'none (root task)')
    + '. A target must be an open manager — browse or search below; the server refuses cycles, self/descendant targets and closed ancestors.';
  dialog.appendChild(intro);

  // The chooser's own browse state: one bounded page chain per level (the
  // same revision-bound tree API the sidebar uses), expanded on demand —
  // never a capped roots-only page and never a whole-tree download.
  const mv = {
    levels: new Map(),   // '' (roots) or parentId -> {items, nextCursor, loading, error, notice, loaded}
    expanded: new Set(),
    chosen: panel.selection.moveChosen || null, // the intended choice survives a refusal
    mode: 'browse',      // 'browse' | 'search'
    search: null,        // {items, error, loading} in search mode
  };

  async function fetchMovePage(parentKey, cursor) {
    const params = new URLSearchParams({include_archived: 'false', limit: String(MOVE_PAGE_LIMIT)});
    if (parentKey) params.set('parent_id', parentKey);
    if (cursor) params.set('cursor', cursor);
    const res = await fetch('/api/sessions/tree?' + params.toString(), {cache: 'no-store'});
    if (dialogStale({sessionId: originSession})) return null; // the dialog was dismissed mid-flight
    if (res.status === 409) {
      // The tree moved during pagination: the caller reloads this level from
      // fresh facts and keeps a visible explanation.
      const body = await res.json().catch(() => ({}));
      return {conflict: body.detail?.message || 'Task tree changed while loading candidates'};
    }
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error((body.detail && body.detail.message) || body.detail || ('HTTP ' + res.status));
    }
    return await res.json();
  }

  async function ensureMoveLevel(parentKey, opts = {}) {
    const key = parentKey || '';
    let level = mv.levels.get(key);
    if (!level) {
      level = {items: [], nextCursor: null, loading: false, error: null, notice: null, loaded: false, pages: 1};
      mv.levels.set(key, level);
    }
    if (level.loading) return;
    if (level.loaded && !opts.more) return;
    if (opts.more) level.pages += 1;
    level.loading = true;
    level.error = null;
    if (!opts.more) {
      level.items = [];
      level.nextCursor = null;
      level.notice = null;
    }
    renderMoveBrowser();
    let next = opts.more ? level.nextCursor : null;
    let attempts = 0;
    let done = 0;
    let wantPages = 1;
    try {
      while (true) {
        const page = await fetchMovePage(parentKey, next);
        if (!page) { level.loading = false; return; } // dismissed mid-flight
        if (page.conflict) {
          if (attempts >= 2) throw new Error(page.conflict);
          attempts += 1;
          level.notice = page.conflict;
          // Coherent reload: fresh facts, re-paging to the depth already
          // reached on this level — never silently dropping loaded rows.
          level.items = [];
          level.nextCursor = null;
          next = null;
          done = 0;
          wantPages = Math.max(1, level.pages);
          renderMoveBrowser();
          continue;
        }
        const known = new Set(level.items.map((r) => r.id));
        for (const row of page.items || []) {
          if (!known.has(row.id)) { known.add(row.id); level.items.push(row); }
        }
        level.nextCursor = page.next_cursor || null;
        level.loaded = true;
        done += 1;
        if (done < wantPages && level.nextCursor) {
          next = level.nextCursor;
          continue;
        }
        break;
      }
    } catch (err) {
      level.error = 'Failed to load candidates: ' + (err && err.message ? err.message : err);
    }
    level.loading = false;
    // A revision-change notice stays visible after the coherent reload (the
    // next explicit user action on this level clears it).
    if (!dialogStale({sessionId: originSession})) renderMoveBrowser();
  }

  // Only open managers are valid targets; the moving task itself is excluded.
  function selectableRows(level) {
    return level.items.filter((row) => row.profile === 'manager' && row.task_state === 'open' && row.id !== detail.id);
  }

  function choose(option) {
    mv.chosen = option;
    panel.selection.moveChosen = option;
    renderMoveBrowser();
  }

  function describeChosen() {
    if (!mv.chosen) return 'No target chosen — the Move button stays disabled (making this a root task is an explicit choice below).';
    if (mv.chosen.root) return 'Chosen target: no parent (the task becomes a root).';
    return 'Chosen target: ' + mv.chosen.label + ' (' + mv.chosen.id.slice(0, 8) + ')';
  }

  function chosenRowClass(selected) {
    return 'flex items-center gap-1.5 rounded px-2 py-1 cursor-pointer text-sm min-w-0 '
      + (selected ? 'bg-blue-600/25 text-blue-100' : 'hover:bg-slate-700/50 text-slate-200');
  }

  function rootOptionRow() {
    const rowEl = el('div', chosenRowClass(mv.chosen && mv.chosen.root) + ' border border-dashed border-slate-600 mb-1');
    rowEl.tabIndex = 0;
    rowEl.setAttribute('role', 'button');
    rowEl.setAttribute('aria-label', 'Move under no parent: make it a root task');
    rowEl.appendChild(el('span', 'flex-1 min-w-0 truncate', 'No parent — make it a root task'));
    rowEl.addEventListener('click', () => choose({root: true}));
    rowEl.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); choose({root: true}); } });
    return rowEl;
  }

  function moveRowEl(row, depth, pathLabel) {
    const selected = !!(mv.chosen && !mv.chosen.root && mv.chosen.id === row.id);
    const rowEl = el('div', chosenRowClass(selected));
    rowEl.tabIndex = 0;
    rowEl.setAttribute('role', 'button');
    rowEl.setAttribute('aria-label', 'Move under ' + (pathLabel || row.name));
    rowEl.style.paddingLeft = (4 + depth * 14) + 'px';
    const hasChildren = (row.child_count || 0) > 0;
    const toggle = el('button', 'w-4 h-4 flex-shrink-0 flex items-center justify-center text-slate-500 hover:text-slate-300'
      + (hasChildren ? '' : ' invisible'));
    toggle.type = 'button';
    toggle.setAttribute('aria-label', (mv.expanded.has(row.id) ? 'Collapse ' : 'Expand ') + row.name + "'s manager subtasks");
    toggle.textContent = mv.expanded.has(row.id) ? '▾' : '▸';
    toggle.addEventListener('click', (e) => {
      e.stopPropagation();
      if (mv.expanded.has(row.id)) mv.expanded.delete(row.id);
      else { mv.expanded.add(row.id); void ensureMoveLevel(row.id); }
      renderMoveBrowser();
    });
    rowEl.appendChild(toggle);
    const nameSpan = el('span', 'flex-1 min-w-0 truncate', row.name);
    nameSpan.title = pathLabel || row.name;
    rowEl.appendChild(nameSpan);
    if (detail.task_parent_id === row.id) rowEl.appendChild(badge('current', 'border-slate-500 text-slate-400'));
    rowEl.addEventListener('click', () => choose({id: row.id, label: pathLabel || row.name}));
    rowEl.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); choose({id: row.id, label: pathLabel || row.name}); }
    });
    return rowEl;
  }

  function stateRow(level, parentKey) {
    const wrap = el('div', 'space-y-1');
    if (level.notice) wrap.appendChild(el('p', 'text-xs text-amber-300 px-2', level.notice + ' — refreshing.'));
    if (level.error) {
      const errWrap = el('div', 'space-y-1 px-2');
      errWrap.appendChild(el('p', 'text-xs text-red-300', level.error));
      errWrap.appendChild(actionButton('Retry', 'text-xs border border-slate-600 text-slate-300 hover:bg-slate-700',
        () => { level.loaded = false; level.error = null; void ensureMoveLevel(parentKey); }));
      wrap.appendChild(errWrap);
    }
    return wrap;
  }

  function renderBrowse(container, parentKey, depth) {
    const key = parentKey || '';
    const level = mv.levels.get(key);
    if (!level) {
      container.appendChild(el('p', 'text-xs text-slate-500 px-2 py-1', 'Loading candidates…'));
      void ensureMoveLevel(parentKey);
      return;
    }
    if (depth === 0) container.appendChild(rootOptionRow());
    container.appendChild(stateRow(level, parentKey));
    const rows = selectableRows(level);
    for (const row of rows) {
      container.appendChild(moveRowEl(row, depth, row.name));
      if (mv.expanded.has(row.id)) renderBrowse(container, row.id, depth + 1);
    }
    if (level.loaded && depth > 0 && !rows.length) {
      container.appendChild(el('p', 'text-xs text-slate-500 px-2', 'No open manager subtasks here.'));
    }
    if (level.loading) container.appendChild(el('p', 'text-xs text-slate-500 px-2', 'Loading…'));
    if (level.nextCursor) {
      const more = actionButton('Load more (showing ' + rows.length + ')',
        'text-xs text-blue-400 hover:text-blue-300 border border-slate-700 rounded px-2 py-1 ml-2',
        () => void ensureMoveLevel(parentKey, {more: true}));
      container.appendChild(more);
    }
  }

  function renderSearchResults(container) {
    const s = mv.search || {};
    container.appendChild(rootOptionRow());
    if (s.loading) {
      container.appendChild(el('p', 'text-xs text-slate-500 px-2 py-1', 'Searching tasks…'));
      return;
    }
    if (s.error) {
      container.appendChild(el('p', 'text-xs text-red-300 px-2 py-1', s.error));
      return;
    }
    if (!s.items || !s.items.length) {
      container.appendChild(el('p', 'text-xs text-slate-500 px-2 py-1', 'No tasks match this search.'));
      return;
    }
    for (const hit of s.items) {
      const row = hit.row;
      const pathLabel = (hit.ancestors || []).map((a) => a.name).concat([row.name]).join(' › ');
      let disabledReason = null;
      if (row.id === detail.id) disabledReason = 'this task';
      else if ((hit.ancestors || []).some((a) => a.id === detail.id)) disabledReason = 'inside this subtree (the server refuses it)';
      else if (row.profile !== 'manager') disabledReason = 'worker task';
      else if (row.task_state !== 'open') disabledReason = 'not open';
      const rowEl = el('div', chosenRowClass(!mv.chosen || mv.chosen.root ? false : mv.chosen.id === row.id)
        + (disabledReason ? ' opacity-50 cursor-not-allowed' : ''));
      rowEl.tabIndex = disabledReason ? -1 : 0;
      if (!disabledReason) rowEl.setAttribute('role', 'button');
      rowEl.style.paddingLeft = '4px';
      const nameSpan = el('span', 'flex-1 min-w-0 truncate', pathLabel + (disabledReason ? ' — ' + disabledReason : ''));
      nameSpan.title = pathLabel;
      rowEl.appendChild(nameSpan);
      if (!disabledReason) {
        const pick = () => choose({id: row.id, label: pathLabel});
        rowEl.addEventListener('click', pick);
        rowEl.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); pick(); } });
      }
      container.appendChild(rowEl);
    }
  }

  function renderMoveBrowser() {
    const listEl = document.getElementById('task-move-list');
    if (!listEl) return;
    listEl.textContent = '';
    if (mv.mode === 'search') renderSearchResults(listEl);
    else renderBrowse(listEl, '', 0);
    const chosenLabel = document.getElementById('task-move-chosen');
    if (chosenLabel) chosenLabel.textContent = describeChosen();
    const moveBtn = document.getElementById('task-move-confirm');
    if (moveBtn) moveBtn.disabled = !mv.chosen;
  }

  function runMoveSearch() {
    const input = document.getElementById('task-move-search');
    const q = (input && input.value.trim()) || '';
    if (!q) {
      mv.mode = 'browse';
      mv.search = null;
      renderMoveBrowser();
      return;
    }
    mv.mode = 'search';
    mv.search = {items: null, error: null, loading: true};
    renderMoveBrowser();
    const doFetch = async () => {
      try {
        const res = await fetch('/api/sessions/tree/search?q=' + encodeURIComponent(q) + '&limit=20', {cache: 'no-store'});
        if (dialogStale({sessionId: originSession})) return;
        if (!res.ok) throw new Error('HTTP ' + res.status);
        const body = await res.json();
        if (dialogStale({sessionId: originSession})) return;
        mv.search = {items: body.items || [], error: null, loading: false};
      } catch (err) {
        mv.search = {items: null, error: 'Task search failed: ' + (err && err.message ? err.message : err), loading: false};
      }
      if (!dialogStale({sessionId: originSession})) renderMoveBrowser();
    };
    void doFetch();
  }

  // Dialog chrome.
  const searchRow = el('div', 'flex gap-2');
  const searchInput = inputEl('task-move-search', '', 'Search task names');
  searchRow.appendChild(searchInput);
  searchRow.appendChild(actionButton('Search', 'border border-slate-600 text-slate-300 hover:bg-slate-700 text-xs px-2 py-1', runMoveSearch));
  searchRow.appendChild(actionButton('Browse', 'border border-slate-600 text-slate-400 hover:bg-slate-700 text-xs px-2 py-1', () => {
    const input = document.getElementById('task-move-search');
    if (input) input.value = '';
    mv.mode = 'browse';
    mv.search = null;
    renderMoveBrowser();
  }));
  dialog.appendChild(searchRow);

  const list = el('div', 'border border-slate-700 rounded-lg p-2 max-h-72 overflow-y-auto space-y-0.5 min-h-[6rem]');
  list.id = 'task-move-list';
  dialog.appendChild(list);

  const chosenLabel = el('p', 'text-xs text-slate-300');
  chosenLabel.id = 'task-move-chosen';
  dialog.appendChild(chosenLabel);

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const move = actionButton('Move', 'bg-blue-600 hover:bg-blue-500 text-white', async () => {
    if (!mv.chosen) return; // no silent default: an explicit choice is required
    if (panel.sessionId !== originSession) return; // invalidated with the session switch
    move.disabled = true;
    const targetId = mv.chosen.root ? null : mv.chosen.id;
    try {
      const res = await fetch('/api/sessions/' + detail.id, {
        method: 'PATCH', headers: JSON_HEADERS,
        body: JSON.stringify({task_parent_id: targetId}),
      });
      if (panel.sessionId !== originSession) return; // a late answer never lands on the new task
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        const blockers = (body.detail && body.detail.blockers) || [body.detail?.message || ('HTTP ' + res.status)];
        errBox.textContent = 'The server refused this move:\n' + blockers.join('\n');
        errBox.classList.remove('hidden');
        move.disabled = false; // the intended choice stays selected so the user can correct it
        return;
      }
      panel.selection.moveChosen = null;
      overlay.remove();
      if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
      await refresh();
    } catch (err) {
      errBox.textContent = String(err);
      errBox.classList.remove('hidden');
      move.disabled = false;
    }
  }, 'task-move-confirm');
  move.disabled = true;
  buttons.appendChild(move);
  dialog.appendChild(buttons);

  overlay.appendChild(dialog);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
  document.body.appendChild(overlay);
  searchInput.focus();
  void ensureMoveLevel('');
}

// -- completion ------------------------------------------------------------

const COMPLETE_RUNS_LIMIT = 50;

function completeCollection() {
  // The completion dialog owns its evidence collection: one bounded,
  // newest-first page chain per session (order=desc + the server's cursor) —
  // never a capped first page that silently omits the actual delivery, and
  // never a whole-history download.
  if (!panel.completeRuns) {
    panel.completeRuns = {items: [], nextCursor: null, loading: false, error: null, loadedOnce: false, pagedOlder: false};
  }
  return panel.completeRuns;
}

function fetchCompleteRunsPage(flight, cursor) {
  // Serialized page fetches: a live first-page refresh and a user's
  // "Load older runs" click queue behind each other instead of dropping one
  // side's request.
  const col = completeCollection();
  col.chain = (col.chain || Promise.resolve()).then(
    () => doFetchCompleteRunsPage(flight, cursor, col),
    () => doFetchCompleteRunsPage(flight, cursor, col));
  return col.chain;
}

async function doFetchCompleteRunsPage(flight, cursor, col) {
  const sessionId = boundSessionId();
  // Late-dequeued after a session switch: never render the orphaned
  // collection into the new task's dialog, never issue a read for it.
  if (!sessionId || dialogStale(flight)) return;
  col.loading = true;
  col.error = null;
  renderCompleteRuns();
  try {
    let cur = cursor || null;
    let guard = 0;
    while (true) {
      guard += 1;
      if (guard > 50) throw new Error('runs pagination did not converge');
      const params = new URLSearchParams({limit: String(COMPLETE_RUNS_LIMIT), order: 'desc'});
      if (cur) params.set('cursor', cur);
      const res = await fetch('/api/sessions/' + sessionId + '/runs?' + params.toString(), {cache: 'no-store'});
      if (!res.ok) throw new Error('runs failed: ' + res.status);
      const page = await res.json();
      if (dialogStale(flight)) return; // a late page never lands in another task's dialog
      const known = new Set(col.items.map((r) => r.id));
      const fresh = [];
      for (const run of page.items || []) {
        if (!known.has(run.id)) { known.add(run.id); fresh.push(run); }
      }
      // A first-page refresh (a live Run fact while the dialog is open) merges
      // the newest rows ahead of the pages already loaded; a cursor page
      // appends older rows. Both dedupe by id; the operator's selection and
      // the older pages survive untouched.
      col.items = cur ? col.items.concat(fresh) : fresh.concat(col.items);
      // A first-page refresh must not roll the continuation back to the first
      // boundary once older pages are already loaded; a cursor page advances.
      if (cur || !col.pagedOlder) col.nextCursor = page.next_cursor || null;
      if (cur) col.pagedOlder = true;
      col.loadedOnce = true;
      renderCompleteRuns();
      // A cursor page that only re-walked rows already loaded (live refreshes
      // advanced the history underneath the picker) keeps walking until it
      // contributes new rows or the history ends — every click is productive.
      if (cur && !fresh.length && page.next_cursor) {
        cur = page.next_cursor;
        continue;
      }
      break;
    }
  } catch (err) {
    if (dialogStale(flight)) return;
    col.error = 'Failed to load runs: ' + (err && err.message ? err.message : err);
  }
  col.loading = false;
  if (!dialogStale(flight)) renderCompleteRuns();
}

function refreshCompleteRunsIfOpen(flight) {
  // A Run fact landed for this task while the completion dialog is open: the
  // delivery run may have just finished, so merge the newest page without
  // clearing the operator's selection or the pages already loaded.
  if (!document.getElementById('task-complete-modal')) return;
  void fetchCompleteRunsPage(flight, null);
}

function runEligibleForEvidence(run) {
  // Runs with a recorded terminal fact are the evidence candidates; active,
  // queued or stop-requested rows stay visible for orientation but are not
  // deliverable evidence (the server refuses them authoritatively).
  return !!run.state && run.state !== 'queued' && run.state !== 'running' && run.state !== 'stopped' && run.state !== 'attention';
}

function renderCompleteSelected() {
  // Selected evidence stays visible even when its run is paged out of the
  // loaded window: the chips are the selection's visible truth.
  const box = document.getElementById('task-complete-selected');
  if (!box) return;
  box.textContent = '';
  const ids = [...panel.selection.completeRunIds];
  if (!ids.length) return;
  box.appendChild(el('p', 'text-xs text-slate-300 font-medium', 'Selected evidence (' + ids.length + ')'));
  const rowWrap = el('div', 'flex flex-wrap gap-1');
  for (const id of ids) {
    const chip = el('span', 'inline-flex items-center gap-1 text-[11px] font-mono bg-blue-900/40 border border-blue-700/50 text-blue-200 rounded px-1.5 py-0.5');
    chip.appendChild(el('span', undefined, id.slice(0, 8)));
    const rm = el('button', 'text-blue-300 hover:text-white', '×');
    rm.setAttribute('aria-label', 'Remove run ' + id + ' from the completion evidence');
    rm.addEventListener('click', () => {
      panel.selection.completeRunIds.delete(id);
      renderCompleteRuns();
    });
    chip.appendChild(rm);
    rowWrap.appendChild(chip);
  }
  box.appendChild(rowWrap);
}

function renderCompleteRuns() {
  renderCompleteSelected();
  const list = document.getElementById('task-complete-runs');
  if (!list) return;
  const col = completeCollection();
  list.textContent = '';
  if (col.error) {
    // A failed read is a fetch error with a retry — never "No finished runs".
    const errWrap = el('div', 'space-y-1');
    errWrap.appendChild(el('p', 'text-xs text-red-300', col.error));
    errWrap.appendChild(actionButton('Retry', 'text-xs border border-slate-600 text-slate-300 hover:bg-slate-700',
      () => void fetchCompleteRunsPage({sessionId: panel.sessionId}, null)));
    list.appendChild(errWrap);
    return;
  }
  if (col.loading && !col.items.length) {
    list.appendChild(el('p', 'text-xs text-slate-500', 'Loading runs…'));
    return;
  }
  if (!col.items.length) {
    list.appendChild(el('p', 'text-xs text-slate-500', 'No finished runs yet.'));
    return;
  }
  for (const run of col.items) {
    if (runEligibleForEvidence(run)) {
      const label = el('label', 'flex items-center gap-2 text-xs text-slate-300 cursor-pointer');
      const box = el('input');
      box.type = 'checkbox';
      box.value = run.id;
      box.className = 'accent-blue-500';
      box.checked = panel.selection.completeRunIds.has(run.id);
      box.addEventListener('change', () => {
        if (box.checked) panel.selection.completeRunIds.add(run.id);
        else panel.selection.completeRunIds.delete(run.id);
        renderCompleteSelected();
      });
      label.appendChild(box);
      label.appendChild(el('span', 'font-mono', run.id.slice(0, 8)));
      label.appendChild(el('span', 'text-slate-500', run.kind + (run.ended_at ? ' · ended' : '')));
      list.appendChild(label);
    } else {
      const row = el('div', 'flex items-center gap-2 text-xs text-slate-500');
      row.appendChild(el('span', 'font-mono', run.id.slice(0, 8)));
      row.appendChild(el('span', undefined, run.kind + ' · ' + (run.state || 'unknown') + ' (not delivery evidence)'));
      list.appendChild(row);
    }
  }
  if (col.loading && col.items.length) list.appendChild(el('p', 'text-xs text-slate-500', 'Loading…'));
  if (col.nextCursor) {
    list.appendChild(actionButton('Load older runs (showing ' + col.items.length + ')',
      'w-full text-xs text-blue-400 hover:text-blue-300 border border-slate-700 rounded-lg py-1.5',
      () => void fetchCompleteRunsPage({sessionId: panel.sessionId}, col.nextCursor)));
  }
}

function openCompleteModal() {
  const originSession = panel.sessionId;
  if (!originSession) return;
  const {overlay, dialog} = modalShell('task-complete-modal', 'Complete task');
  dialog.appendChild(el('p', 'text-xs text-slate-400',
    'The server checks pending inputs, active Runs and open children; blockers are reported, never bypassed. Newly arrived inputs are NOT auto-acknowledged.'));

  // The operator's half-written completion (summary, refs, selected evidence)
  // survives page loads, live refreshes, a close/reopen and a session switch.
  const summary = inputEl('task-complete-summary', panel.selection.completeSummary, 'Delivery summary', true);
  summary.addEventListener('input', () => { panel.selection.completeSummary = summary.value; });
  dialog.appendChild(fieldRow('Summary', summary));

  const refs = inputEl('task-complete-refs', panel.selection.completeRefs, 'Evidence references, one per line (paths, run ids, links)', true);
  refs.addEventListener('input', () => { panel.selection.completeRefs = refs.value; });
  dialog.appendChild(fieldRow('Evidence references', refs));

  dialog.appendChild(el('p', 'text-xs text-slate-400 mt-1', 'Run references (delivered evidence), newest first — "Load older runs" reaches earlier history:'));
  const selectedBox = el('div', 'space-y-1');
  selectedBox.id = 'task-complete-selected';
  dialog.appendChild(selectedBox);
  const runsList = el('div', 'space-y-1 max-h-40 overflow-y-auto border border-slate-700 rounded-lg p-2');
  runsList.id = 'task-complete-runs';
  dialog.appendChild(runsList);
  renderCompleteRuns();

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const confirm = actionButton('Complete task', 'bg-green-700 hover:bg-green-600 text-white', async () => {
    if (panel.sessionId !== originSession) return; // invalidated with the session switch
    confirm.disabled = true;
    const body = {
      request_id: requestIdFor('complete'),
      summary: summary.value,
      result_refs: refs.value.split('\n').map((s) => s.trim()).filter(Boolean),
      run_ids: [...panel.selection.completeRunIds],
    };
    try {
      const res = await fetch('/api/sessions/' + originSession + '/complete', {
        method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body),
      });
      if (panel.sessionId !== originSession) return; // a late success/refusal never lands on the new task
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
      panel.selection.completeRunIds = new Set();
      panel.selection.completeSummary = '';
      panel.selection.completeRefs = '';
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
  void fetchCompleteRunsPage({sessionId: originSession}, null);
}

// -- pending inputs -----------------------------------------------------------

function renderPendingInputs() {
  // Updates the live panel in place (the fetch lands after the first render);
  // render() also calls this, so both orders converge on the same DOM.
  document.getElementById('task-pending-inputs')?.remove();
  if (!panel.pendingInputs.length) {
    // Nothing to render right now. The selection is NOT cleared here: a
    // just-restored stash would be wiped before its pending fetch lands. The
    // acknowledgement path only exists while this box is rendered, and the
    // rendered case prunes the selection to still-pending ids below.
    return;
  }
  // Prune the selection to still-pending inputs: an id that left the pending
  // set (handled elsewhere, or claimed by a Run) is no longer acknowledgable.
  panel.selection.ackIds = new Set(
    panel.pendingInputs.filter((i) => panel.selection.ackIds.has(i.id)).map((i) => i.id));
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
    check.checked = panel.selection.ackIds.has(input.id);
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
  const note = inputEl('task-ack-note', panel.selection.ackNote, 'Optional note (what was done with these inputs)');
  note.addEventListener('input', () => { panel.selection.ackNote = note.value; });
  box.appendChild(fieldRow('Note', note));
  const ackBtn = actionButton('Acknowledge selected', 'border border-amber-500/60 text-amber-200 hover:bg-amber-900/30',
    () => acknowledgeSelected(), 'task-ack-btn');
  ackBtn.disabled = panel.selection.ackIds.size === 0;
  if (panel.selection.ackIds.size) ackBtn.textContent = 'Acknowledge selected (' + panel.selection.ackIds.size + ')';
  box.appendChild(ackBtn);
  const actions = document.getElementById('task-actions-box');
  if (actions && actions.parentElement) actions.after(box);
  return box;
}

function syncAckSelection() {
  // The rendered checkboxes are the selection's truth for the inputs on
  // screen; the set never silently grows beyond what the operator can see.
  const domIds = new Set();
  document.querySelectorAll('#task-pending-inputs input[type="checkbox"]').forEach((c) => {
    if (c.checked) domIds.add(c.dataset.inputId);
  });
  panel.selection.ackIds = domIds;
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
    note: noteEl ? noteEl.value : panel.selection.ackNote,
  };
  const res = await fetch('/api/sessions/' + sessionId + '/task-inputs/acknowledge', {
    method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body),
  });
  if (isStale(flight)) return;
  await handleMutationResponse(res, 'ack', () => {
    panel.selection.ackIds = new Set();
    panel.selection.ackNote = '';
  });
}

// -- reason modal (cancel / reopen) ---------------------------------------------

function openReasonModal(kind) {
  const originSession = panel.sessionId;
  if (!originSession) return;
  const {overlay, dialog} = modalShell('task-reason-modal', kind === 'cancel' ? 'Cancel task' : 'Reopen task', 'max-w-md');
  overlay.dataset.kind = kind;
  // The reason draft belongs to (this task, this action): a session switch or
  // an accidental close keeps it for the same task's next attempt.
  const stash = dialogDrafts.get(originSession) || {};
  const reason = inputEl('task-reason-input', (stash.reasons || {})[kind] || '',
    kind === 'cancel' ? 'Reason for cancelling' : 'Reason for reopening', true);
  reason.addEventListener('input', () => {
    const s = dialogDrafts.get(originSession) || {};
    s.reasons = s.reasons || {};
    s.reasons[kind] = reason.value;
    dialogDrafts.set(originSession, s);
  });
  dialog.appendChild(fieldRow('Reason', reason));

  const errBox = el('div', 'hidden text-xs text-red-300 bg-red-900/40 border border-red-700/50 rounded px-3 py-2 whitespace-pre-wrap');
  dialog.appendChild(errBox);

  const buttons = el('div', 'flex justify-end gap-2 pt-1');
  buttons.appendChild(actionButton('Cancel', 'border border-slate-600 text-slate-300 hover:bg-slate-700', () => overlay.remove()));
  const confirm = actionButton(kind === 'cancel' ? 'Cancel task' : 'Reopen task',
    kind === 'cancel' ? 'bg-red-700 hover:bg-red-600 text-white' : 'bg-blue-600 hover:bg-blue-500 text-white',
    async () => {
      if (panel.sessionId !== originSession) return; // invalidated with the session switch
      confirm.disabled = true;
      const url = '/api/sessions/' + originSession + '/' + (kind === 'cancel' ? 'cancel' : 'reopen');
      const body = {request_id: requestIdFor(kind), reason: reason.value};
      try {
        const res = await fetch(url, {method: 'POST', headers: JSON_HEADERS, body: JSON.stringify(body)});
        if (panel.sessionId !== originSession) return; // a late answer never lands on the new task
        if (!res.ok) {
          const detailBody = await res.json().catch(() => ({}));
          const blockers = (detailBody.detail && detailBody.detail.blockers) || [detailBody.detail?.message || ('HTTP ' + res.status)];
          errBox.textContent = blockers.join('\n');
          errBox.classList.remove('hidden');
          confirm.disabled = false;
          return;
        }
        clearRequestId(kind);
        clearReasonDraft(originSession, kind);
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

function clearReasonDraft(sessionId, kind) {
  const s = dialogDrafts.get(sessionId);
  if (s && s.reasons) s.reasons[kind] = '';
}

// -- lifecycle -----------------------------------------------------------

function onSessionChanged(session) {
  // A dialog opened for the prior task dies with the session switch (its
  // in-flight responses are dropped by the origin checks); the operator's
  // drafts and selections move into the per-session stash and come back with
  // the task.
  dismissTaskModals();
  stashDialogSelection(panel.sessionId);
  panel.detail = null;
  panel.pendingInputs = [];
  panel.lastBlockers = null;
  panel.completeRuns = null;
  panel.actionRequests.clear();
  if (!session || !session.profile) {
    panel.sessionId = null;
    applySelection(null);
    return;
  }
  panel.sessionId = session.id;
  applySelection(dialogDrafts.get(session.id));
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
  createRootTask,
  refresh,
};
})();
