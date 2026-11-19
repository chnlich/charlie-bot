// ---------------------------------------------------------------------------
// Scheduled tasks (cron): the read-only task panel under a bound session's
// header, the error view of a task that failed to load, the Workspace error
// badge, and a bound node's clock badge and schedule lines. Every task setting
// is changed by the session's agent through /api/cron or the task file; no
// control here writes one. The sidebar reaches this file through the entries it
// registers with.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Read-only task panel
// ---------------------------------------------------------------------------
// The panel lists the tasks bound to the active session, read from GET
// /api/cron/tasks. It sends no write request and has no input, select, or
// button. It redraws when the active session changes; no timer polls it.
function cronPanelValue(value) {
  return value === null || value === undefined || value === '' ? '—' : String(value);
}

function renderCronPanelRow(label, value) {
  return `<div class="flex gap-2"><span class="w-28 flex-shrink-0 text-slate-400">${escapeHtml(label)}</span>`
    + `<span class="min-w-0 break-words text-slate-200">${escapeHtml(cronPanelValue(value))}</span></div>`;
}

function renderCronPanelPrompt(label, prompt) {
  return `<div class="flex flex-col gap-1"><span class="text-slate-400">${escapeHtml(label)}</span>`
    + `<pre class="whitespace-pre-wrap break-words rounded bg-slate-900/60 px-2 py-1 font-mono text-slate-200">${escapeHtml(cronPanelValue(prompt))}</pre></div>`;
}

// One task's block: the schedule as its title, then every setting in the order the file lists them, then each step
// of a steps task. The task's name sits in small type, since agents address the task by it.
function renderCronPanelTask(task) {
  const disabled = task.enabled === false;
  let html = `<div class="flex flex-col gap-1.5 border-b border-slate-700/50 pb-2 last:border-b-0">`
    + `<div class="flex items-baseline gap-2"><span class="text-sm font-medium text-slate-200">${escapeHtml(cronPanelValue(task.cron))}</span>`
    + (disabled ? '<span class="text-amber-400">Disabled</span>' : '')
    + `</div><span class="font-mono text-[11px] text-slate-400">${escapeHtml(task.name)}</span>`
    + renderCronPanelRow('Session ID', task.session_id)
    + renderCronPanelRow('Cron', task.cron)
    + renderCronPanelRow('Timezone', task.timezone)
    + renderCronPanelRow('Enabled', task.enabled ? 'yes' : 'no')
    + renderCronPanelRow('Prompt file', task.prompt_file)
    + renderCronPanelPrompt('Prompt', task.prompt)
    + renderCronPanelRow('Repo', task.repo)
    + renderCronPanelRow('Allow failure', task.allow_failure ? 'yes' : 'no')
    + renderCronPanelRow('Mode', task.mode)
    + renderCronPanelRow('Handler', task.handler)
    + renderCronPanelRow('Improve host', task.improve_host);
  for (const step of task.steps || []) {
    html += `<div class="flex flex-col gap-1 rounded border border-slate-700/50 px-2 py-1.5">`
      + renderCronPanelRow('Step', step.name)
      + renderCronPanelRow('Backend', step.backend)
      + renderCronPanelRow('Distinct from', step.distinct_backend_from)
      + renderCronPanelRow('Prompt file', step.prompt_file)
      + renderCronPanelPrompt('Prompt', step.prompt)
      + '</div>';
  }
  return html + '</div>';
}

async function renderCronPanel() {
  const panel = document.getElementById('cron-session-panel');
  if (!panel) return;
  const body = document.getElementById('cron-session-panel-body');
  const sessionId = SESSION_ID;
  panel.classList.add('hidden');
  body.textContent = '';
  let tasks = [];
  let error = null;
  if (sessionId) {
    try {
      const res = await fetch('/api/cron/tasks');
      if (!res.ok) throw new Error(`cron tasks fetch failed: ${res.status}`);
      tasks = (await res.json()).filter((task) => task.session_id === sessionId);
    } catch (err) {
      console.error('Cron panel fetch failed:', err);
      error = err;
    }
  }
  // A session switch that landed during the fetch owns the panel now.
  if (sessionId !== SESSION_ID) return;
  if (error) {
    body.textContent = `Could not load scheduled tasks: ${error}`;
    panel.classList.remove('hidden');
    return;
  }
  panel.classList.toggle('hidden', tasks.length === 0);
  body.innerHTML = tasks.map(renderCronPanelTask).join('');
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
registerWorkspaceBadge(() => fetchBrokenCronTasks().then(renderCronErrorBadge));
// Cron owns the listing marker, so its package exempts bound roots from the
// group preview limit here. A bound root always shows in place and takes no
// rank against the 5-row cap.
registerGroupPreviewLimitExemption((s) => !!s.schedule_task);
// A bound node's row carries the clock badge and the schedule lines, in every
// view.
registerSessionRowBadge((s) => (s.schedule_task ? renderScheduledBadge(s) : ''));
registerSessionRowLine((s) => (s.schedule_task ? renderSessionScheduleLine(s) : null));

const GLOBALS = {
  openCronErrorView,
  closeCronModal,
};
Sidebar.wire(GLOBALS, {showCronErrorView, renderCronPanel});

// The panel follows the active session: it redraws at each switch. The page fetches this module after its document,
// so the first draw runs here when the document has already parsed.
document.addEventListener('charliebot:active-session-changed', renderCronPanel);
if (document.readyState !== 'loading') {
  renderCronPanel();
  // The sidebar's rows were already painted without the schedule badge and line registered above. Repaint the rows
  // the sidebar still shows, then re-read the Workspace badges with this module's loader in the list.
  Sidebar.repaintRenderedRows();
  Sidebar.refreshWorkspaceBadges();
}

})();
