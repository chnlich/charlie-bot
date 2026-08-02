(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Tracks server-reported unread state per session so we can restore the
// unread dot after the spinner hides.
globalThis.TuiStatusMap = globalThis.TuiStatusMap || {};

// Status polls are scoped to the sessions the sidebar is actually rendering.
// The URL stays well under the 8 KB request-line budget at the ~23 sessions the
// sidebar shows, but a longer list is split so no single request can overflow.
const STATUS_QUERY_MAX_BYTES = 8192;

function sidebarSessionIds() {
  const ids = [];
  const seen = new Set();
  if (typeof SESSION_ID !== 'undefined' && SESSION_ID) {
    seen.add(SESSION_ID);
    ids.push(SESSION_ID);
  }
  document.querySelectorAll('a[id^="session-"]').forEach(el => {
    const sid = el.id.slice('session-'.length);
    if (!sid || seen.has(sid)) return;
    seen.add(sid);
    ids.push(sid);
  });
  return ids;
}

function statusRequestUrls(path, ids) {
  const urlFor = (batch) => path + '?ids=' + batch.map(encodeURIComponent).join(',');
  const urls = [];
  let batch = [];
  const flush = () => {
    if (batch.length) urls.push(urlFor(batch));
    batch = [];
  };
  ids.forEach(sid => {
    if (batch.length && urlFor(batch.concat([sid])).length > STATUS_QUERY_MAX_BYTES) flush();
    batch.push(sid);
  });
  flush();
  return urls;
}

async function fetchScopedStatus(path) {
  const ids = sidebarSessionIds();
  if (!ids.length) return {};
  const responses = await Promise.all(statusRequestUrls(path, ids).map(url => fetch(url)));
  const merged = {};
  for (const r of responses) {
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    Object.assign(merged, await r.json());
  }
  return merged;
}

function sessionBackendType(session) {
  return session && session.backend && typeof BACKEND_TYPES !== 'undefined' ? (BACKEND_TYPES[session.backend] || '') : '';
}

function isTuiSession(session) {
  return sessionBackendType(session) === 'tui-cli';
}

function renderTuiStatusDot(session) {
  if (!isTuiSession(session)) return '';
  const status = globalThis.TuiStatusMap[session.id] || {running: false, busy: false};
  const classes = ['tui-status-dot', 'w-2', 'h-2', 'rounded-full', 'flex-shrink-0'];
  if (status.running) classes.push('running');
  if (status.running && status.busy) classes.push('busy');
  const title = !status.running ? 'Claude stopped' : (status.busy ? 'Claude busy' : 'Claude idle');
  return `<span class="${classes.join(' ')}" data-session-id="${escapeHtmlAttr(session.id)}" title="${title}"></span>`;
}

async function fetchTuiStatus() {
  try {
    globalThis.TuiStatusMap = await fetchScopedStatus('/api/sessions/tui/status');
    refreshTuiDots();
  } catch (err) {
    console.error('fetchTuiStatus failed:', err);
  }
}

function refreshTuiDots() {
  document.querySelectorAll('.tui-status-dot[data-session-id]').forEach(dot => {
    const id = dot.dataset.sessionId;
    const status = globalThis.TuiStatusMap[id] || {running: false, busy: false};
    const running = !!status.running;
    const busy = running && !!status.busy;
    if (dot.classList.contains('running') !== running) dot.classList.toggle('running', running);
    if (dot.classList.contains('busy') !== busy) dot.classList.toggle('busy', busy);
    const title = !running ? 'Claude stopped' : (busy ? 'Claude busy' : 'Claude idle');
    if (dot.title !== title) dot.title = title;
  });
  updateBackendHeaderControls(globalThis.ACTIVE_BACKEND_TYPE || '', SESSION_ID);
}

function startTuiStatusPolling() {
  if (pageTimerRegistered('tui-status')) return;
  fetchTuiStatus();
  startPageTimer('tui-status', fetchTuiStatus, 3000);
}

function compactButtonTitle(backendType) {
  if (backendType === 'cc-claude') return '';
  if (backendType === 'codex') return 'codex only compacts automatically — tune model_auto_compact_token_limit';
  return 'Manual compaction is not supported on this backend';
}

function updateBackendHeaderControls(backendType, sessionId) {
  const stopBtn = document.getElementById('stop-tui-btn');
  if (stopBtn) {
    const isTui = backendType === 'tui-cli';
    const stopped = isTui && globalThis.TuiStatusMap[sessionId]?.running === false;
    stopBtn.classList.toggle('hidden', !isTui || stopped);
    stopBtn.dataset.sessionId = isTui ? sessionId : '';
  }

  const compactBtn = document.getElementById('compact-btn');
  if (compactBtn) {
    compactBtn.disabled = backendType !== 'cc-claude';
    compactBtn.title = compactButtonTitle(backendType);
  }
}

function updateSidebarSessionName(sessionId, name) {
  const link = document.getElementById('session-' + sessionId);
  if (!link) return;
  const nameEl = link.querySelector('.session-name');
  if (!nameEl) return;
  nameEl.textContent = name;
}

function getSessionIndicatorState(status) {
  if (status.thinking_since) return 'thinking';
  if (status.has_running_tasks) return 'worker_only';
  return 'idle';
}

function pendingTriggerTitle(count) {
  const normalized = Number(count) || 0;
  if (normalized === 1) return '1 pending delayed trigger';
  if (normalized > 1) return `${normalized} pending delayed triggers`;
  return 'Pending delayed trigger';
}

function renderPendingTriggerIndicator(session) {
  const count = Number(session.pending_trigger_count) || 0;
  return `<svg id="pending-trigger-${session.id}" data-count="${count}" data-next-trigger-at="${escapeHtmlAttr(session.next_trigger_at || '')}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 ${session.has_pending_trigger ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="${escapeHtmlAttr(pendingTriggerTitle(count))}"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 17h5l-1.405-1.405A2.032 2.032 0 0118 14.158V11a6.002 6.002 0 00-4-5.659V5a2 2 0 10-4 0v.341C7.67 6.165 6 8.388 6 11v3.159c0 .538-.214 1.055-.595 1.436L4 17h5m6 0a3 3 0 01-6 0"/></svg>`;
}

function renderPendingPlanApprovalIndicator(session) {
  const hasPending = !!(session && session.has_pending_plan_approval);
  return `<svg id="pending-plan-approval-${session.id}" class="w-3.5 h-3.5 text-blue-400 flex-shrink-0 ${hasPending ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Plan awaiting approval"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4"/></svg>`;
}


function updateSidebarHighlight(newSessionId) {
  document.querySelectorAll('[id^="session-"]').forEach(el => {
    if (!el.id.startsWith('session-')) return;
    el.classList.remove('bg-blue-600/20', 'text-blue-300');
    el.classList.add('hover:bg-slate-700/50', 'text-slate-300');
    el.querySelectorAll('.group-hover\\:opacity-100').forEach(btn => {
      if (btn.classList.contains('star-btn') && btn.classList.contains('text-yellow-400')) return;
      btn.classList.remove('!opacity-100');
    });
  });
  const active = document.getElementById('session-' + newSessionId);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('hover:bg-slate-700/50', 'text-slate-300');
    active.querySelectorAll('.group-hover\\:opacity-100').forEach(btn => {
      btn.classList.add('!opacity-100');
    });
  }
}

