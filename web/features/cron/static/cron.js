// ---------------------------------------------------------------------------
// Scheduled tasks (cron): the schedule dialog that Edit schedule… and Add
// schedule… open, the Scheduled task section of a bound session's Task & context
// dialog (read-only), the error view of a task that failed to load, the Workspace
// error badge, and a bound row's clock badge and schedule lines. The dialog
// changes a task's Schedule, Timezone and Enabled, and removes its binding. The
// prompt file, the repo and the other task fields stay with the session's agent,
// which edits the task file. The sidebar reaches this file through the entries
// it registers with.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Schedule sentence
// ---------------------------------------------------------------------------
// cronScheduleTitle reads a five-field cron as a plain sentence and returns the
// raw text for any form it does not read.
function cronValue(value) {
  return value === null || value === undefined || value === '' ? '—' : String(value);
}

function cronNumericValues(field, maximum) {
  const values = [];
  for (const part of field.split(',')) {
    const match = part.match(/^(\d+)(?:-(\d+))?$/);
    if (!match) return null;
    const first = Number(match[1]);
    const last = Number(match[2] || match[1]);
    if (first > last || last > maximum) return null;
    for (let value = first; value <= last; value++) values.push(value);
  }
  return [...new Set(values)];
}

function cronList(values, names, padNumbers = false) {
  const labels = values.map((value) => names ? names[value] : (padNumbers ? String(value).padStart(2, '0') : String(value)));
  if (labels.length === 1) return labels[0];
  if (labels.length === 2) return `${labels[0]} and ${labels[1]}`;
  return `${labels.slice(0, -1).join(', ')}, and ${labels[labels.length - 1]}`;
}

function cronFieldDescription(field, values, names, padNumbers = false) {
  if (/^\d+-\d+$/.test(field)) {
    return `${names ? names[values[0]] : (padNumbers ? String(values[0]).padStart(2, '0') : values[0])}`
      + ` through ${names ? names[values[values.length - 1]] : (padNumbers ? String(values[values.length - 1]).padStart(2, '0') : values[values.length - 1])}`;
  }
  return cronList(values, names, padNumbers);
}

function cronTimeDescription(minute, hour) {
  const minutes = minute === '*' ? null : cronNumericValues(minute, 59);
  if (minute === '*' && hour === '*') return {fixed: false, text: 'Every minute'};
  if (minute === '0' && hour === '*') return {fixed: false, text: 'Every hour'};
  if (!minutes) return null;
  if (hour === '*') {
    return {fixed: false, text: `At minute${minutes.length === 1 ? '' : 's'} ${cronList(minutes, null, true)} past every hour`};
  }
  const hours = cronNumericValues(hour, 23);
  if (!hours) return null;
  if (hours.length === 1 && minutes.length === 1) {
    return {
      fixed: true,
      text: `${String(hours[0]).padStart(2, '0')}:${String(minutes[0]).padStart(2, '0')}`,
    };
  }
  const minuteText = `${minutes.length === 1 ? 'minute' : 'minutes'} ${cronList(minutes, null, true)} past`;
  if (/^\d+-\d+$/.test(hour)) {
    return {
      fixed: false,
      text: `At ${minuteText} each hour from ${String(hours[0]).padStart(2, '0')}:00 through `
        + `${String(hours[hours.length - 1]).padStart(2, '0')}:00`,
    };
  }
  return {fixed: false, text: `At ${minuteText} hour${hours.length === 1 ? '' : 's'} ${cronList(hours, null, true)}`};
}

