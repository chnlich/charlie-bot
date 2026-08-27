(function() {
  const Sidebar = globalThis.Sidebar;

let renameSessionId = null;
let sessionActionModalState = null;

function startRename(e, id, currentName) {
  e.preventDefault();
  e.stopPropagation();
  renameSessionId = id;
  const link = document.getElementById('session-' + id);
  const rect = link.getBoundingClientRect();
  const input = document.getElementById('rename-input');
  input.style.top = rect.top + 'px';
  input.style.left = rect.left + 'px';
  input.style.width = rect.width + 'px';
  input.value = currentName;
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
      headers: { 'Content-Type': 'application/json' },
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
    errorBox.textContent = `加载失败：${task.error}\n文件路径：${task.path}`;
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

async function saveCronTask() {
  const name = document.getElementById('cron-name').value.trim();
  const cron = document.getElementById('cron-expr').value.trim();
  const prompt_file = document.getElementById('cron-prompt-file').value.trim() || null;
  const repo = document.getElementById('cron-repo').value.trim() || null;
  const backend = document.getElementById('cron-backend').value || null;
  const project = document.getElementById('cron-project').value.trim() || null;
  const timezone = document.getElementById('cron-timezone').value.trim();
  const enabled = document.getElementById('cron-enabled').checked;

  let res;
  try {
    if (cronEditMode === 'edit') {
      res = await fetch(`/api/cron/tasks/${encodeURIComponent(cronOriginalName)}`, {
        method: 'PUT',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({cron, prompt_file, repo, backend, project, timezone, enabled}),
      });
    } else {
      res = await fetch('/api/cron/tasks', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({name, cron, prompt_file, repo, backend, project, timezone, enabled}),
      });
    }
  } catch (err) {
    alert('Failed: ' + err);
    return;
  }
  if (!res.ok) {
    alert('Failed: ' + await res.text());
    return;
  }
  closeCronModal();
  switchSidebarFilter('scheduled');
}

async function deleteCronTask() {
  const name = cronOriginalName;
  if (!confirm(`Delete task "${name}"?`)) return;
  let res;
  try {
    res = await fetch(`/api/cron/tasks/${encodeURIComponent(name)}`, {method: 'DELETE'});
  } catch (err) {
    alert('Failed: ' + err);
    return;
  }
  if (!res.ok) {
    alert('Failed: ' + await res.text());
    return;
  }
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
      headers: {'Content-Type': 'application/json'},
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
    bodyText: 'Start a fresh takeover session from this point. Warning: the current session will be archived and rated thumbs down.',
    confirmLabel: 'Elon-e',
    failureLabel: 'Elon-e',
  });
}


Object.assign(Sidebar, {
  startRename,
  handleRenameKey,
  commitRename,
  initSidebarResize,
  openCronEditor,
  openCronAdder,
  closeCronModal,
  saveCronTask,
  deleteCronTask,
  applyCronBrokenView,
  closeSessionActionModal,
  submitSessionActionModal,
  forkSession,
  eloneSession,
});
Sidebar.expose([
  'startRename',
  'handleRenameKey',
  'commitRename',
  'initSidebarResize',
  'openCronEditor',
  'openCronAdder',
  'closeCronModal',
  'saveCronTask',
  'deleteCronTask',
  'closeSessionActionModal',
  'submitSessionActionModal',
  'forkSession',
  'eloneSession',
]);

})();