function setSessionIndicator(sid, state) {
  const spinner = document.getElementById('spinner-' + sid);
  const worker = document.getElementById('worker-indicator-' + sid);
  const dot = document.getElementById('unread-' + sid);
  if (spinner) spinner.classList.toggle('hidden', state !== 'thinking');
  if (worker) worker.classList.toggle('hidden', state !== 'worker_only');
  if (dot) dot.classList.toggle('hidden', state !== 'idle' || !sessionUnread[sid]);
}

function setSessionSpinner(sid, visible) {
  return refreshSessionStatusNow();
}

function setSessionPendingTriggerIndicator(sid, status) {
  const icon = document.getElementById('pending-trigger-' + sid);
  if (!icon) return;
  const hasPending = !!(status && status.has_pending_trigger);
  const count = Number(status && status.pending_trigger_count) || 0;
  icon.classList.toggle('hidden', !hasPending);
  icon.dataset.count = String(count);
  icon.dataset.nextTriggerAt = (status && status.next_trigger_at) || '';
  icon.title = pendingTriggerTitle(count);
}

function setSessionPendingPlanApprovalIndicator(sid, status) {
  const icon = document.getElementById('pending-plan-approval-' + sid);
  if (!icon) return;
  const hasPending = !!(status && status.has_pending_plan_approval);
  icon.classList.toggle('hidden', !hasPending);
}

