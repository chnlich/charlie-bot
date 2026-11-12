// ---------------------------------------------------------------------------
// Scheduled tasks (cron): the task editor, the Workspace error badge, a bound
// node's clock badge and schedule lines, and the schedule menu items. The
// sidebar reaches all of it through the entries this file registers with.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Cron task editor modal
// ---------------------------------------------------------------------------
let cronEditMode = null; // 'edit' or 'add'
let cronOriginalName = null;
// The node a create-mode editor binds: set by Add schedule / New scheduled
// task, saved with the task, and cleared by any other open path. An edit-mode
// save never sends it (the PUT body leaves the task's binding untouched).
let cronAddSessionId = null;

// Fields (besides the always-readonly name) that a broken task locks down: a
// broken file's truth is the raw yaml on disk, never an edit form.
const CRON_EDITABLE_FIELD_IDS = ['cron-expr', 'cron-prompt-file', 'cron-repo', 'cron-project', 'cron-timezone'];

// Switch the modal between the broken read-only error view (task.broken) and
// the normal editable form. Broken: every field read-only, Enabled disabled and
// rendering the file's raw value (indeterminate when the file was
// unparseable). The full load error + path sit in the error box, Save hidden,
// Delete kept.
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

// The Backend dropdown lists the entries the page renders from the picker list (visible options, then visible pools).
// A task bound to a hidden option or pool has no entry there, so the editor adds one and selects it. An open-then-save
// then sends the task's own backend id. The added entry is kept in cronHiddenBackendOption, and the next open removes
// it. "Inherit / default" stays the only way to send null.
let cronHiddenBackendOption = null;

function selectCronBackend(backendId) {
  const select = document.getElementById('cron-backend');
  if (cronHiddenBackendOption) cronHiddenBackendOption.remove();
  cronHiddenBackendOption = null;
  if (backendId && !Array.from(select.options).some((option) => option.value === backendId)) {
    cronHiddenBackendOption = document.createElement('option');
    cronHiddenBackendOption.value = backendId;
    cronHiddenBackendOption.textContent = cronBackendLabel(backendId);
    select.appendChild(cronHiddenBackendOption);
  }
  select.value = backendId || '';
}

// The label a backend id carries in the full lists, which name every option and pool, hidden ones included. An id in
// neither list keeps its own id as the label: the save sends it, and the server refuses it with its own message.
function cronBackendLabel(backendId) {
  if (Object.hasOwn(BACKEND_OPTIONS, backendId)) return BACKEND_OPTIONS[backendId];
  const pool = BACKEND_POOLS.find((candidate) => candidate.id === backendId);
  return pool ? pool.label : backendId;
}

async function openCronEditor(taskName) {
  cronAddSessionId = null;
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
  selectCronBackend(task.backend);
  document.getElementById('cron-project').value = task.project || '';
  document.getElementById('cron-timezone').value = task.timezone || 'America/Los_Angeles';
  document.getElementById('cron-enabled').checked = task.enabled !== false;
  applyCronBrokenView(task);
  document.getElementById('cron-delete-btn').classList.remove('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function openCronAdder(options = {}) {
  cronEditMode = 'add';
  cronOriginalName = null;
  cronAddSessionId = options.sessionId || null;
  document.getElementById('cron-modal-title').textContent = 'New Scheduled Task';
  document.getElementById('cron-name').value = '';
  document.getElementById('cron-name').readOnly = false;
  document.getElementById('cron-expr').value = '';
  document.getElementById('cron-prompt-file').value = '';
  document.getElementById('cron-repo').value = '';
  selectCronBackend(null);
  document.getElementById('cron-project').value = options.project || '';
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
      // A created task with a prefilled binding is born bound (its node
      // exists); a null binding drops out of the body and the scheduler's
      // auto-bind creates the node.
      body: JSON.stringify({name, cron, prompt_file, repo, backend, project, timezone, enabled, session_id: cronAddSessionId}),
    });
  }
  if ((await cronModalRequest(promise)) === null) return;
  closeCronModal();
  // The saved schedule shows on its node row in the view the editor was opened from.
  switchSidebarFilter(currentFilter);
}

async function deleteCronTask() {
  const name = cronOriginalName;
  if (!confirm(`Delete task "${name}"?`)) return;
  const promise = fetch(`/api/cron/tasks/${encodeURIComponent(name)}`, {method: 'DELETE'});
  if ((await cronModalRequest(promise)) === null) return;
  closeCronModal();
  // Deletion archives nothing: the node stays, its clock row goes.
  switchSidebarFilter(currentFilter);
}

// ---------------------------------------------------------------------------
// Cron error badge
// ---------------------------------------------------------------------------
// Global cron load-failure badge, rendered at the top of Workspace from the
// broken cron entries the filter fetch hands the renderer (name order).
// Clicking it opens the cron editor on the first broken task. Empty when
// nothing is broken.
function renderCronErrorBadge(brokenTasks) {
  const broken = brokenTasks || [];
  if (!broken.length) return '';
  return `<div class="mx-3 my-2 px-3 py-2 rounded-lg bg-red-900/40 border border-red-700/50 text-red-300 text-xs cursor-pointer"
       role="button" title="Open the first failed task"
       onclick="openCronEditor('${escapeHtml(broken[0].name)}')">⚠ ${broken.length} scheduled tasks failed to load</div>`;
}

