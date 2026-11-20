// ---------------------------------------------------------------------------
// Scheduled tasks (cron): the Scheduled task section of a bound session's
// Task & context dialog, the error view of a task that failed to load, the
// Workspace error badge, and a bound node's clock badge and schedule lines.
// Every task setting is changed by the session's agent through /api/cron or
// the task file; no control here writes one. The sidebar reaches this file
// through the entries it registers with.
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
// The section lists the tasks bound to the session, read from GET /api/cron/tasks each time the dialog opens: the
// schedule sentence, cron, timezone, enabled, the prompt file, and the whole prompt in a block that scrolls. It holds
// no control. A session no task binds gets no section. A failed fetch shows in the section, so an unreadable task list
// does not look like an unbound session.
function scheduledTaskField(label, valueHtml) {
  return `<div><div class="text-xs text-slate-400 mb-1">${escapeHtml(label)}</div>`
    + `<div class="break-words">${valueHtml}</div></div>`;
}

// One bound task: its schedule sentence heads the block, its name sits under it in small type since agents address the
// task by it, then its settings and its prompt text.
function renderScheduledTask(task) {
  return `<div class="flex flex-col gap-3 border-t border-slate-700/60 pt-2 first:border-t-0 first:pt-0">`
    + `<div><div class="text-sm font-medium text-slate-200">${escapeHtml(cronScheduleTitle(task.cron))}</div>`
    + `<div class="font-mono text-[11px] text-slate-400">${escapeHtml(cronValue(task.name))}</div></div>`
    + scheduledTaskField('Cron', escapeHtml(cronValue(task.cron)))
    + scheduledTaskField('Timezone', escapeHtml(cronValue(task.timezone)))
    + scheduledTaskField('Enabled', task.enabled ? 'yes' : 'no')
    + scheduledTaskField('Prompt file', escapeHtml(cronValue(task.prompt_file)))
    + scheduledTaskField('Prompt', '<pre class="max-h-56 overflow-y-auto whitespace-pre-wrap break-words rounded '
      + `bg-slate-900/60 px-2 py-1 font-mono text-xs text-slate-200">${escapeHtml(cronValue(task.prompt))}</pre>`)
    + '</div>';
}

function scheduledTaskSection(body) {
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Scheduled task</div>
    ${body}
  </div>`;
}

async function loadScheduledTaskSection(sessionId) {
  let tasks;
  try {
    const res = await fetch('/api/cron/tasks');
    if (!res.ok) throw new Error(`cron tasks fetch failed: ${res.status}`);
    tasks = (await res.json()).filter((task) => task.session_id === sessionId);
  } catch (err) {
    console.error('Scheduled task fetch failed:', err);
    return scheduledTaskSection(
      `<div class="text-xs text-amber-400">Could not load scheduled tasks: ${escapeHtml(String(err))}</div>`);
  }
  return tasks.length === 0 ? '' : scheduledTaskSection(tasks.map(renderScheduledTask).join(''));
}

// ---------------------------------------------------------------------------
// Task error view
// ---------------------------------------------------------------------------
// The modal shows one task that failed to load: the load error and the file
// path. It has no form and no control; the agent deletes the file.
function showCronErrorView(task) {
  document.getElementById('cron-error-box').textContent = `Load failed: ${task.error}\nFile: ${task.path}`;
  document.getElementById('cron-modal').classList.remove('hidden');
}

async function openCronErrorView(taskName) {
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
  showCronErrorView(task);
}

function closeCronModal() {
  document.getElementById('cron-modal').classList.add('hidden');
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

const GLOBALS = {
  openCronErrorView,
  closeCronModal,
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