function updateSpinner() {
  return refreshSessionStatusNow();
}

let activeSessionViewPollInflight = false;
const ACTIVE_SESSION_VIEW_POLL_MS = 3000;

function stopActiveSessionViewPolling() {
  stopPageTimer('active-session-view');
}

function ensureActiveSessionViewPolling() {
  if (!SESSION_ID || (!masterThinking && !THINKING_SINCE)) {
    stopActiveSessionViewPolling();
    return;
  }
  if (pageTimerRegistered('active-session-view')) return;
  startPageTimer('active-session-view', pollActiveSessionView, ACTIVE_SESSION_VIEW_POLL_MS);
}

async function pollActiveSessionView(opts) {
  const force = opts && opts.force;
  if (activeSessionViewPollInflight || !SESSION_ID || (!force && !masterThinking && !THINKING_SINCE)) return;

  const pollSessionId = SESSION_ID;
  activeSessionViewPollInflight = true;
  try {
    const res = await fetch('/api/sessions/' + pollSessionId + '/usage');
    if (!res.ok) throw new Error(res.status);
    const data = await res.json();
    if (pollSessionId !== SESSION_ID) return;

    THINKING_SINCE = data.session.thinking_since || null;
    setActiveBackendId(data.active_backend);
    updateActiveBackendBadges();
    globalThis.renderUsageFromData(data.usage);

    if (!THINKING_SINCE && masterThinking) {
      globalThis.stopThinking();
    }
  } catch (err) {
    console.error('pollActiveSessionView failed:', err);
  } finally {
    activeSessionViewPollInflight = false;
    globalThis.ensureActiveSessionViewPolling();
  }
}

// Poll-based sidebar status (corrects WS drift)
let statusPollInflight = false;
let statusPollPromise = Promise.resolve(false);
let statusPollQueued = false;

function applySessionStatus(sid, status) {
  sessionUnread[sid] = status.has_unread;
  globalThis.setSessionIndicator(sid, getSessionIndicatorState(status));
  globalThis.setSessionPendingTriggerIndicator(sid, status);
  globalThis.setSessionPendingPlanApprovalIndicator(sid, status);
}

function refreshSessionStatusNow(opts) {
  if (opts && opts.refreshWorkers) pollWorkers();
  if (!statusPollInflight) return globalThis.pollSessionStatus();

  statusPollQueued = true;
  return statusPollPromise.then(() => {
    if (!statusPollQueued) return false;
    statusPollQueued = false;
    return globalThis.pollSessionStatus();
  });
}

function pollSessionStatus() {
  if (statusPollInflight) return statusPollPromise;
  statusPollInflight = true;
  statusPollPromise = fetchScopedStatus('/api/sessions/status')
    .then(data => {
      if (!data) return false;
      let anyRunning = false;
      for (const [sid, st] of Object.entries(data)) {
        applySessionStatus(sid, st);
        if (st.has_running_tasks) anyRunning = true;
      }
      return anyRunning;
    })
    .catch(err => {
      console.error('pollSessionStatus failed:', err);
      return false;
    })
    .finally(() => { statusPollInflight = false; });
  return statusPollPromise;
}


