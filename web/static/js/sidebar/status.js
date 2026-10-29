(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Status polls are scoped to the sessions the sidebar is actually rendering.
// The URL stays well under the 8 KB request-line budget at the ~23 sessions the
// sidebar shows, but a longer list is split so no single request can overflow.
const STATUS_QUERY_MAX_BYTES = 8192;

// Status of every rendered sidebar row, keyed by session id. The status
// poll skips archived rows through it. Their server probe is the constant-False
// shortcut (src/runtime/session_sidebar.py populate_sidebar_state), so per-cycle request
// volume tracks the active rows on screen, not the archived list length.
const renderedSessionStatuses = {};

// Rows painted with profile=worker: a leaf runs its own work, so its running
// state paints as the spinner rather than the delegated-work gear.
const renderedWorkerRows = new Set();
// Rows painted as task-tree nodes (profile not null): their own running state
// is a live Run of their own and paints as the spinner too. The gear is
// exclusively a parent row's stand-in for a running descendant or a paused
// worker.
const renderedTaskTreeRows = new Set();

// The paused-worker fact per row: a worker whose Run ended with its wake-up
// trigger still pending has no live Run and no running turn. Its own state
// then reads idle, yet it delivers nothing until the trigger fires. The
// wake-up trigger is a charliebot schedule-trigger registration. The gear
// stands in for it on every ancestor row for the whole pause. The list paint
// records the fact from the row's own profile and has_pending_trigger. The
// status poll's reply carries no profile, so the poll path takes the worker
// identity from renderedWorkerRows.
const pausedWorkerRows = new Set();

function recordPausedWorkerFact(sid, isWorkerRow, hasPendingTrigger) {
  if (isWorkerRow && hasPendingTrigger) pausedWorkerRows.add(sid);
  else pausedWorkerRows.delete(sid);
}

function isTaskTreeRow(session) {
  return !!session && session.profile != null;
}

function recordRenderedSessionStatus(session) {
  renderedSessionStatuses[session.id] = (session && session.status) || 'active';
  // The row's own activity as painted; the indicator aggregation reads it.
  ownIndicatorState[session.id] = getSessionIndicatorState(session);
  if (session && session.profile === 'worker') renderedWorkerRows.add(session.id);
  else renderedWorkerRows.delete(session.id);
  recordPausedWorkerFact(session.id,
      !!(session && session.profile === 'worker'), !!(session && session.has_pending_trigger));
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

// The compact button's facts come from the server, by backend type (BACKEND_COMPACT).
// A type that runs manual compaction enables the button. Any other type shows
// its hint or the generic title, including a type the configuration no longer lists.
function compactButtonTitle(compact) {
  if (compact && compact.manual) return '';
  return (compact && compact.hint) || 'Manual compaction is not supported on this backend';
}

function updateBackendHeaderControls(backendType) {
  const compactBtn = document.getElementById('compact-btn');
  if (compactBtn) {
    const compact = BACKEND_COMPACT[backendType];
    compactBtn.disabled = !(compact && compact.manual);
    compactBtn.title = compactButtonTitle(compact);
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
  // A task-tree row's fact-derived work verdict (idle | running | waiting)
  // when its sidebar state carries one. has_running_tasks already covered
  // 'running'; the remaining verdict maps onto its own icon.
  if (status.work_state === 'waiting') return 'waiting';
  return 'idle';
}

// ---------------------------------------------------------------------------
// Unread facts — one application seam with response-ordering protection
// ---------------------------------------------------------------------------
// sessionUnread holds the client's unread truth per session. Server facts land
// through three channels: unread_changed broadcasts, the scoped status poll,
// and the task tree's row fetches. A reply can carry a snapshot older than a
// fact this client already applied. Examples are a poll issued before the
// flip and a tree page read from a pre-flip projection. Every applied fact is
// stamped with a per-session sequence. A reply captured before the newest
// stamp is refused, so a stale reply can neither erase a newer unread nor
// resurrect a cleared one. Broadcasts are ordered by the socket and always
// apply.
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
// (a broadcast, a polled fact, or a fetched row). Otherwise it is the row's
// own server fact, such as a search hit that has not been through a level
// fetch yet.
function rowUnreadState(sessionId, serverValue) {
  return typeof sessionUnread[sessionId] === 'boolean' ? sessionUnread[sessionId] : !!serverValue;
}

// ---------------------------------------------------------------------------
// Activity indicators — one owner, one visual language
// ---------------------------------------------------------------------------
// The thinking spinner and the delegated-work gear are this module's SVGs and
// this module's element ids (spinner-<id>, worker-indicator-<id>). Every row
// that renders them is patchable by setSessionIndicator, whatever view built
// it.
const SPINNER_SVG_INNER =
  '<circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/>'
  + '<path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/>';
// The one cog-outline body: the worker indicator below and, through the
// namespace, groups.js's Settings buttons (Sidebar.GEAR_SVG_PATH). Each call
// site keeps its own center markup.
const GEAR_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/>`;
const GEAR_CENTER_PATH =
  '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/>';
const SPINNER_TITLE = 'Task is running';
const GEAR_TITLE = 'Delegated work running or waiting in subtasks';
// The waiting clock completes the visual language: one activity icon per row,
// chosen by the priority table in paintSessionIndicator. The activity family
// sits beside the unread marks, which ride independently. The order is own
// state first, then a parent's gear for a running descendant or a paused
// worker. The clock always describes the row itself, never its subtree.
const CLOCK_TITLE = 'Task waiting to run';
const CLOCK_SVG_PATH =
  '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" '
  + 'd="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z"/>';

function gearSvgContent() {
  return GEAR_SVG_PATH + GEAR_CENTER_PATH;
}

// The unread dot: the row's own unread reply — never a descendant's. Pinned
// to the unread-<id> element id the row re-render path and the unread_changed
// handler both address. It reads the unread facts only — any activity icon
// may show beside it, and the flag survives every paint either way.
const UNREAD_TITLE = 'Unread reply';
// The subtree unread mark: an unread reply anywhere in the row's subtree —
// same size as the dot, hollow instead of filled, and it does not pulse.
// Pinned to the subtree-unread-<id> element id paintSessionIndicator toggles.
// The tree shape is unknown at first render, so the mark paints hidden and
// refreshTreeIndicators lights it after the grouped paint.
const SUBTREE_UNREAD_TITLE = 'Unread reply in subtasks';

// Thinking spinner, worker gear, waiting clock, unread dot: one session row's
// header indicators. setSessionIndicator toggles each element by id, so the id
// prefixes are pinned.
function renderSessionIndicators(session) {
  const ownState = getSessionIndicatorState(session);
  const indicatorState = displayIndicatorState(session.id, ownState);
  return `<svg id="spinner-${session.id}" title="${escapeHtmlAttr(SPINNER_TITLE)}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24">${SPINNER_SVG_INNER}</svg>
    <svg id="worker-indicator-${session.id}" title="${escapeHtmlAttr(GEAR_TITLE)}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24">${gearSvgContent()}</svg>
    <svg id="waiting-indicator-${session.id}" title="${escapeHtmlAttr(CLOCK_TITLE)}" class="w-3.5 h-3.5 text-slate-500 flex-shrink-0 ${indicatorState === 'waiting' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24">${CLOCK_SVG_PATH}</svg>
    <span id="unread-${session.id}" data-has-unread="${session.has_unread ? 1 : 0}" title="${escapeHtmlAttr(UNREAD_TITLE)}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${session.has_unread ? '' : 'hidden'}"></span>
    <span id="subtree-unread-${session.id}" title="${escapeHtmlAttr(SUBTREE_UNREAD_TITLE)}" class="w-2 h-2 rounded-full border border-yellow-400 flex-shrink-0 hidden"></span>`;
}

function pendingTriggerTitle(count, nextTriggerAt) {
  const normalized = Number(count) || 0;
  let title;
  if (normalized === 1) title = '1 pending delayed trigger';
  else if (normalized > 1) title = `${normalized} pending delayed triggers`;
  else title = 'Pending delayed trigger';
  if (nextTriggerAt) title += ' · next ' + clockTimeHM(new Date(nextTriggerAt));
  return title;
}

function renderPendingTriggerIndicator(session) {
  const count = Number(session.pending_trigger_count) || 0;
  const click = `event.preventDefault(); event.stopPropagation(); sidebarControl('openPendingTriggers', '${session.id}')`;
  return `<svg id="pending-trigger-${session.id}" data-count="${count}" data-next-trigger-at="${escapeHtmlAttr(session.next_trigger_at || '')}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 cursor-pointer ${session.has_pending_trigger ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="${escapeHtmlAttr(pendingTriggerTitle(count, session.next_trigger_at))}" onclick="${click}">${Chat.PENDING_TRIGGER_BELL_SVG_PATH}</svg>`;
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

// Indicator priority over the nested sidebar. Two independent families each pick at most one
// element per row, in a fixed order that reads no expansion state. The activity family starts
// with the row's own state. The row's own thinking or running paints the spinner through the
// display mapping. A legacy row's own delegated work keeps the gear, and own waiting (queued,
// not launched) takes the muted clock. A parent row whose own state is idle then stands in for
// its subtree with one icon only. A running descendant at any depth shows the gear, and so
// does a paused worker (a worker row whose latest fact is idle with a pending wake-up
// trigger). The clock never stands in for a subtree. The unread family reads the unread facts
// alone, gated by no activity state. The row's own unread paints the filled dot, otherwise an
// unread reply anywhere below paints the subtree's hollow mark. The facts are the latest
// applied state per row, from the list paint, the status poll, and the task_tree_changed /
// running_changed broadcasts. They also include the paused-worker fact that the list paint and
// the status poll record, and the shared unread map. The tree shape comes from the last grouped
// paint, which groups.js records here.
const ownIndicatorState = {};
let lastTreeChildrenOf = new Map();
let lastTreeParentOf = new Map();

function recordTreeShape(childrenOf, parentOf) {
  lastTreeChildrenOf = childrenOf;
  lastTreeParentOf = parentOf;
}

// The children of the last grouped paint, for indicator aggregation.
function treeChildIds(sessionId) {
  return (lastTreeChildrenOf.get(sessionId) || []).map(s => s.id);
}

// The parent of a nested row in the last grouped paint (null for a root).
function treeParentId(sessionId) {
  return lastTreeParentOf.get(sessionId) || null;
}

function subtreeHasState(sid, state) {
  return treeChildIds(sid).some(
      child => (ownIndicatorState[child] || 'idle') === state || subtreeHasState(child, state));
}

function subtreeUnread(sid) {
  return treeChildIds(sid).some(child => !!sessionUnread[child] || subtreeUnread(child));
}

// A paused worker anywhere below the row: the same walk as subtreeHasState,
// over the trigger fact the list paint and the status poll record.
function subtreeHasPausedWorker(sid) {
  return treeChildIds(sid).some(child => pausedWorkerRows.has(child) || subtreeHasPausedWorker(child));
}

// The subtree stand-in sits in the same priority position as the row's own
// state. A running descendant at any depth shows the gear, and so does a
// paused worker at any depth. A running descendant is one whose own state is
// worker_only or thinking: a manager's turn or a worker's live Run, whose
// header timer (thinking_since) paints its own row's spinner. The clock never
// stands in for a subtree: a queued Run marks its own row only.
function subtreeStandInState(sid) {
  if (subtreeHasState(sid, 'worker_only') || subtreeHasState(sid, 'thinking')
      || subtreeHasPausedWorker(sid)) return 'worker_only';
  return 'idle';
}

function effectiveIndicatorState(sid) {
  const own = ownIndicatorState[sid] || 'idle';
  if (own !== 'idle') return own;
  return subtreeStandInState(sid);
}

// The split the two unread marks read: the dot is the row's own fact; the
// subtree mark is the subtree's. One row's own unread outranks its subtree's.
function ownUnread(sid) {
  return !!sessionUnread[sid];
}

function paintSessionIndicator(sid) {
  // The display mapping (a row's own running Run reads as the spinner) applies
  // only to the row's own state. The subtree stand-in keeps its own icon: the
  // gear for a running descendant or a paused worker.
  const own = ownIndicatorState[sid] || 'idle';
  const state = own !== 'idle' ? displayIndicatorState(sid, own) : subtreeStandInState(sid);
  const spinner = document.getElementById('spinner-' + sid);
  const worker = document.getElementById('worker-indicator-' + sid);
  const clock = document.getElementById('waiting-indicator-' + sid);
  const dot = document.getElementById('unread-' + sid);
  const subtreeMark = document.getElementById('subtree-unread-' + sid);
  if (spinner) spinner.classList.toggle('hidden', state !== 'thinking');
  if (worker) worker.classList.toggle('hidden', state !== 'worker_only');
  if (clock) clock.classList.toggle('hidden', state !== 'waiting');
  // The unread family reads the unread facts alone; no activity state gates
  // it. The dot reads the row's own fact; the subtree mark yields to it and
  // reads the subtree's.
  if (dot) dot.classList.toggle('hidden', !ownUnread(sid));
  if (subtreeMark) subtreeMark.classList.toggle('hidden',
      ownUnread(sid) || !subtreeUnread(sid));
}

// Repaint one row and every ancestor whose gear or subtree mark may have
// changed. The read paths (app.js's initial render, switchSession's winning
// generation) call this right after recordUnreadFact(id, false), so the opened
// row's dot and every ancestor's subtree mark clear in one paint.
function refreshSessionIndicator(sid) {
  paintSessionIndicator(sid);
  for (let parent = treeParentId(sid); parent; parent = treeParentId(parent)) {
    paintSessionIndicator(parent);
  }
}

function setSessionIndicator(sid, state) {
  ownIndicatorState[sid] = state;
  refreshSessionIndicator(sid);
}

// A grouped paint renders each row with its own facts; the parent rows take
// their stand-in gear and their subtree marks here, once the rows exist.
function refreshTreeIndicators() {
  Object.keys(ownIndicatorState).forEach(sid => {
    if (treeChildIds(sid).length) paintSessionIndicator(sid);
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
  icon.title = pendingTriggerTitle(count, status && status.next_trigger_at);
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

// Poll-based sidebar status (corrects WS drift)
let statusPollInflight = false;
let statusPollPromise = Promise.resolve(false);
let statusPollQueued = false;

function applySessionStatus(sid, status, requestSeq) {
  // A reply captured before a newer applied fact may not overwrite it: the
  // poll's snapshot can predate an unread_changed broadcast already rendered.
  if (unreadReplyIsCurrent(sid, requestSeq)) recordUnreadFact(sid, status.has_unread);
  // The paused-worker fact lands before the repaint below reads it; the poll
  // reply carries no profile, so the worker identity comes from the list
  // paint's renderedWorkerRows.
  recordPausedWorkerFact(sid, renderedWorkerRows.has(sid), !!(status && status.has_pending_trigger));
  globalThis.setSessionIndicator(sid, getSessionIndicatorState(status));
  globalThis.setSessionPendingTriggerIndicator(sid, status);
  globalThis.setSessionPendingTriggerTrayStatus(sid, status);
  globalThis.setSessionPendingPlanApprovalIndicator(sid, status);
  paintDelegateCardState(sid, status);
}

// The running_changed seam. A worker row's Run ending usually means a pause.
// The worker registers its wake-up trigger just before its turn ends, so the
// event reaches the browser before any poll could carry the new trigger fact.
// Repainting the row idle on the event would blink the row's spinner and the
// ancestors' gear off until the next poll. That repaint is left to the status
// poll the handler starts right after, which applies the new state and the
// trigger fact in one write. A really finished worker pays one status round
// trip of stale spinner and gear for this. Every other event — a busy start,
// any non-worker row — repaints at once.
function applyRunningChanged(ev) {
  const runEnded = !ev.thinking_since && !ev.has_running_tasks;
  if (runEnded && renderedWorkerRows.has(ev.session_id)) return;
  setSessionIndicator(ev.session_id, getSessionIndicatorState({
    thinking_since: ev.thinking_since,
    has_running_tasks: ev.has_running_tasks,
    work_state: ev.work_state,
  }));
}

// The Delegated cards' live line reads "running · <model>" while the
// delegation works. Once it stops running, a task-tree child's work verdict
// (the same work_state its sidebar row paints from) reads "queued" while its
// Run waits. Anything else reads "idle". A card tracks either its child
// session (new-style) or the owning session (legacy), keyed by data attribute.
const DELEGATE_CARD_VERDICTS = {waiting: 'queued'};

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


const API = {
  recordRenderedSessionStatus,
  recordTreeShape,
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
  ownUnread,
  subtreeUnread,
  markSessionRead,
  setSessionPendingTriggerIndicator,
  setSessionPendingPlanApprovalIndicator,
  paintDelegateCardState,
  refreshSessionStatusNow,
  pollSessionStatus,
  applyRunningChanged,
};
Sidebar.wire(API, {
  sidebarSessionIds,
  updateSpinner,
  GEAR_SVG_PATH,
  treeChildIds,
  treeParentId,
});

})();
