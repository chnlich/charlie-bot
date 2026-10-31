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
    // Update DOM — the sidebar row, and through the displayed-session module
    // the header and tab title when this is the displayed session. A rename
    // never changes which session is displayed.
    updateSidebarSessionName(renameSessionId, newName);
    refreshDisplayedSessionName(renameSessionId, newName);
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
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
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

// The backend-choice modal of a session action. With onConfirm null, the confirm
// POSTs /api/sessions/<id>/<action> and opens the session it creates. A package
// passes onConfirm(backend) for an action that stays on this session.
function openSessionActionModal({
  action,
  sessionId,
  eventIndex,
  title,
  bodyText,
  confirmLabel,
  failureLabel,
  onConfirm,
}) {
  const overlay = document.getElementById('session-action-modal-overlay');
  const titleEl = document.getElementById('session-action-modal-title');
  const bodyEl = document.getElementById('session-action-modal-body');
  const confirmEl = document.getElementById('session-action-modal-confirm');

  sessionActionModalState = {action, sessionId, eventIndex, failureLabel, onConfirm};
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

  const {action, sessionId, eventIndex, failureLabel, onConfirm} = sessionActionModalState;
  const backendSelect = document.getElementById('session-action-backend');
  const backend = backendSelect ? backendSelect.value : getActiveBackendId();

  try {
    if (onConfirm) {
      // A package's action stays on this session: its handler runs the request
      // and its follow-up, and the modal closes after it.
      await onConfirm(backend);
      closeSessionActionModal();
      return;
    }
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

function forkSession(sessionId, eventIndex) {
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
    onConfirm: null,
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
    onConfirm: null,
  });
}

// The separator line's Clone to here and Elon-e buttons. modals.js is the one
// core file that registers separator buttons, and core loads before every
// package, so these two lead the line.
Chat.registerSeparatorButton(function(sessionId, eventIndex) {
  return "<button onclick=\"forkSession(\x27" + sessionId + "\x27, " + eventIndex + ")\""
    + " class=\"p-0.5 text-slate-500 hover:text-green-400\" title=\"Clone to here\">"
    + Chat.CLONE_SVG
    + "</button>"
    + "<button onclick=\"eloneSession(\x27" + sessionId + "\x27, " + eventIndex + ")\""
    + " class=\"p-0.5 text-slate-500 hover:text-yellow-400\" title=\"Elon-e: retry with a fresh perspective\">"
    + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 10V3L4 14h7v7l9-11h-7z\"/></svg>"
    + "</button>";
});


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

function taskContextField(label, valueHtml, {preWrap}) {
  const wrap = preWrap ? 'whitespace-pre-wrap ' : '';
  return `<div><div class="text-xs text-slate-400 mb-1">${label}</div><div class="${wrap}break-words">${valueHtml}</div></div>`;
}

function taskContextList(items) {
  if (!Array.isArray(items) || !items.length) return TASK_CONTEXT_NONE;
  return `<ul class="list-disc pl-4 space-y-0.5">${items.map((item) => `<li>${escapeHtml(String(item))}</li>`).join('')}</ul>`;
}

// A full goal body renders as prose, the chat bubble's pipeline (escape +
// renderProseMarkdown + data-raw round-trip): delegation goals open with a
// "## Goal" heading, and a field that shows it raw paints a literal '## '.
// The renderer escapes raw HTML itself, so the field stays injection-safe.
function taskContextGoalHtml(goal) {
  const raw = escapeHtml(goal).replace(/"/g, '&quot;');
  return `<div class="prose-msg break-words" data-raw="${raw}">${renderProseMarkdown(goal)}</div>`;
}

// The upper half: the task record's three fields, each with a visible empty state.
function taskContextTaskHtml(task) {
  const t = task || {};
  const goal = (t.goal || '').trim();
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Task</div>
    ${taskContextField('Goal', goal ? taskContextGoalHtml(goal) : TASK_CONTEXT_NONE, {preWrap: false})}
    ${taskContextField('Acceptance', taskContextList(t.acceptance), {preWrap: true})}
    ${taskContextField('Context refs', taskContextList(t.context_refs), {preWrap: true})}
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

// The session row's double-click (groups.js).
registerSidebarControl('startRename', startRename);

const GLOBALS = {
  startRename,
  handleRenameKey,
  commitRename,
  initSidebarResize,
  openSessionActionModal,
  closeSessionActionModal,
  submitSessionActionModal,
  forkSession,
  eloneSession,
  openTaskContextModal,
  closeTaskContextModal,
};
Sidebar.wire(GLOBALS);

})();