function cronScheduleTitle(cron) {
  const raw = cronValue(cron);
  const fields = raw.trim().split(/\s+/);
  if (fields.length !== 5) return raw;
  const [minute, hour, dayOfMonth, month, dayOfWeek] = fields;
  const time = cronTimeDescription(minute, hour);
  if (!time) return raw;

  if (dayOfMonth === '*' && month === '*' && dayOfWeek === '*') {
    if (time.text === 'Every minute' || time.text === 'Every hour') return time.text;
    return time.fixed ? `Every day at ${time.text}` : `${time.text} every day`;
  }

  const weekdays = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
  if (dayOfMonth === '*' && month === '*' && dayOfWeek !== '*') {
    const values = cronNumericValues(dayOfWeek, 7);
    if (!values) return raw;
    const normalized = [...new Set(values.map((value) => value % 7))];
    const days = cronFieldDescription(dayOfWeek, normalized, weekdays);
    return time.fixed ? `Every ${days} at ${time.text}` : `${time.text} on ${days}`;
  }

  const months = [
    'January', 'February', 'March', 'April', 'May', 'June',
    'July', 'August', 'September', 'October', 'November', 'December',
  ];
  if (dayOfWeek === '*' && /^\d+$/.test(dayOfMonth) && /^\d+$/.test(month)) {
    const dayNumber = Number(dayOfMonth);
    const monthNumber = Number(month);
    if (dayNumber >= 1 && dayNumber <= 31 && monthNumber >= 1 && monthNumber <= 12) {
      return time.fixed
        ? `Every ${months[monthNumber - 1]} ${dayNumber} at ${time.text}`
        : `${time.text} every ${months[monthNumber - 1]} ${dayNumber}`;
    }
  }
  if (dayOfWeek === '*' && month === '*' && /^\d+$/.test(dayOfMonth)) {
    const dayNumber = Number(dayOfMonth);
    if (dayNumber >= 1 && dayNumber <= 31) {
      return time.fixed ? `On day ${dayNumber} of every month at ${time.text}` : `${time.text} on day ${dayNumber} of every month`;
    }
  }
  return raw;
}