// ---------------------------------------------------------------------------
// Thinking indicator
// ---------------------------------------------------------------------------
function startThinking(opts) {
  masterThinking = true;
  thinkingStart = thinkingStart || Date.now();
  document.getElementById('thinking').classList.remove('hidden');
  updateThinkingTime();
  startPageTimer('thinking-tick', updateThinkingTime, 1000);
  if (!(opts && opts.keepSendEnabled)) {
    document.getElementById('send-btn').disabled = true;
    document.getElementById('send-btn').classList.add('opacity-50');
  }
  globalThis.ensureActiveSessionViewPolling();
}

function stopThinking(opts) {
  masterThinking = false;
  document.getElementById('thinking').classList.add('hidden');
  stopPageTimer('thinking-tick');
  thinkingStart = null;
  document.getElementById('send-btn').disabled = false;
  document.getElementById('send-btn').classList.remove('opacity-50');
  stopActiveSessionViewPolling();
  if (!switching && !(opts && opts.preserveSessionIndicator)) updateSpinner();
}

function updateThinkingTime() {
  if (!thinkingStart) return;
  const secs = Math.floor((Date.now() - thinkingStart) / 1000);
  document.getElementById('thinking-time').textContent = secs + 's';
}

async function cancelMaster() {
  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/cancel`, { method: 'POST' });
    if (res.ok) return;

    let detail = '';
    try {
      const body = await res.json();
      detail = body.detail || body.error || body.message || '';
    } catch (_err) {
      detail = '';
    }

    if (res.status === 404 && detail === 'No active master agent') {
      // Backend already broadcasts a visible assistant_error for this case.
      return;
    }

    const suffix = detail || (`HTTP ${res.status}`);
    showToast(`Cancel failed: ${suffix}`, true);
    console.error('Cancel master failed:', res.status, detail || res.statusText);
  } catch (err) {
    showToast('Cancel failed: network error. Please try again.', true);
    console.error('Cancel master failed:', err);
  }
}


Object.assign(Sidebar, {
  sidebarSessionIds,
  statusRequestUrls,
  fetchScopedStatus,
  sessionBackendType,
  isTuiSession,
  renderTuiStatusDot,
  fetchTuiStatus,
  refreshTuiDots,
  startTuiStatusPolling,
  updateBackendHeaderControls,
  updateSidebarSessionName,
  getSessionIndicatorState,
  pendingTriggerTitle,
  renderPendingTriggerIndicator,
  renderPendingPlanApprovalIndicator,
  updateSidebarHighlight,
  setSessionIndicator,
  setSessionSpinner,
  setSessionPendingTriggerIndicator,
  setSessionPendingPlanApprovalIndicator,
  updateSpinner,
  stopActiveSessionViewPolling,
  ensureActiveSessionViewPolling,
  pollActiveSessionView,
  applySessionStatus,
  refreshSessionStatusNow,
  pollSessionStatus,
  startThinking,
  stopThinking,
  updateThinkingTime,
  cancelMaster,
});
Sidebar.expose([
  'sidebarSessionIds',
  'statusRequestUrls',
  'fetchScopedStatus',
  'sessionBackendType',
  'isTuiSession',
  'renderTuiStatusDot',
  'fetchTuiStatus',
  'refreshTuiDots',
  'startTuiStatusPolling',
  'updateBackendHeaderControls',
  'updateSidebarSessionName',
  'getSessionIndicatorState',
  'pendingTriggerTitle',
  'renderPendingTriggerIndicator',
  'renderPendingPlanApprovalIndicator',
  'updateSidebarHighlight',
  'setSessionIndicator',
  'setSessionSpinner',
  'setSessionPendingTriggerIndicator',
  'setSessionPendingPlanApprovalIndicator',
  'updateSpinner',
  'stopActiveSessionViewPolling',
  'ensureActiveSessionViewPolling',
  'pollActiveSessionView',
  'applySessionStatus',
  'refreshSessionStatusNow',
  'pollSessionStatus',
  'startThinking',
  'stopThinking',
  'updateThinkingTime',
  'cancelMaster',
]);

})();
