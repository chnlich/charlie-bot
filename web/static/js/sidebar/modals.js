(function() {
  const Sidebar = globalThis.Sidebar;

let renameSessionId = null;
let sessionActionModalState = null;

function startRename(e, id) {
  e.preventDefault();
  e.stopPropagation();
  const link = document.getElementById('session-' + id);
  const header = document.getElementById('header-session-name');
  // The control the reader used owns the field: a chat-header click opens it on the header's name
  // line, and a row click or its menu opens it on the row's. A row that draws no box, as in the
  // closed phone drawer, leaves the header.
  const fromHeader = !!(header && e.currentTarget && e.currentTarget.contains(header));
  const linkRect = link ? link.getBoundingClientRect() : null;
  const linkUsable = !fromHeader && !!(linkRect && linkRect.width);
  const anchor = linkUsable ? link : header;
  const valueEl = linkUsable ? (link.querySelector('.session-name') || header) : header;
  if (!anchor || !valueEl) return;
  renameSessionId = id;
  const rect = anchor.getBoundingClientRect();
  const valueRect = valueEl.getBoundingClientRect();
  const input = document.getElementById('rename-input');
  // The field takes the name line's own height. The row's second line — the time and the backend — stays
  // below it and readable while the name is edited.
  input.style.top = valueRect.top + 'px';
  input.style.left = rect.left + 'px';
  input.style.width = rect.width + 'px';
  input.style.height = valueRect.height + 'px';
  input.style.paddingTop = '0px';
  input.style.paddingBottom = '0px';
  // The field prefills the session's full name, not the row's rendered title:
  // a grouped Threads title drops its header's label (sessionRowTitle), and
  // Enter on an untouched field must keep the name. The shortened row carries
  // its full name in data-full-name (renderSessionRowShell, kept current by
  // updateSidebarSessionName); every other row's text is the name itself.
  input.value = linkUsable && link.dataset.fullName ? link.dataset.fullName : valueEl.textContent;
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
// Session clone (fork) and Elon-e
// ---------------------------------------------------------------------------
function populateSessionActionBackendSelect(selectedBackendId) {
  const select = document.getElementById('session-action-backend');
  if (!select) return;

  select.innerHTML = '';
  for (const entry of BACKEND_PICKER_ENTRIES || []) {
    const option = document.createElement('option');
    option.value = entry.id;
    option.textContent = entry.label;
    option.selected = entry.id === selectedBackendId;
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

// ---------------------------------------------------------------------------
// Task & context dialog (plan 4.2): a read-only view of a logical session's
// Goal, the status Goal the session set. Below it sits the context its
// next Run would assemble: GET /api/sessions/{id}/effective-prompt returns one
// row per managed block with its sources, delivery and measured length. The
// dialog shell shares the session-action modal's chrome in index.html. The
// body is fetched on every open, so it shows the current configuration and
// stores nothing.
// ---------------------------------------------------------------------------
// The dialog's muted tone: its empty values, source labels and prompt table header sit on the
// modal's slate-800, where slate-500 renders 3.07:1. That is under the 4.5:1 floor for text
// this size. Slate-400 renders 5.71:1 and stays quieter than the field values.
const TASK_CONTEXT_MUTED = 'text-slate-400';
const TASK_CONTEXT_NONE = `<span class="${TASK_CONTEXT_MUTED}">(none)</span>`;

let taskContextModalSession = null;

function taskContextField(label, valueHtml) {
  return `<div><div class="text-xs text-slate-400 mb-1">${label}</div>`
    + `<div class="break-words">${valueHtml}</div></div>`;
}

// A full goal body renders as prose through the chat bubble's pipeline:
// escape + renderProseMarkdown + data-raw round-trip. Delegation goals open
// with a "## Goal" heading, and a field that shows it raw paints a literal
// '## '. The renderer escapes raw HTML itself, so the field stays
// injection-safe.
function taskContextGoalHtml(goal) {
  const raw = escapeHtml(goal).replace(/"/g, '&quot;');
  return `<div class="prose-msg break-words" data-raw="${raw}">${renderProseMarkdown(goal)}</div>`;
}

// The upper half: the session's Goal, with a visible empty state.
function taskContextTaskHtml(session) {
  const goal = String((session.web_status || {}).goal || '').trim();
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Task</div>
    ${taskContextField('Goal', goal ? taskContextGoalHtml(goal) : TASK_CONTEXT_NONE)}
  </div>`;
}

function promptSourceLabel(source) {
  const owner = source.source_session_id
    ? ` <span class="${TASK_CONTEXT_MUTED}">from ${escapeHtml(source.source_session_id)}</span>` : '';
  return `<span class="${TASK_CONTEXT_MUTED}">${escapeHtml(source.scope || '')}</span> ${escapeHtml(source.source_ref || '')}${owner}`;
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
    <table class="w-full text-xs"><thead><tr class="${TASK_CONTEXT_MUTED} text-left"><th class="pb-1 font-normal">Source</th><th class="pb-1 font-normal">Delivery</th><th class="pb-1 font-normal text-right">Chars</th></tr></thead><tbody>${rows}</tbody></table>
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
  const taskHtml = taskContextTaskHtml(session);
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