// The Workspace error badge rides this fetch: a failed cron pull yields no
// badge ([]) but never hides the session list.
async function fetchBrokenCronTasks() {
  try {
    const res = await fetch('/api/cron/tasks');
    if (!res.ok) throw new Error(`cron tasks fetch failed: ${res.status}`);
    const tasks = await res.json();
    return tasks.filter(t => t.broken).sort((a, b) => a.name.localeCompare(b.name));
  } catch (err) {
    console.error('Cron tasks fetch failed:', err);
    return [];
  }
}

// The one clock-badge body (face plus hands): renderScheduledBadge below.
const CLOCK_SVG_BODY = `<circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/>`;

// A bound node's clock badge: blue while the task is enabled, grey with the
// Disabled line when it is not. One markup home for every view's row.
function renderScheduledBadge(s) {
  return `<svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtmlAttr(s.schedule_task)}">${CLOCK_SVG_BODY}</svg>`;
}

// The `last_run_status` values the scheduler stores for a schedule's last firing; the cron crate's
// field validation accepts exactly these four.
const LAST_RUN_STATUS = {
  RUNNING: 'running',
  SUCCESS: 'success',
  FAILED: 'failed',
  SKIPPED: 'skipped',
};

// A bound node's schedule lines, in every view: the next run (or Disabled),
// then the truncated cron · timezone line, then the Last line from the firing
// bookkeeping. One markup home for every view's row.
function renderSessionScheduleLine(s) {
  const disabled = s.schedule_enabled === false;
  let html = `<span class="block text-xs ${disabled ? 'text-slate-500' : 'text-blue-300'}">${disabled ? 'Disabled' : 'Next: ' + relativeTime(s.schedule_next_run)}</span>`
    + `<span class="block truncate text-xs text-slate-500" title="${escapeHtmlAttr(s.schedule_cron)} (${escapeHtmlAttr(s.schedule_timezone || '')})">${escapeHtml(s.schedule_cron)} · ${escapeHtml(s.schedule_timezone || '')}</span>`;
  if (s.last_run_status) {
    const lastClass = s.last_run_status === LAST_RUN_STATUS.SUCCESS ? 'text-green-400'
      : s.last_run_status === LAST_RUN_STATUS.RUNNING ? 'text-yellow-400'
        : s.last_run_status === LAST_RUN_STATUS.SKIPPED ? 'text-slate-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400');
    // The Last line truncates with an ellipsis like the cron line above it,
    // and stays one line tall on the touch drawer too. Its full text sits in
    // the title attribute.
    const lastText = `Last: ${s.last_run_status}${s.last_scheduled_run ? ', ' + formatBubbleTime(s.last_scheduled_run) : ''}${s.last_run_status === LAST_RUN_STATUS.FAILED && s.schedule_allow_failure ? ' (review needed)' : ''}`;
    html += `<span class="block truncate text-xs ${lastClass}" title="${escapeHtmlAttr(lastText)}">${escapeHtml(lastText)}</span>`;
  }
  return html;
}

// The group header's clock button: a scheduled task is born bound. The
// manager node is created in that group first. Then the cron editor opens in
// create mode, with the node prefilled as the binding and the task's project
// set to the group.
async function createScheduledTaskInGroup(group) {
  try {
    const node = await Sidebar.createSessionNode(null, group);
    if (typeof Sidebar.expandSessionGroup === 'function') Sidebar.expandSessionGroup(group);
    openCronAdder({sessionId: node.id, project: group});
  } catch (err) {
    console.error('Create scheduled task failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Sidebar registrations
// ---------------------------------------------------------------------------
// The Workspace error badge rides each Workspace fetch.
registerWorkspaceBadge(() => fetchBrokenCronTasks().then(renderCronErrorBadge));
// Cron owns the listing marker, so its package exempts bound roots from the
// group preview limit here. A bound root always shows in place and takes no
// rank against the 5-row cap.
registerGroupPreviewLimitExemption((s) => !!s.schedule_task);
// A bound node's row carries the clock badge and the schedule lines, in every
// view.
registerSessionRowBadge((s) => (s.schedule_task ? renderScheduledBadge(s) : ''));
registerSessionRowLine((s) => (s.schedule_task ? renderSessionScheduleLine(s) : null));
// The row's Settings menu: a bound row edits its task, while an unbound
// manager adds one bound to the row.
registerSessionMenuItems({
  rowAttrs: (s) => ({scheduleTask: s.schedule_task || ''}),
  items: (attrs, sessionId) => {
    if (attrs.scheduleTask) {
      return [{label: 'Edit schedule…', onSelect: () => openCronEditor(attrs.scheduleTask)}];
    }
    if (attrs.profile === 'manager') {
      return [{label: 'Add schedule…', onSelect: () => openCronAdder({sessionId})}];
    }
    return [];
  },
});
// The named group header's menu leads with the scheduled-task creation.
registerGroupMenuItem({label: 'New scheduled task', onSelect: createScheduledTaskInGroup});

const GLOBALS = {
  openCronEditor,
  openCronAdder,
  closeCronModal,
  saveCronTask,
  deleteCronTask,
};
Sidebar.wire(GLOBALS, {applyCronBrokenView});

// The page fetches this module after its document, so the sidebar's rows were already painted without the schedule
// badge and line registered above. Its Workspace badge list was sampled before this module's loader registered.
// Repaint the rows the sidebar still shows, then re-read the Workspace badges with this module's loader in the list.
if (document.readyState !== 'loading') {
  Sidebar.repaintRenderedRows();
  Sidebar.refreshWorkspaceBadges();
}

})();
