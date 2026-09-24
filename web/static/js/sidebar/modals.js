(function() {
  const Sidebar = globalThis.Sidebar;

let renameSessionId = null;
let sessionActionModalState = null;

function startRename(e, id) {
  e.preventDefault();
  e.stopPropagation();
  const link = document.getElementById('session-' + id);
  const header = document.getElementById('header-session-name');
  const linkRect = link ? link.getBoundingClientRect() : null;
  const linkUsable = !!(linkRect && linkRect.width);
  const anchor = linkUsable ? link : header;
  const valueEl = linkUsable && link.querySelector('.session-name') || header;
  if (!anchor || !valueEl) return;
  renameSessionId = id;
  const rect = anchor.getBoundingClientRect();
  const input = document.getElementById('rename-input');
  input.style.top = rect.top + 'px';
  input.style.left = rect.left + 'px';
  input.style.width = rect.width + 'px';
  input.value = valueEl.textContent;
  input.classList.remove('hidden');
  input.focus();
  input.select();
}

function handleRenameKey(e) {
  if (e.key === 'Enter') { e.preventDefault(); commitRename(); }
  if (e.key === 'Escape') { cancelRename(); }
}

async function commitRename() {
  const input = document.getElementById('rename-input');
  if (input.classList.contains('hidden')) return;
  const newName = input.value.trim();
  input.classList.add('hidden');
  if (!newName || !renameSessionId) return;

  try {
    await fetch(`/api/sessions/${renameSessionId}`, {
      method: 'PATCH',
      headers: JSON_HEADERS,
      body: JSON.stringify({ name: newName }),
    });
    // Update DOM — sidebar and header
    updateSidebarSessionName(renameSessionId, newName);
    const header = document.getElementById('header-session-name');
    if (header && renameSessionId === SESSION_ID) header.textContent = newName;
  } catch (err) {
    console.error('Rename failed:', err);
  }
  renameSessionId = null;
}

function cancelRename() {
  document.getElementById('rename-input').classList.add('hidden');
  renameSessionId = null;
}

// ---------------------------------------------------------------------------
// Sidebar resize
// ---------------------------------------------------------------------------
function initSidebarResize() {
  const sidebar = document.getElementById('sidebar');
  const handle = document.getElementById('resize-handle');
  const saved = localStorage.getItem('sidebar-width');
  if (saved) sidebar.style.width = saved + 'px';

  let startX, startW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    startW = sidebar.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');

    function onMove(e) {
      const w = Math.min(Math.max(startW + e.clientX - startX, 200), 600);
      sidebar.style.width = w + 'px';
    }
    function onUp() {
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      localStorage.setItem('sidebar-width', sidebar.offsetWidth);
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      if (latexPanelOpen) {
        loadLatexPdf(true);
      } else if (typeof pdfNeedsReload !== 'undefined') {
        pdfNeedsReload = true;
      }
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}

// ---------------------------------------------------------------------------
// Cron task editor modal
// ---------------------------------------------------------------------------
let cronEditMode = null; // 'edit' or 'add'
let cronOriginalName = null;

// Fields (besides the always-readonly name) that a broken task locks down: a
// broken file's truth is the raw yaml on disk, never an edit form.
const CRON_EDITABLE_FIELD_IDS = ['cron-expr', 'cron-prompt-file', 'cron-repo', 'cron-project', 'cron-timezone'];

// Switch the modal between the broken read-only error view (task.broken) and
// today's editable form. Broken: every field read-only, Enabled disabled and
// rendering the file's raw value (indeterminate when the file was
// unparseable), the full load error + path in the error box, Save hidden,
// Delete kept. Normal/add: byte-identical behavior to the pre-broken form.
function applyCronBrokenView(task) {
  const isBroken = !!(task && task.broken);
  CRON_EDITABLE_FIELD_IDS.forEach(id => { document.getElementById(id).readOnly = isBroken; });
  document.getElementById('cron-backend').disabled = isBroken;
  const enabledEl = document.getElementById('cron-enabled');
  enabledEl.disabled = isBroken;
  enabledEl.indeterminate = isBroken && task.enabled === null;
  if (isBroken) enabledEl.checked = task.enabled === true;
  document.getElementById('cron-save-btn').classList.toggle('hidden', isBroken);
  const errorBox = document.getElementById('cron-error-box');
  if (isBroken) {
    errorBox.textContent = `Load failed: ${task.error}\nFile: ${task.path}`;
    errorBox.classList.remove('hidden');
  } else {
    errorBox.textContent = '';
    errorBox.classList.add('hidden');
  }
}

async function openCronEditor(taskName) {
  let task;
  try {
    const res = await fetch('/api/cron/tasks');
    if (!res.ok) throw new Error(await res.text());
    const tasks = await res.json();
    task = tasks.find(t => t.name === taskName);
  } catch (err) {
    console.error('Failed to load cron tasks:', err);
    alert('Failed to load task: ' + err);
    return;
  }
  if (!task) {
    alert('Task "' + taskName + '" not found');
    return;
  }
  cronEditMode = 'edit';
  cronOriginalName = taskName;
  document.getElementById('cron-modal-title').textContent = 'Edit Scheduled Task';
  document.getElementById('cron-name').value = task.name;
  document.getElementById('cron-name').readOnly = true;
  document.getElementById('cron-expr').value = task.cron || '';
  document.getElementById('cron-prompt-file').value = task.prompt_file || '';
  document.getElementById('cron-repo').value = task.repo || '';
  document.getElementById('cron-backend').value = task.backend || '';
  document.getElementById('cron-project').value = task.project || '';
  document.getElementById('cron-timezone').value = task.timezone || 'America/Los_Angeles';
  document.getElementById('cron-enabled').checked = task.enabled !== false;
  applyCronBrokenView(task);
  document.getElementById('cron-delete-btn').classList.remove('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function openCronAdder() {
  cronEditMode = 'add';
  cronOriginalName = null;
  document.getElementById('cron-modal-title').textContent = 'New Scheduled Task';
  document.getElementById('cron-name').value = '';
  document.getElementById('cron-name').readOnly = false;
  document.getElementById('cron-expr').value = '';
  document.getElementById('cron-prompt-file').value = '';
  document.getElementById('cron-repo').value = '';
  document.getElementById('cron-backend').value = '';
  document.getElementById('cron-project').value = '';
  document.getElementById('cron-timezone').value = 'America/Los_Angeles';
  document.getElementById('cron-enabled').checked = true;
  applyCronBrokenView(null);
  document.getElementById('cron-delete-btn').classList.add('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function closeCronModal() {
  document.getElementById('cron-modal').classList.add('hidden');
}

// Cron-modal mutations share one failure surface: alert on transport error or
// non-OK response and leave the modal open. A null return means the caller
// stops before the close-and-refresh.
async function cronModalRequest(promise) {
  let res;
  try {
    res = await promise;
  } catch (err) {
    alert('Failed: ' + err);
    return null;
  }
  if (!res.ok) {
    alert('Failed: ' + await res.text());
    return null;
  }
  return res;
}

async function saveCronTask() {
  const name = document.getElementById('cron-name').value.trim();
  const cron = document.getElementById('cron-expr').value.trim();
  const prompt_file = document.getElementById('cron-prompt-file').value.trim() || null;
  const repo = document.getElementById('cron-repo').value.trim() || null;
  const backend = document.getElementById('cron-backend').value || null;
  const project = document.getElementById('cron-project').value.trim() || null;
  const timezone = document.getElementById('cron-timezone').value.trim();
  const enabled = document.getElementById('cron-enabled').checked;

  let promise;
  if (cronEditMode === 'edit') {
    promise = fetch(`/api/cron/tasks/${encodeURIComponent(cronOriginalName)}`, {
      method: 'PUT',
      headers: JSON_HEADERS,
      body: JSON.stringify({cron, prompt_file, repo, backend, project, timezone, enabled}),
    });
  } else {
    promise = fetch('/api/cron/tasks', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({name, cron, prompt_file, repo, backend, project, timezone, enabled}),
    });
  }
  if ((await cronModalRequest(promise)) === null) return;
  closeCronModal();
  switchSidebarFilter('scheduled');
}

async function deleteCronTask() {
  const name = cronOriginalName;
  if (!confirm(`Delete task "${name}"?`)) return;
  const promise = fetch(`/api/cron/tasks/${encodeURIComponent(name)}`, {method: 'DELETE'});
  if ((await cronModalRequest(promise)) === null) return;
  closeCronModal();
  switchSidebarFilter('scheduled');
}

// ---------------------------------------------------------------------------
// Session clone (fork) and Elon-e
// ---------------------------------------------------------------------------
function populateSessionActionBackendSelect(selectedBackendId) {
  const select = document.getElementById('session-action-backend');
  if (!select) return;

  select.innerHTML = '';
  for (const [backendId, label] of Object.entries(BACKEND_OPTIONS || {})) {
    const option = document.createElement('option');
    option.value = backendId;
    option.textContent = label;
    option.selected = backendId === selectedBackendId;
    select.appendChild(option);
  }

  if (!select.value) {
    select.value = selectedBackendId || getDefaultBackendId();
  }
}

function openSessionActionModal({
  action,
  sessionId,
  eventIndex = null,
  title,
  bodyText,
  confirmLabel,
  failureLabel,
}) {
  const overlay = document.getElementById('session-action-modal-overlay');
  const titleEl = document.getElementById('session-action-modal-title');
  const bodyEl = document.getElementById('session-action-modal-body');
  const confirmEl = document.getElementById('session-action-modal-confirm');

  sessionActionModalState = {action, sessionId, eventIndex, failureLabel};
  populateSessionActionBackendSelect(getActiveBackendId());

  if (titleEl) titleEl.textContent = title;
  if (bodyEl) bodyEl.textContent = bodyText;
  if (confirmEl) confirmEl.textContent = confirmLabel;
  if (overlay) {
    overlay.classList.remove('hidden');
    overlay.classList.add('flex');
  }
}

function closeSessionActionModal() {
  const overlay = document.getElementById('session-action-modal-overlay');
  if (overlay) {
    overlay.classList.add('hidden');
    overlay.classList.remove('flex');
  }
  sessionActionModalState = null;
}

async function submitSessionActionModal() {
  if (!sessionActionModalState) return;

  const {action, sessionId, eventIndex, failureLabel} = sessionActionModalState;
  const backendSelect = document.getElementById('session-action-backend');
  const backend = backendSelect ? backendSelect.value : getActiveBackendId();

  try {
    const res = await fetch('/api/sessions/' + sessionId + '/' + action, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({event_index: eventIndex, backend}),
    });
    if (!res.ok) throw new Error(await res.text());
    const data = await res.json();
    closeSessionActionModal();
    location.href = '/?session=' + data.id;
  } catch (err) {
    console.error(failureLabel + ' failed:', err);
    alert(failureLabel + ' failed: ' + err.message);
  }
}

function forkSession(sessionId, eventIndex = null) {
  const isPartialClone = eventIndex != null;
  openSessionActionModal({
    action: 'fork',
    sessionId,
    eventIndex,
    title: isPartialClone ? 'Clone to Here' : 'Clone Session',
    bodyText: isPartialClone
      ? 'Create a new session from this response boundary and choose the backend for the clone.'
      : 'Create a full clone of this session and choose the backend for the clone.',
    confirmLabel: 'Clone',
    failureLabel: 'Clone',
  });
}

function eloneSession(sessionId, eventIndex) {
  openSessionActionModal({
    action: 'elone',
    sessionId,
    eventIndex,
    title: 'Elon-e Session',
    bodyText: 'Start a fresh takeover session from this point. Warning: the current session will be archived.',
    confirmLabel: 'Elon-e',
    failureLabel: 'Elon-e',
  });
}


// ---------------------------------------------------------------------------
// Task & context dialog (plan 4.2): a read-only view of a logical session's
// task record (goal, acceptance, context refs) over the context its next Run
// would assemble (GET /api/sessions/{id}/effective-prompt: one row per
// managed block with its sources, delivery and measured length). The dialog
// shell shares the session-action modal's chrome in index.html; the body is
// fetched on every open, so it shows the current configuration and stores
// nothing.
// ---------------------------------------------------------------------------
const TASK_CONTEXT_NONE = '<span class="text-slate-500">(none)</span>';

let taskContextModalSession = null;

function taskContextField(label, valueHtml) {
  return `<div><div class="text-xs text-slate-400 mb-1">${label}</div><div class="whitespace-pre-wrap break-words">${valueHtml}</div></div>`;
}

function taskContextList(items) {
  if (!Array.isArray(items) || !items.length) return TASK_CONTEXT_NONE;
  return `<ul class="list-disc pl-4 space-y-0.5">${items.map((item) => `<li>${escapeHtml(String(item))}</li>`).join('')}</ul>`;
}

// The upper half: the task record's three fields, each with a visible empty state.
function taskContextTaskHtml(task) {
  const t = task || {};
  const goal = (t.goal || '').trim();
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Task</div>
    ${taskContextField('Goal', goal ? escapeHtml(goal) : TASK_CONTEXT_NONE)}
    ${taskContextField('Acceptance', taskContextList(t.acceptance))}
    ${taskContextField('Context refs', taskContextList(t.context_refs))}
  </div>`;
}

function promptSourceLabel(source) {
  const owner = source.source_session_id
    ? ` <span class="text-slate-500">from ${escapeHtml(source.source_session_id)}</span>` : '';
  return `<span class="text-slate-500">${escapeHtml(source.scope || '')}</span> ${escapeHtml(source.source_ref || '')}${owner}`;
}

function formatCharCount(n) {
  return Number(n || 0).toLocaleString('en-US');
}

// The lower half: one row per assembled block (its sources, delivery and
// measured length) under the run kind and the total the backend receives.
function taskContextPromptHtml(preview) {
  const blocks = Array.isArray(preview.blocks) ? preview.blocks : [];
  const rows = blocks.map((block) => {
    const sources = (block.sources || []).map(promptSourceLabel).join('<br>') || TASK_CONTEXT_NONE;
    const chars = typeof block.text === 'string' ? block.text.length : 0;
    return `<tr class="border-t border-slate-700/60 align-top">
      <td class="py-1.5 pr-3 break-all">${sources}</td>
      <td class="py-1.5 pr-3 text-slate-400">${escapeHtml(block.delivery || '')}</td>
      <td class="py-1.5 text-right tabular-nums text-slate-400">${formatCharCount(chars)}</td>
    </tr>`;
  }).join('');
  const total = typeof preview.char_count === 'number'
    ? preview.char_count
    : blocks.reduce((n, b) => n + (typeof b.text === 'string' ? b.text.length : 0), 0);
  const overlayNote = preview.overlay && preview.overlay.error
    ? `<div class="text-xs text-amber-400">Launch overlay unavailable: ${escapeHtml(preview.overlay.error)}</div>` : '';
  return `<div class="space-y-2">
    <div class="flex items-baseline justify-between gap-3">
      <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Next run context</div>
      <div class="text-xs text-slate-400">${escapeHtml(preview.kind || '')} · ${formatCharCount(total)} chars</div>
    </div>
    <table class="w-full text-xs"><thead><tr class="text-slate-500 text-left"><th class="pb-1 font-normal">Source</th><th class="pb-1 font-normal">Delivery</th><th class="pb-1 font-normal text-right">Chars</th></tr></thead><tbody>${rows}</tbody></table>
    ${overlayNote}
  </div>`;
}

function taskContextNoteHtml(text) {
  return `<div class="text-xs text-slate-400">${escapeHtml(text)}</div>`;
}

// A reply that arrives after the dialog closed or moved to another session
// is dropped: the open dialog shows the session it was opened for.
function setTaskContextModalBody(sessionId, html) {
  if (taskContextModalSession !== sessionId) return;
  const bodyEl = document.getElementById('task-context-modal-body');
  if (bodyEl) bodyEl.innerHTML = html;
}

async function fetchJsonOrDetail(url) {
  const res = await fetch(url);
  if (res.ok) return {ok: true, data: await res.json()};
  let detail = res.statusText || ('HTTP ' + res.status);
  try {
    const body = await res.json();
    if (body && body.detail) detail = String(body.detail);
  } catch (_) { /* a non-JSON error body keeps the status text */ }
  return {ok: false, detail};
}

// Assembles the dialog body for one session: the task record from the
// session detail, then the effective-prompt preview. A session without a
// profile is not a task node yet, so its context section says so in place of
// a preview request the server would refuse.
async function loadTaskContextModal(sessionId) {
  const detail = await fetchJsonOrDetail('/api/sessions/' + sessionId);
  if (!detail.ok) {
    setTaskContextModalBody(sessionId, taskContextNoteHtml('Session unavailable: ' + detail.detail));
    return;
  }
  const session = detail.data;
  const titleEl = document.getElementById('task-context-modal-title');
  if (titleEl && taskContextModalSession === sessionId) {
    titleEl.textContent = 'Task & context · ' + (session.name || sessionId);
  }
  const taskHtml = taskContextTaskHtml(session.task);
  if (!session.profile) {
    setTaskContextModalBody(sessionId, taskHtml + taskContextNoteHtml(
        'Not a task node yet: this session becomes one on its first child session or delegation; '
        + 'its next run context is assembled from then on.'));
    return;
  }
  setTaskContextModalBody(sessionId, taskHtml + taskContextNoteHtml('Loading next run context…'));
  const preview = await fetchJsonOrDetail('/api/sessions/' + sessionId + '/effective-prompt');
  setTaskContextModalBody(sessionId, taskHtml + (preview.ok
      ? taskContextPromptHtml(preview.data)
      : taskContextNoteHtml('Next run context unavailable: ' + preview.detail)));
}

function openTaskContextModal(sessionId) {
  taskContextModalSession = sessionId;
  const overlay = document.getElementById('task-context-modal-overlay');
  const titleEl = document.getElementById('task-context-modal-title');
  const bodyEl = document.getElementById('task-context-modal-body');
  if (titleEl) titleEl.textContent = 'Task & context';
  if (bodyEl) bodyEl.innerHTML = taskContextNoteHtml('Loading…');
  if (overlay) {
    overlay.classList.remove('hidden');
    overlay.classList.add('flex');
  }
  return loadTaskContextModal(sessionId).catch((err) => {
    console.error('Task & context failed:', err);
    setTaskContextModalBody(sessionId, taskContextNoteHtml('Task & context failed: ' + err.message));
  });
}

function closeTaskContextModal() {
  const overlay = document.getElementById('task-context-modal-overlay');
  if (overlay) {
    overlay.classList.add('hidden');
    overlay.classList.remove('flex');
  }
  taskContextModalSession = null;
}


const GLOBALS = {
  startRename,
  handleRenameKey,
  commitRename,
  initSidebarResize,
  openCronEditor,
  openCronAdder,
  closeCronModal,
  saveCronTask,
  deleteCronTask,
  closeSessionActionModal,
  submitSessionActionModal,
  forkSession,
  eloneSession,
  openTaskContextModal,
  closeTaskContextModal,
};
const SIDEBAR_ONLY = { applyCronBrokenView };
Sidebar.wire(GLOBALS, SIDEBAR_ONLY);

})();