// ---------------------------------------------------------------------------
// Scheduled task section of Task & context
// ---------------------------------------------------------------------------
// The section lists the tasks bound to the session, read from GET /api/cron/tasks each time the dialog opens. Each task
// shows its schedule, its name, the settings it sets and its prompt text, in the dialog's flat fields. The prompt
// renders like the Goal, in the dialog's one scroll. The section holds no control. A session no task binds gets no
// section. A failed fetch shows in the section, so an unreadable task list does not look like an unbound session.
function scheduledTaskSection(body) {
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Scheduled task</div>
    <div class="space-y-3">${body}</div>
  </div>`;
}

// A setting shows as a field only when it holds a value: an unset or empty setting is left out of the section.
function scheduledTaskSetting(label, value) {
  return value ? Sidebar.taskContextField(label, escapeHtml(value)) : '';
}

// A prompt shows as its file and its text, each only when it holds a value.
function scheduledTaskPrompt(promptFile, prompt) {
  return scheduledTaskSetting('Prompt file', promptFile)
    + (prompt ? Sidebar.taskContextField('Prompt', Sidebar.taskContextProseHtml(prompt)) : '');
}

// One step of a steps task, in the order its config declares it: its name, the backend settings it
// sets, then its prompt. Each step after the first starts under a flat divider.
function renderScheduledStep(step) {
  return `<div class="flex flex-col gap-3 border-t border-slate-700/60 pt-2 first:border-t-0 first:pt-0">`
    + `<div class="text-sm font-medium text-slate-200">${escapeHtml(step.name)}</div>`
    + scheduledTaskSetting('Backend', step.backend)
    + scheduledTaskSetting('Distinct backend from', step.distinct_backend_from)
    + scheduledTaskPrompt(step.prompt_file, step.prompt)
    + '</div>';
}

// What a task runs: a handler names its built-in work; a steps task lists its steps; any other task has one prompt.
function renderScheduledTaskBody(task) {
  if (task.handler) return scheduledTaskSetting('Handler', task.handler);
  if (task.steps && task.steps.length > 0) {
    const steps = task.steps.map(renderScheduledStep).join('');
    return Sidebar.taskContextField('Steps', `<div class="flex flex-col gap-3">${steps}</div>`);
  }
  return scheduledTaskPrompt(task.prompt_file, task.prompt);
}

// One bound task: its schedule sentence heads the block, its name sits under it in small type since agents address the
// task by it, and a disabled task says so. Its schedule, its settings and its body follow.
function renderScheduledTask(task) {
  const disabled = task.enabled === false ? '<div class="text-xs font-medium text-amber-400">Disabled</div>' : '';
  return `<div class="flex flex-col gap-3 border-t border-slate-700/60 pt-2 first:border-t-0 first:pt-0">`
    + `<div><div class="text-sm font-medium text-slate-200">${escapeHtml(cronScheduleTitle(task.cron))}</div>`
    + `<div class="font-mono text-[11px] text-slate-400">${escapeHtml(task.name)}</div>${disabled}</div>`
    + Sidebar.taskContextField('Schedule', escapeHtml(`${task.cron} (${task.timezone})`))
    + scheduledTaskSetting('Mode', task.mode)
    + scheduledTaskSetting('Repo', task.repo)
    + scheduledTaskSetting('Improve host', task.improve_host)
    + scheduledTaskSetting('Allow failure', task.allow_failure === true ? 'yes' : '')
    + renderScheduledTaskBody(task)
    + '</div>';
}

async function loadScheduledTaskSection(sessionId) {
  let tasks;
  try {
    const res = await fetch('/api/cron/tasks');
    if (!res.ok) throw new Error(`cron tasks fetch failed: ${res.status}`);
    tasks = (await res.json()).filter((task) => !task.broken && task.session_id === sessionId);
  } catch (err) {
    console.error('Scheduled task fetch failed:', err);
    return scheduledTaskSection(
      `<div class="text-xs text-amber-400">Could not load scheduled tasks: ${escapeHtml(String(err))}</div>`);
  }
  return tasks.length === 0 ? '' : scheduledTaskSection(tasks.map(renderScheduledTask).join(''));
}

// ---------------------------------------------------------------------------
// Schedule dialog
// ---------------------------------------------------------------------------
// One dialog, three views. 'edit' (Edit schedule…) changes one bound task's
// Schedule, Timezone and Enabled, switches between the session's tasks, and
// deletes the binding. 'add' (Add schedule…) binds a new prompt-file task to an
// unbound manager session. 'error' shows a task that failed to load: the load
// error and the file path, with no form and no Save or Delete. A save or delete
// the server refuses shows its detail in #cron-form-error and keeps the inputs.
const CRON_DEFAULT_TIMEZONE = 'America/Los_Angeles';
const CRON_DIALOG_TITLES = {edit: 'Edit schedule', add: 'Add schedule', error: 'Scheduled task failed to load'};
let cronView = null;          // the view the open dialog shows, or null while it is closed
let cronEditTasks = [];       // the tasks the session binds, as the edit view's switcher lists them
let cronEditName = null;      // the task the edit view's fields hold
let cronAddSessionId = null;  // the session the add view binds

async function fetchCronTasks() {
  const res = await fetch('/api/cron/tasks');
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

function showCronDialog(view) {
  cronView = view;
  const isEdit = view === 'edit';
  const isAdd = view === 'add';
  const isError = view === 'error';
  document.getElementById('cron-modal-title').textContent = CRON_DIALOG_TITLES[view];
  document.getElementById('cron-form').classList.toggle('hidden', isError);
  document.getElementById('cron-error-box').classList.toggle('hidden', !isError);
  document.getElementById('cron-task-row').classList.toggle('hidden', !(isEdit && cronEditTasks.length > 1));
  document.getElementById('cron-name-row').classList.toggle('hidden', !isAdd);
  document.getElementById('cron-prompt-row').classList.toggle('hidden', !isAdd);
  document.getElementById('cron-delete-btn').classList.toggle('hidden', !isEdit);
  document.getElementById('cron-save-btn').classList.toggle('hidden', isError);
  clearCronFormError();
  document.getElementById('cron-modal').classList.remove('hidden');
}

function closeCronModal() {
  cronView = null;
  document.getElementById('cron-modal').classList.add('hidden');
}

function showCronFormError(message) {
  const box = document.getElementById('cron-form-error');
  box.textContent = message;
  box.classList.remove('hidden');
}

function clearCronFormError() {
  const box = document.getElementById('cron-form-error');
  box.textContent = '';
  box.classList.add('hidden');
}

// The fields of one task: its Schedule, Timezone and Enabled. The name and prompt file belong to the add view.
function fillCronFields(task) {
  document.getElementById('cron-name').value = task.name;
  document.getElementById('cron-expr').value = task.cron || '';
  document.getElementById('cron-timezone').value = task.timezone || CRON_DEFAULT_TIMEZONE;
  document.getElementById('cron-enabled').checked = task.enabled !== false;
}

function renderCronTaskSwitcher() {
  const select = document.getElementById('cron-task-select');
  select.innerHTML = cronEditTasks
    .map((task) => `<option value="${escapeHtmlAttr(task.name)}">${escapeHtml(task.name)}</option>`).join('');
  select.value = cronEditName;
}

// Edit schedule…: the bound task the row names opens; a session that binds more tasks switches among them.
async function openCronEditor(taskName, sessionId) {
  let bound;
  try {
    bound = (await fetchCronTasks()).filter((task) => !task.broken && task.session_id === sessionId);
  } catch (err) {
    console.error('Failed to load cron tasks:', err);
    showToast('Failed to load schedule: ' + err, true);
    return;
  }
  const task = bound.find((candidate) => candidate.name === taskName);
  if (!task) {
    showToast(`Schedule "${taskName}" not found`, true);
    return;
  }
  cronEditTasks = bound;
  cronEditName = taskName;
  cronAddSessionId = null;
  fillCronFields(task);
  renderCronTaskSwitcher();
  showCronDialog('edit');
}

// Add schedule…: the unbound manager session the menu opened on is the one the new task binds.
function openCronAdder(sessionId) {
  cronEditTasks = [];
  cronEditName = null;
  cronAddSessionId = sessionId;
  document.getElementById('cron-name').value = '';
  document.getElementById('cron-expr').value = '';
  document.getElementById('cron-timezone').value = CRON_DEFAULT_TIMEZONE;
  document.getElementById('cron-prompt-file').value = '';
  document.getElementById('cron-enabled').checked = true;
  showCronDialog('add');
}

function switchCronTask(taskName) {
  const task = cronEditTasks.find((candidate) => candidate.name === taskName);
  cronEditName = taskName;
  fillCronFields(task);
  clearCronFormError();
}

// Every dialog request shares one failure surface: a transport error or a refusal shows its text in the dialog and
// returns null, and the inputs stay as the user left them.
async function sendCronRequest(url, options) {
  let res;
  try {
    res = await fetch(url, options);
  } catch (err) {
    showCronFormError(String(err));
    return null;
  }
  if (!res.ok) {
    showCronFormError(await cronRefusalDetail(res));
    return null;
  }
  return res;
}

// A refused request answers {"detail": ...}. A body that is not JSON, such as a proxy's page, shows as the text it is.
async function cronRefusalDetail(res) {
  const text = await res.text();
  try {
    return JSON.parse(text).detail ?? text;
  } catch (parseError) {
    return text;
  }
}

// Save: Edit sends only Schedule, Timezone and Enabled; Add sends the whole new task bound to its session.
async function saveCronTask() {
  const adding = cronView === 'add';
  const cron = document.getElementById('cron-expr').value.trim();
  const timezone = document.getElementById('cron-timezone').value.trim();
  const enabled = document.getElementById('cron-enabled').checked;
  const res = adding
    ? await sendCronRequest('/api/cron/tasks', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({
        name: document.getElementById('cron-name').value.trim(),
        cron,
        timezone,
        prompt_file: document.getElementById('cron-prompt-file').value.trim(),
        enabled,
        session_id: cronAddSessionId,
      }),
    })
    : await sendCronRequest(`/api/cron/tasks/${encodeURIComponent(cronEditName)}`, {
      method: 'PUT',
      headers: JSON_HEADERS,
      body: JSON.stringify({cron, timezone, enabled}),
    });
  if (res === null) return;
  closeCronModal();
  if (platform.isMobile) closeMobileSidebar();
  showToast(adding ? 'Schedule added' : 'Schedule saved', false,
    {aboveComposer: true, reserveMessageSpace: platform.isMobile});
  Sidebar.invalidateWorkspaceMemory();
  Sidebar.refreshCurrentList();
}

// Delete: the confirmation says the schedule goes and the session stays; the binding's YAML is what is removed.
function deleteCronTask() {
  const name = cronEditName;
  Sidebar.openConfirmDialog({
    iconKey: 'delete',
    iconClass: 'text-red-400',
    heading: 'Delete schedule?',
    detail: `The schedule "${name}" is deleted. Its session and conversation history are kept.`,
    confirmLabel: 'Delete',
    confirmClass: 'bg-red-600 hover:bg-red-500',
    onConfirm: async () => {
      const res = await sendCronRequest(`/api/cron/tasks/${encodeURIComponent(name)}`, {method: 'DELETE'});
      if (res === null) return;
      closeCronModal();
      Sidebar.invalidateWorkspaceMemory();
      Sidebar.refreshCurrentList();
    },
  });
}

// The error view: one task that failed to load, read-only. The agent deletes the file.
function showCronErrorView(task) {
  document.getElementById('cron-error-box').textContent = `Load failed: ${task.error}\nFile: ${task.path}`;
  showCronDialog('error');
}

async function openCronErrorView(taskName) {
  let tasks;
  try {
    tasks = await fetchCronTasks();
  } catch (err) {
    console.error('Failed to load cron tasks:', err);
    showToast('Failed to load task: ' + err, true);
    return;
  }
  const task = tasks.find(t => t.name === taskName);
  if (!task) {
    showToast(`Task "${taskName}" not found`, true);
    return;
  }
  showCronErrorView(task);
}

// ---------------------------------------------------------------------------
// Cron error badge
// ---------------------------------------------------------------------------
// Global cron load-failure badge, rendered at the top of Workspace from the
// broken cron entries the filter fetch hands the renderer (name order).
// Clicking it opens the error view on the first broken task. Empty when
// nothing is broken.
function renderCronErrorBadge(brokenTasks) {
  const broken = brokenTasks || [];
  if (!broken.length) return '';
  return `<div class="mx-3 my-2 px-3 py-2 rounded-lg bg-red-900/40 border border-red-700/50 text-red-300 text-xs cursor-pointer"
       role="button" title="Open the first failed task"
       onclick="openCronErrorView('${escapeHtml(broken[0].name)}')">${cbIcon('warning', 12)} ${broken.length} scheduled tasks failed to load</div>`;
}

// The Workspace error badge rides this fetch: a failed cron pull yields no
// badge ([]) but never hides the session list.
async function fetchBrokenCronTasks(signal) {
  try {
    const res = await fetch('/api/cron/tasks', {signal});
    if (!res.ok) throw new Error(`cron tasks fetch failed: ${res.status}`);
    const tasks = await res.json();
    return tasks.filter(t => t.broken).sort((a, b) => a.name.localeCompare(b.name));
  } catch (err) {
    // An abort is the Workspace's own cancel: the badge load ends with it, not as an empty answer.
    if (err.name === 'AbortError') throw err;
    console.error('Cron tasks fetch failed:', err);
    return [];
  }
}

// A bound node's clock badge: blue while the task is enabled, grey with the
// Disabled line when it is not. One markup home for every view's row.
function renderScheduledBadge(s) {
  const schedule = `${s.schedule_cron} · ${s.schedule_timezone || ''}`;
  return cbIcon('scheduled', 12, `flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}`,
    `title="Scheduled: ${escapeHtmlAttr(schedule)}"`);
}

// The `last_run_status` values the scheduler stores for a schedule's last firing; the cron crate's
// field validation accepts exactly these four.
const LAST_RUN_STATUS = {
  RUNNING: 'running',
  SUCCESS: 'success',
  FAILED: 'failed',
  SKIPPED: 'skipped',
};

// The backend text a bound node's schedule lines name: a pool-bound task shows the pool label with the
// member and model its session's records carry ("Cheap → Codex GPT Luna · gpt-6-luna"), and a plain-bound
// task shows the recorded model alone. A session whose records name neither renders '' — the row keeps
// only its schedule lines, which is the true state of a task that has not run on a plain backend.
function scheduledBackendText(s) {
  const pool = typeof BACKEND_POOLS !== 'undefined' && BACKEND_POOLS.find((candidate) => candidate.id === s.backend);
  if (pool) {
    const label = Sidebar.sessionBackendLabel(s);
    return label ? label.full : '';
  }
  return s.native_model || '';
}

// The schedule slot of the task a row is bound to: the session's `task_schedule_slots` entry under that task's
// name. Each task bound to a session keeps its own entry; a row with no entry yet shows no Last line.
function boundTaskSlot(s) {
  return (s.task_schedule_slots && s.task_schedule_slots[s.schedule_task]) || {};
}

// A bound node's schedule lines, in every view: the next run (or Disabled),
// then the truncated cron · timezone line, then the backend line naming the
// member and model the records hold, then the Last line from the firing
// bookkeeping. One markup home for every view's row. The secondary lines sit
// on the sidebar's slate-850 (#172033), where slate-500 draws 3.42:1 — under
// the 4.5:1 floor for 12 px text — and slate-400 draws 6.34:1 while staying
// quieter than the row's name. The plain session row's time line keeps the
// same tone.
function renderSessionScheduleLine(s) {
  const disabled = s.schedule_enabled === false;
  const slot = boundTaskSlot(s);
  let html = `<span class="block text-xs ${disabled ? 'text-slate-400' : 'text-blue-300'}">${disabled ? 'Disabled' : 'Next: ' + relativeTime(s.schedule_next_run)}</span>`
    + `<span class="block truncate text-xs text-slate-400" title="${escapeHtmlAttr(s.schedule_cron)} (${escapeHtmlAttr(s.schedule_timezone || '')})">${escapeHtml(s.schedule_cron)} · ${escapeHtml(s.schedule_timezone || '')}</span>`;
  const backend = scheduledBackendText(s);
  if (backend) {
    html += `<span class="session-backend block truncate text-xs text-slate-400" title="${escapeHtmlAttr(backend)}">${escapeHtml(backend)}</span>`;
  }
  if (slot.last_run_status) {
    const lastClass = slot.last_run_status === LAST_RUN_STATUS.SUCCESS ? 'text-green-400'
      : slot.last_run_status === LAST_RUN_STATUS.RUNNING ? 'text-yellow-400'
        : slot.last_run_status === LAST_RUN_STATUS.SKIPPED ? 'text-slate-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400');
    // The Last line truncates with an ellipsis like the cron line above it,
    // and stays one line tall on the touch drawer too. Its full text sits in
    // the title attribute.
    const lastText = `Last: ${slot.last_run_status}${slot.last_scheduled_run ? ', ' + formatBubbleTime(slot.last_scheduled_run) : ''}${slot.last_run_status === LAST_RUN_STATUS.FAILED && s.schedule_allow_failure ? ' (review needed)' : ''}`;
    html += `<span class="block truncate text-xs ${lastClass}" title="${escapeHtmlAttr(lastText)}">${escapeHtml(lastText)}</span>`;
  }
  return html;
}

// ---------------------------------------------------------------------------
// Sidebar registrations
// ---------------------------------------------------------------------------
// The Workspace error badge rides each Workspace fetch.
registerWorkspaceBadge(signal => fetchBrokenCronTasks(signal).then(renderCronErrorBadge));
// Cron owns the listing marker, so its package exempts bound roots from the
// group preview limit here. A bound root always shows in place and takes no
// rank against the 5-row cap.
registerGroupPreviewLimitExemption((s) => !!s.schedule_task);
// A bound node's row carries the clock badge and the schedule lines, in every
// view.
registerSessionRowBadge((s) => (s.schedule_task ? renderScheduledBadge(s) : ''));
registerSessionRowLine((s) => (s.schedule_task ? renderSessionScheduleLine(s) : null));
// A bound session's Task & context dialog shows the Scheduled task section.
registerTaskContextSection(loadScheduledTaskSection);
// The row's Settings menu: a bound row edits its schedule, and an unbound manager row adds one bound to the row.
registerSessionMenuItems({
  rowAttrs: (s) => ({scheduleTask: s.schedule_task || ''}),
  items: (attrs, sessionId) => {
    if (attrs.scheduleTask) {
      return [{label: 'Edit schedule…', onSelect: () => openCronEditor(attrs.scheduleTask, sessionId)}];
    }
    if (attrs.profile === 'manager') {
      return [{label: 'Add schedule…', onSelect: () => openCronAdder(sessionId)}];
    }
    return [];
  },
});

const GLOBALS = {
  openCronErrorView,
  closeCronModal,
  saveCronTask,
  deleteCronTask,
  switchCronTask,
};
Sidebar.wire(GLOBALS, {showCronErrorView});

// The page fetches this module after its document, so the first repaint runs here when the document has already parsed.
if (document.readyState !== 'loading') {
  // The sidebar's rows were already painted without the schedule badge and line registered above. Repaint the rows
  // the sidebar still shows, then re-read the Workspace badges with this module's loader in the list.
  Sidebar.repaintRenderedRows();
  Sidebar.refreshWorkspaceBadges();
}

})();
