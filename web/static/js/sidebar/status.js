(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Tmux liveness and recent jsonl activity per tui-cli session id
// ({running, busy}), as returned by the /api/sessions/tui/status poll.
globalThis.TuiStatusMap = globalThis.TuiStatusMap || {};

// Status polls are scoped to the sessions the sidebar is actually rendering.
// The URL stays well under the 8 KB request-line budget at the ~23 sessions the
// sidebar shows, but a longer list is split so no single request can overflow.
const STATUS_QUERY_MAX_BYTES = 8192;

// Status of every rendered sidebar row, keyed by session id — the same
// rendered-truth pattern as renderedSessionBackendTypes below. The status/tui
// polls skip archived rows through it: their server probe is the constant-False
// shortcut (src/core/sessions.py populate_sidebar_state), so per-cycle request
// volume tracks the active rows on screen, not the archived list length.
const renderedSessionStatuses = {};

// Rows painted with profile=worker: a leaf runs its own work, so its running
// state paints as the spinner rather than the delegated-work gear.
const renderedWorkerRows = new Set();
// Rows painted as task-tree nodes (profile not null, not a projected legacy
// worker-thread leaf): their own running state is a live Run of their own and
// paints as the spinner too — the gear is exclusively a collapsed row's
// stand-in for a running descendant.
const renderedTaskTreeRows = new Set();

// A projected legacy worker-thread leaf (profile 'worker' plus worker_thread)
// is a legacy row: it keeps the legacy derivation and the legacy mapping.
function isTaskTreeRow(session) {
  return !!session && session.profile != null && !session.worker_thread;
}

function recordRenderedSessionStatus(session) {
  renderedSessionStatuses[session.id] = (session && session.status) || 'active';
  // The row's own activity as painted; the indicator aggregation reads it.
  ownIndicatorState[session.id] = getSessionIndicatorState(session);
  if (session && session.profile === 'worker') renderedWorkerRows.add(session.id);
  else renderedWorkerRows.delete(session.id);
  if (isTaskTreeRow(session)) renderedTaskTreeRows.add(session.id);
  else renderedTaskTreeRows.delete(session.id);
}

function displayIndicatorState(sid, state) {
  if (state === 'worker_only'
      && (renderedWorkerRows.has(sid) || renderedTaskTreeRows.has(sid))) return 'thinking';
  return state;
}

function sidebarSessionIds() {
  const ids = [];
  const seen = new Set();
  if (typeof SESSION_ID !== 'undefined' && SESSION_ID) {
    seen.add(SESSION_ID);
    ids.push(SESSION_ID);
  }
  // Delegated cards in the open chat read their child's live state from this
  // poll; a child whose sidebar row is filtered out still rides the request.
  document.querySelectorAll('.delegate-live-state[data-delegate-session]').forEach(el => {
    const sid = el.dataset.delegateSession;
    if (!sid || seen.has(sid)) return;
    seen.add(sid);
    ids.push(sid);
  });
  document.querySelectorAll('a[id^="session-"]').forEach(el => {
    const sid = el.id.slice('session-'.length);
    if (!sid || seen.has(sid)) return;
    if (renderedSessionStatuses[sid] === 'archived') return;
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

async function fetchScopedStatus(path, ids) {
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

// Backend type of every rendered sidebar row, keyed by session id. The sidebar
// re-renders its rows wholesale on every refresh, and each row render flows
// through renderTuiStatusDot, so this map tracks the rendered truth; the
// tui/status poll reads it to request only tui-cli rows.
const renderedSessionBackendTypes = {};

function renderTuiStatusDot(session) {
  renderedSessionBackendTypes[session.id] = sessionBackendType(session);
  if (!isTuiSession(session)) return '';
  const status = globalThis.TuiStatusMap[session.id] || {running: false, busy: false};
  const classes = ['tui-status-dot', 'w-2', 'h-2', 'rounded-full', 'flex-shrink-0'];
  if (status.running) classes.push('running');
  if (status.running && status.busy) classes.push('busy');
  const title = !status.running ? 'Claude stopped' : (status.busy ? 'Claude busy' : 'Claude idle');
  return `<span class="${classes.join(' ')}" data-session-id="${escapeHtmlAttr(session.id)}" title="${title}"></span>`;
}

// The poll goes out only for sessions that can consume the answer: rows
// rendered as tui-cli, plus the active session whenever its current backend
// type is tui-cli — the Stop button reads TuiStatusMap[SESSION_ID] and the
// active session's row can be missing or stale right after a backend switch.
// With no tui-cli sessions the request is skipped entirely.
function tuiSidebarSessionIds() {
  const ids = sidebarSessionIds().filter(sid => renderedSessionBackendTypes[sid] === 'tui-cli');
  if (typeof SESSION_ID !== 'undefined' && SESSION_ID &&
      globalThis.ACTIVE_BACKEND_TYPE === 'tui-cli' && !ids.includes(SESSION_ID)) {
    ids.unshift(SESSION_ID);
  }
  return ids;
}

async function fetchTuiStatus() {
  try {
    globalThis.TuiStatusMap = await fetchScopedStatus('/api/sessions/tui/status', tuiSidebarSessionIds());
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
  // A task-tree row's fact-derived work verdict (idle | running | waiting |
  // attention) when its sidebar state carries one. has_running_tasks already
  // covered 'running'; the remaining verdicts map onto their own icons.
  if (status.work_state === 'attention') return 'attention';
  if (status.work_state === 'waiting') return 'waiting';
  return 'idle';
}

// ---------------------------------------------------------------------------
// Unread facts — one application seam with response-ordering protection
// ---------------------------------------------------------------------------
// sessionUnread holds the client's unread truth per session. Server facts land
// through three channels — unread_changed broadcasts, the scoped status poll,
// and the task tree's row fetches — and a reply can carry a snapshot older
// than a fact this client already applied (a poll issued before the flip, a
// tree page read from a pre-flip projection). Every applied fact is stamped
// with a per-session sequence; a reply captured before the newest stamp is
// refused, so a stale reply can neither erase a newer unread nor resurrect a
// cleared one. Broadcasts are ordered by the socket and always apply.
let unreadFactSeq = 0;
const unreadFactSeqBySession = {};

function recordUnreadFact(sessionId, hasUnread) {
  sessionUnread[sessionId] = !!hasUnread;
  unreadFactSeqBySession[sessionId] = ++unreadFactSeq;
}

function unreadSeqAtRequest() {
  return unreadFactSeq;
}

function unreadReplyIsCurrent(sessionId, requestSeq) {
  return requestSeq >= (unreadFactSeqBySession[sessionId] || 0);
}

// The unread state a row renders: the map when this client knows the session
// (a broadcast, a polled fact, or a fetched row), otherwise the row's own
// server fact (a search hit that has not been through a level fetch yet).
function rowUnreadState(sessionId, serverValue) {
  return typeof sessionUnread[sessionId] === 'boolean' ? sessionUnread[sessionId] : !!serverValue;
}

// ---------------------------------------------------------------------------
// Activity indicators — one owner, one visual language
// ---------------------------------------------------------------------------
// The thinking spinner and the delegated-work gear are this module's SVGs and
// this module's element ids (spinner-<id>, worker-indicator-<id>): every row
// that renders them is patchable by setSessionIndicator, whatever view built
// it.
const SPINNER_SVG_INNER =
  '<circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/>'
  + '<path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/>';
const GEAR_CENTER_PATH =
  '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/>';
const SPINNER_TITLE = 'Task is running';
const GEAR_TITLE = 'Delegated work running in subtasks';
// The attention alert and the waiting clock complete the visual language:
// one activity icon per row, chosen by the priority table in
// paintSessionIndicator (own state first, then a collapsed row's stand-in).
const ALERT_TITLE = 'Task needs attention';
const CLOCK_TITLE = 'Task waiting to run';
const ALERT_SVG_PATH =
  '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" '
  + 'd="M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>';
const CLOCK_SVG_PATH =
  '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" '
  + 'd="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z"/>';

function gearSvgContent() {
  return (Sidebar.GEAR_SVG_PATH || '') + GEAR_CENTER_PATH;
}

// The unread dot: the legacy rows' familiar yellow pulse, pinned to the
// unread-<id> element id the row re-render path and the unread_changed
// handler both address. Hidden while any activity cue shows (the dot is an
// idle-state cue; activity hides it without discarding the flag).
const UNREAD_TITLE = 'Unread reply';

// Thinking spinner, worker gear, unread dot: one session row's header indicators.
// setSessionIndicator toggles each element by id, so the three id prefixes are pinned.
function renderSessionIndicators(session) {
  const ownState = getSessionIndicatorState(session);
  const indicatorState = displayIndicatorState(session.id, ownState);
  return `<svg id="spinner-${session.id}" title="${escapeHtmlAttr(SPINNER_TITLE)}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24">${SPINNER_SVG_INNER}</svg>
    <svg id="worker-indicator-${session.id}" title="${escapeHtmlAttr(GEAR_TITLE)}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24">${gearSvgContent()}</svg>
    <svg id="alert-indicator-${session.id}" title="${escapeHtmlAttr(ALERT_TITLE)}" class="w-3.5 h-3.5 text-red-500 flex-shrink-0 ${indicatorState === 'attention' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24">${ALERT_SVG_PATH}</svg>
    <svg id="waiting-indicator-${session.id}" title="${escapeHtmlAttr(CLOCK_TITLE)}" class="w-3.5 h-3.5 text-slate-500 flex-shrink-0 ${indicatorState === 'waiting' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24">${CLOCK_SVG_PATH}</svg>
    <span id="unread-${session.id}" data-has-unread="${session.has_unread ? 1 : 0}" title="${escapeHtmlAttr(UNREAD_TITLE)}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${session.has_unread && indicatorState === 'idle' ? '' : 'hidden'}"></span>`;
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

// Indicator priority over the nested sidebar. A row's own activity comes
// first, in this order: own thinking or running -> the yellow spinner; own
// attention -> the red alert; own waiting (queued, not launched) -> the muted
// clock. A collapsed parent whose own state is idle then stands in for its
// subtree in the same order: a running descendant shows the gear, an
// attention descendant the alert, a waiting descendant the clock. An expanded
// parent shows its own facts only, since the descendants show theirs. The
// unread dot shows only when no activity icon does. The facts are the latest
// applied state per row (list paint, status poll, task_tree_changed /
// running_changed broadcasts) and the shared unread map; the tree shape comes
// from the last grouped paint.
const ownIndicatorState = {};

function treeChildIdsOf(sid) {
  return Sidebar.treeChildIds ? Sidebar.treeChildIds(sid) : [];
}

function treeStandsInForSubtree(sid) {
  if (!treeChildIdsOf(sid).length) return false;
  return !(Sidebar.isTreeNodeExpanded && Sidebar.isTreeNodeExpanded(sid));
}

function subtreeHasState(sid, state) {
  return treeChildIdsOf(sid).some(
      child => (ownIndicatorState[child] || 'idle') === state || subtreeHasState(child, state));
}

function subtreeHasActivity(sid) {
  return treeChildIdsOf(sid).some(child => (ownIndicatorState[child] || 'idle') !== 'idle' || subtreeHasActivity(child));
}

function subtreeHasUnread(sid) {
  return treeChildIdsOf(sid).some(child => !!sessionUnread[child] || subtreeHasUnread(child));
}

// The collapsed stand-in verdict, in the same priority order as the row's own
// state: a running descendant (the gear), then attention (the alert), then
// waiting (the clock).
function subtreeStandInState(sid) {
  if (!treeStandsInForSubtree(sid)) return 'idle';
  if (subtreeHasState(sid, 'worker_only')) return 'worker_only';
  if (subtreeHasState(sid, 'attention')) return 'attention';
  if (subtreeHasState(sid, 'waiting')) return 'waiting';
  return 'idle';
}

function effectiveIndicatorState(sid) {
  const own = ownIndicatorState[sid] || 'idle';
  if (own !== 'idle') return own;
  return subtreeStandInState(sid);
}

function effectiveUnread(sid) {
  if (sessionUnread[sid]) return true;
  return treeStandsInForSubtree(sid) && subtreeHasUnread(sid);
}

function paintSessionIndicator(sid) {
  // The display mapping (a row's own running Run reads as the spinner) applies
  // only to the row's own state; a collapsed stand-in keeps its own icon
  // vocabulary — the gear for a running descendant, the alert, the clock.
  const own = ownIndicatorState[sid] || 'idle';
  const state = own !== 'idle' ? displayIndicatorState(sid, own) : subtreeStandInState(sid);
  const spinner = document.getElementById('spinner-' + sid);
  const worker = document.getElementById('worker-indicator-' + sid);
  const alert = document.getElementById('alert-indicator-' + sid);
  const clock = document.getElementById('waiting-indicator-' + sid);
  const dot = document.getElementById('unread-' + sid);
  if (spinner) spinner.classList.toggle('hidden', state !== 'thinking');
  if (worker) worker.classList.toggle('hidden', state !== 'worker_only');
  if (alert) alert.classList.toggle('hidden', state !== 'attention');
  if (clock) clock.classList.toggle('hidden', state !== 'waiting');
  // The unread dot is an idle-state cue: any activity icon hides it.
  if (dot) dot.classList.toggle('hidden', state !== 'idle' || !effectiveUnread(sid));
}

// Repaint one row and every ancestor whose stand-in may have changed.
function refreshSessionIndicator(sid) {
  paintSessionIndicator(sid);
  if (!Sidebar.treeParentId) return;
  for (let parent = Sidebar.treeParentId(sid); parent; parent = Sidebar.treeParentId(parent)) {
    paintSessionIndicator(parent);
  }
}

function setSessionIndicator(sid, state) {
  ownIndicatorState[sid] = state;
  refreshSessionIndicator(sid);
}

// A grouped paint renders each row with its own facts; the parent rows take
// their stand-in state here, once the rows exist.
function refreshTreeIndicators() {
  Object.keys(ownIndicatorState).forEach(sid => {
    if (treeChildIdsOf(sid).length) paintSessionIndicator(sid);
  });
}

// "Read" means rendered: the session's server-side unread flag clears only
// through this POST, which the render paths fire after content painted
// (app.js's initial render, switchSession's winning generation). A bare data
// fetch never clears it. Fire-and-forget: a lost POST leaves the dot up until
// the next render reposts, so no retry or state mutation lives here.
function markSessionRead(sessionId) {
  fetch('/api/sessions/' + sessionId + '/read', {method: 'POST'}).catch((err) => {
    console.error('markSessionRead failed:', err);
  });
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
    setSwitchableBackends(data.switchable_backends);
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

function applySessionStatus(sid, status, requestSeq) {
  // A reply captured before a newer applied fact may not overwrite it: the
  // poll's snapshot can predate an unread_changed broadcast already rendered.
  if (unreadReplyIsCurrent(sid, requestSeq)) recordUnreadFact(sid, status.has_unread);
  globalThis.setSessionIndicator(sid, getSessionIndicatorState(status));
  globalThis.setSessionPendingTriggerIndicator(sid, status);
  globalThis.setSessionPendingPlanApprovalIndicator(sid, status);
  paintDelegateCardState(sid, status);
}

// The Delegated cards' live line: "running · <model>" while the delegation
// works; once it stops running, a task-tree child's work verdict (the same
// work_state its sidebar row paints from) reads "failed" or "queued", and
// anything else "idle". A card tracks either its child session (new-style) or
// the owning session (legacy), keyed by data attribute.
const DELEGATE_CARD_VERDICTS = {attention: 'failed', waiting: 'queued'};

function paintDelegateCardState(sid, status) {
  document.querySelectorAll(
      '.delegate-live-state[data-delegate-session="' + sid + '"],'
      + '.delegate-live-state[data-delegate-parent-session="' + sid + '"]').forEach(el => {
    const backend = el.dataset.delegateBackend || '';
    const verdict = status && status.has_running_tasks
      ? 'running' : (DELEGATE_CARD_VERDICTS[status && status.work_state] || 'idle');
    el.textContent = verdict + (backend ? ' · ' + backend : '');
  });
}

function refreshSessionStatusNow(opts) {
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
  const requestSeq = unreadSeqAtRequest();
  statusPollPromise = fetchScopedStatus('/api/sessions/status', sidebarSessionIds())
    .then(data => {
      if (!data) return false;
      let anyRunning = false;
      for (const [sid, st] of Object.entries(data)) {
        applySessionStatus(sid, st, requestSeq);
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

// Resumes the indicator when a page load or SPA switch lands while the master
// is mid-thought. THINKING_SINCE is the server-stamped start the session view
// and status polls refresh; keepSendEnabled leaves typing available while the
// run is still processing.
function resumeThinkingIfMidThought() {
  if (THINKING_SINCE) {
    thinkingStart = new Date(THINKING_SINCE).getTime();
    startThinking({keepSendEnabled: true});
  }
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
  // A worker node's or legacy thread view's stop button signals the active
  // Run through its own cancel route (session-view.js), not the chat cancel.
  if (typeof transcriptTarget === 'function' && transcriptTarget()) {
    return cancelTranscriptTarget();
  }
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


const API = {
  recordRenderedSessionStatus,
  renderTuiStatusDot,
  fetchTuiStatus,
  refreshTuiDots,
  startTuiStatusPolling,
  updateBackendHeaderControls,
  updateSidebarSessionName,
  getSessionIndicatorState,
  renderSessionIndicators,
  recordUnreadFact,
  unreadSeqAtRequest,
  unreadReplyIsCurrent,
  rowUnreadState,
  renderPendingTriggerIndicator,
  renderPendingPlanApprovalIndicator,
  updateSidebarHighlight,
  setSessionIndicator,
  refreshSessionIndicator,
  refreshTreeIndicators,
  effectiveIndicatorState,
  effectiveUnread,
  markSessionRead,
  setSessionPendingTriggerIndicator,
  setSessionPendingPlanApprovalIndicator,
  paintDelegateCardState,
  stopActiveSessionViewPolling,
  ensureActiveSessionViewPolling,
  pollActiveSessionView,
  refreshSessionStatusNow,
  pollSessionStatus,
  startThinking,
  resumeThinkingIfMidThought,
  stopThinking,
  updateThinkingTime,
  cancelMaster,
};
Sidebar.wire(API, {
  tuiSidebarSessionIds,
  sidebarSessionIds,
  updateSpinner,
});

})();
