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
// poll skips archived rows through it, so per-cycle request volume tracks the
// active rows on screen, not the archived list length. If a caller names an
// archived row, the server still answers its stored Waiting on you flag.
const renderedSessionStatuses = {};

// Rows painted with profile=worker: a leaf runs its own work, so its running
// state paints as the spinner rather than the delegated-work loader.
const renderedWorkerRows = new Set();
// Rows painted as task-tree nodes (profile not null): their own running state
// is a live Run of their own and paints as the spinner too. The loader is
// exclusively a parent row's stand-in for a running descendant or a paused
// worker.
const renderedTaskTreeRows = new Set();

// The paused-worker fact per row: a worker whose Run ended with its wake-up
// trigger still pending has no live Run and no running turn. Its own state
// then reads idle, yet it delivers nothing until the trigger fires. The
// wake-up trigger is a charliebot session trigger registration. The loader
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
  // The row's own activity as painted. The indicator aggregation reads it.
  ownIndicatorState[session.id] = getSessionIndicatorState(session);
  stampFactSeq('indicator', session.id);
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
  // poll. A child whose sidebar row is filtered out still rides the request.
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

// The status URLs that name these sessions by ids, each under the URL budget. A `query` (such as `filter=all`) and the
// `since` token ride in every URL ahead of the ids.
//
// The ids are sorted: the URL names a set, so a repaint that reorders the rows must not change it. A changed URL is a
// URL the browser has no cached answer for, and the poll then costs a whole body where the ETag would have answered
// 304.
function statusRequestUrls(path, ids, query = '', since = '') {
  const canonical = Array.from(new Set(ids)).sort();
  const head = [query, since ? 'since=' + encodeURIComponent(since) : ''].filter(Boolean);
  if (canonical.length === 0) return head.length ? [path + '?' + head.join('&')] : [];
  const urlFor = (batch) => path + '?' + head.concat(['ids=' + batch.map(encodeURIComponent).join(',')]).join('&');
  const urls = [];
  let batch = [];
  const flush = () => {
    if (batch.length) urls.push(urlFor(batch));
    batch = [];
  };
  canonical.forEach(sid => {
    if (batch.length && urlFor(batch.concat([sid])).length > STATUS_QUERY_MAX_BYTES) flush();
    batch.push(sid);
  });
  flush();
  return urls;
}

// The status poll's URLs. A list the server can scope by filter names the filter, and the sessions the list does not
// paint (the open chat, a delegate card's child) travel as ids. The archive view and a search answer their own row
// sets, so they name every session by id.
//
// A page that carried only its first screen names those rows, while they fit one URL, and asks for `answer=ids`. The
// server then answers those rows alone, and the filter still names the set whose membership the poll follows. Its
// first poll otherwise downloads every row of the view, though the page paints only its first screen. Once a full
// listing or a view change moves the poll's row set, the filter form answers it again, so a row entering the view
// still rides a poll.
function sidebarStatusUrls(path, ids, since) {
  if (!sidebarRowsRefreshable()) return statusRequestUrls(path, ids);
  const query = 'filter=' + encodeURIComponent(currentFilter);
  if (pageLeftRowsOut() && firstScreenPollIds === null && ids.length) {
    firstScreenPollIds = [...ids].sort().join(',');
  }
  if (firstScreenPollIds !== null && [...ids].sort().join(',') === firstScreenPollIds) {
    const named = statusRequestUrls(path, ids, query, since);
    if (named.length === 1 && named[0].length + ANSWER_QUERY.length <= STATUS_QUERY_MAX_BYTES) {
      return [named[0] + ANSWER_QUERY];
    }
  }
  const painted = new Set(Array.from(document.querySelectorAll('a[id^="session-"]'),
      el => el.id.slice('session-'.length)));
  const unpainted = ids.filter(sid => !painted.has(sid));
  return statusRequestUrls(path, unpainted, query, since);
}

// The row set the poll named on the page's first poll, while the page carried only its first screen.
let firstScreenPollIds = null;

// The query that asks the server for the rows the request names rather than every row of the filter's set.
const ANSWER_QUERY = '&answer=ids';

// Whether the page carried only its first screen: the sidebar's `$screen` marks a page that left a group's rows out.
function pageLeftRowsOut() {
  return typeof INITIAL_SESSIONS !== 'undefined'
    && Boolean(INITIAL_SESSIONS && INITIAL_SESSIONS.$screen && INITIAL_SESSIONS.$screen.partial);
}

// The status answers: the token to send back, the rows that moved, and the ids that left the filter's set.
// An open's catch-up owns the link until its socket replay lands. The poll's row set is a download beside that replay:
// on the poor-network bench's B open the first poll after the list copy paints named every row of the filter at 3.3 s
// (3.4 KB before the open's newest message). A held poll sends nothing and answers no rows, so the row state stays as
// it is; the first tick after the hold ends polls again.
async function fetchStatusUrls(urls) {
  if (statusPollHeld()) return [];
  const responses = await Promise.all(urls.map(url => fetch(url)));
  const answers = [];
  for (const r of responses) {
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    answers.push(await r.json());
  }
  return answers;
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

// The title a row shows. A Threads row under a named group header drops the header's label from its title: the
// header already names the channel, so the title keeps the thread's date and time. Every other row keeps its full
// name: rows outside Threads, rows under the keyless "(No group)" header, and a title that does not start with its
// header's label. The rule lives in status.js because it loads before the row renderers (groups.js). That module
// owns the one name write both the rename commit and the session_renamed event funnel into. The renderers read it
// as a global.
function sessionRowTitle(s, filter, groupHeader) {
  const label = groupHeader + ' ';
  if (filter !== 'threads' || !groupHeader || !s.name.startsWith(label)) return s.name;
  return s.name.slice(label.length);
}

function updateSidebarSessionName(sessionId, name) {
  const link = document.getElementById('session-' + sessionId);
  if (!link) return;
  const nameEl = link.querySelector('.session-name');
  if (!nameEl) return;
  // The row paints the title the next list repaint would give it. Under a named
  // Threads group header the title drops that label (sessionRowTitle, whose
  // header the row's own group container carries). Everywhere else the title is
  // the name. The hover text is the full name, as renderSessionRowShell paints it.
  const group = link.closest('.session-group');
  nameEl.textContent = sessionRowTitle({name}, currentFilter, group ? group.dataset.sgroupKey : '');
  nameEl.title = name;
}

function getSessionIndicatorState(status) {
  if (status.thinking_since) return 'thinking';
  if (status.has_running_tasks) return 'worker_only';
  // A task-tree row's fact-derived work verdict (idle | running | waiting)
  // when its sidebar state carries one. has_running_tasks already covered
  // 'running'. The remaining verdict maps onto its own icon.
  if (status.work_state === 'waiting') return 'waiting';
  return 'idle';
}

// ---------------------------------------------------------------------------
// Applied facts and reply currency — one gate over the fact domains
// ---------------------------------------------------------------------------
// sessionUnread and ownIndicatorState hold the client's applied facts per
// session. The facts land through four channels: the unread_changed and
// running_changed broadcasts, the fetched row lists, and the scoped status
// poll. The fetched row lists are the page's own rows, a filter's list, the
// row refresh, the full listing and the search answer. A reply can carry a
// snapshot older than a fact this client already applied — a poll issued
// before the flip, a row fetch answered before the read POST landed. Every
// applied fact is stamped with a per-session sequence in its domain, the
// unread mark, the waiting mark or the activity indicator. A reply captured before the domain's
// newest stamp is refused in that domain. A late reply then can neither erase
// a newer unread fact nor revert a newer indicator state, a running_changed
// frame included. Broadcasts are ordered by their channel and always apply.
// Applying one stamps it, so any reply captured earlier is refused.
//
// The unread mark has this one owner: every row paints its dot and subtree
// mark from sessionUnread, and only the functions below write it. A list paint
// that reuses rows the page already holds reads the facts and applies none:
// the Workspace badge repaint, the row refresh's merge, the inline delete. A
// held row's has_unread then cannot bring back a dot a newer fact cleared.
// The waiting mark has the same owner rule over sessionWaiting, in its own
// domain: the listing rows and the status poll write it, and a row paints its
// mark from the fact.
const sessionUnread = {};
const sessionWaiting = {};
let factSeq = 0;
const factSeqByDomain = {unread: {}, indicator: {}, waiting: {}};

function stampFactSeq(domain, sessionId) {
  factSeqByDomain[domain][sessionId] = ++factSeq;
}

function replyIsCurrent(domain, sessionId, requestSeq) {
  return requestSeq >= (factSeqByDomain[domain][sessionId] || 0);
}

// The open view renders the displayed session's replies live, so its unread
// fact does not stand: the page reads it once the tab is visible. A switch in
// flight is leaving the session, so a reply that lands then stays unread on
// the row the switch leaves behind.
function recordUnreadFact(sessionId, hasUnread) {
  sessionUnread[sessionId] = !!hasUnread;
  stampFactSeq('unread', sessionId);
  if (hasUnread && sessionId === SESSION_ID) deferWhileHidden('displayed-session-read', readDisplayedSession);
}

function readDisplayedSession() {
  if (SESSION_ID && !switching && ownUnread(SESSION_ID)) markSessionRead(SESSION_ID, null);
}

// The waiting fact: the session stores at least one Waiting on you item that is not marked done.
function recordWaitingFact(sessionId, hasWaiting) {
  sessionWaiting[sessionId] = !!hasWaiting;
  stampFactSeq('waiting', sessionId);
}

// The Status view's done boxes of the open session: the page where an item is checked shows that session's mark
// from those boxes at once, before the click's reply lands. An unchecked box, or a click that fails, puts the mark
// back. The write is stamped like any other waiting fact, so a listing row or poll reply captured before it is
// refused, and the next poll answers every other page.
function applyStatusCheckState(sessionId, hasUnchecked) {
  recordWaitingFact(sessionId, hasUnchecked);
  refreshSessionIndicator(sessionId);
}
Sidebar.registerWaitingCheckWriter(applyStatusCheckState);

// The facts of one fetched row list, captured at requestSeq (factSeqAtRequest
// before the fetch). Call it before the list paints. Each row carries the
// server's has_waiting_items flag. An archived row stores no waiting fact,
// whatever its flag holds.
function applyListingFacts(rows, requestSeq) {
  rows.forEach(row => {
    if (replyIsCurrent('unread', row.id, requestSeq)) recordUnreadFact(row.id, row.has_unread);
    if (replyIsCurrent('waiting', row.id, requestSeq)) {
      recordWaitingFact(row.id, row.status !== 'archived' && row.has_waiting_items);
    }
  });
}

// A removed row's facts leave with it.
function forgetSessionFacts(sessionId) {
  delete sessionUnread[sessionId];
  delete sessionWaiting[sessionId];
}

function factSeqAtRequest() {
  return factSeq;
}

// ---------------------------------------------------------------------------
// Activity indicators — one owner, one visual language
// ---------------------------------------------------------------------------
// The thinking spinner, the delegated-work loader and the waiting icon are this
// module's icons and this module's element ids (spinner-<id>,
// worker-indicator-<id>, waiting-indicator-<id>). Every row that renders them
// is patchable by setSessionIndicator, whatever view built it.
const SPINNER_TITLE = 'Task is running';
const DELEGATED_TITLE = 'Delegated work running or waiting in subtasks';
// The spin phase every spinner shares: the wall clock's milliseconds within
// one turn of the animation. The paint bakes the current phase into the
// element as a negative animation-delay, so a spinner a repaint recreates
// starts where the spinner it replaces was. A running row's rotation keeps its
// phase through every list update instead of jumping back to 0°.
const SPINNER_PERIOD_MS = 1000;

function spinnerPhaseDelay() {
  const phaseMs = Date.now() % SPINNER_PERIOD_MS;
  return ` style="animation-delay:-${(phaseMs / SPINNER_PERIOD_MS).toFixed(3)}s"`;
}
// The waiting icon completes the visual language: one activity icon per row,
// chosen by the priority table in paintSessionIndicator. The activity family
// sits beside the unread marks, which ride independently. The order is own
// state first, then a parent's loader for a running descendant or a paused
// worker. The waiting icon always describes the row itself, never its subtree.
const WAITING_TITLE = 'Task waiting to run';

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

// Thinking spinner, worker loader, waiting icon, unread dot: one session row's
// header indicators. setSessionIndicator toggles each element by id, so the id
// prefixes are pinned.
function renderSessionIndicators(session) {
  const ownState = getSessionIndicatorState(session);
  const indicatorState = displayIndicatorState(session.id, ownState);
  return `${cbIcon('loading', 16, `animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}`,
    `id="spinner-${session.id}" title="${escapeHtmlAttr(SPINNER_TITLE)}"${spinnerPhaseDelay()}`)}
    ${cbIcon('subtasks-running', 14, `text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}`,
    `id="worker-indicator-${session.id}" title="${escapeHtmlAttr(DELEGATED_TITLE)}"`)}
    ${cbIcon('waiting', 14, `text-slate-500 flex-shrink-0 ${indicatorState === 'waiting' ? '' : 'hidden'}`,
    `id="waiting-indicator-${session.id}" title="${escapeHtmlAttr(WAITING_TITLE)}"`)}
    ${cbIcon('dot', 8, `text-yellow-400 animate-pulse-dot flex-shrink-0 ${ownUnread(session.id) ? '' : 'hidden'}`,
    `id="unread-${session.id}" title="${escapeHtmlAttr(UNREAD_TITLE)}"`)}
    ${cbIcon('ring', 8, 'text-yellow-400 flex-shrink-0 hidden',
    `id="subtree-unread-${session.id}" title="${escapeHtmlAttr(SUBTREE_UNREAD_TITLE)}"`)}`;
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
  return cbIcon('notification', 14, `text-amber-400 flex-shrink-0 cursor-pointer ${session.has_pending_trigger ? '' : 'hidden'}`,
    `id="pending-trigger-${session.id}" data-count="${count}" data-next-trigger-at="${escapeHtmlAttr(session.next_trigger_at || '')}" title="${escapeHtmlAttr(pendingTriggerTitle(count, session.next_trigger_at))}" onclick="${click}"`);
}

function renderPendingPlanApprovalIndicator(session) {
  const hasPending = !!(session && session.has_pending_plan_approval);
  return cbIcon('plan', 14, `text-blue-400 flex-shrink-0 ${hasPending ? '' : 'hidden'}`,
    `id="pending-plan-approval-${session.id}" title="Plan awaiting approval"`);
}

// The waiting marks: a solid square for the row's own Waiting on you fact and
// a hollow square for the subtree mark. Each mark's look is its class string
// below and its exclamation-mark icon, so a later change of look edits these
// strings only.
// Both marks start hidden. paintSessionIndicator shows them from the facts.
const WAITING_MARK_TITLE = 'Waiting on you';
const WAITING_SUBTREE_MARK_TITLE = 'Waiting on you in a session below';
const WAITING_MARK_CLASS = 'inline-flex items-center justify-center flex-shrink-0 w-[18px] h-[18px] rounded-[5px] '
  + 'border border-orange-700 bg-orange-700 text-white text-xs font-extrabold leading-none select-none';
const WAITING_SUBTREE_MARK_CLASS = 'inline-flex items-center justify-center flex-shrink-0 w-[18px] h-[18px] '
  + 'rounded-[5px] border-2 border-orange-400 bg-transparent text-orange-500 text-xs font-extrabold leading-none '
  + 'select-none';

function renderWaitingMarks(session) {
  const hidden = sessionWaiting[session.id] ? '' : ' hidden';
  const own = `<span id="waiting-marker-${session.id}" title="${WAITING_MARK_TITLE}"`
    + ` class="${WAITING_MARK_CLASS}${hidden}">${cbIcon('waiting-on-you', 12)}</span>`;
  const subtree = `<span id="waiting-subtree-marker-${session.id}" title="${WAITING_SUBTREE_MARK_TITLE}"`
    + ` class="${WAITING_SUBTREE_MARK_CLASS} hidden">${cbIcon('waiting-on-you', 12)}</span>`;
  return own + subtree;
}

// Only the session rows carry the row tints. The list's own id
// (`session-list`) also starts with the row-id prefix, so a bare prefix
// selector would stamp the row classes on the nav. The whole panel would then
// paint the row's hover fill, and the current row's second line would read
// 4.34:1 there instead of 5.23:1.
function updateSidebarHighlight(newSessionId) {
  document.querySelectorAll('#session-list .session-row').forEach(el => {
    el.classList.remove('bg-blue-600/20', 'text-blue-300');
    el.classList.add('hover:bg-slate-700/50', 'text-slate-300');
  });
  const active = document.getElementById('session-' + newSessionId);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('hover:bg-slate-700/50', 'text-slate-300');
  }
}

// Indicator priority over the nested sidebar. Two independent families each pick at most one
// element per row, in a fixed order that reads no expansion state. The activity family starts
// with the row's own state. The row's own thinking or running paints the spinner through the
// display mapping. A legacy row's own delegated work keeps the loader, and own waiting (queued,
// not launched) takes the muted waiting icon. A parent row whose own state is idle then stands in for
// its subtree with one icon only. A running descendant at any depth shows the loader, and so
// does a paused worker (a worker row whose latest fact is idle with a pending wake-up
// trigger). The waiting icon never stands in for a subtree. The unread family reads the unread facts
// alone, gated by no activity state. The row's own unread paints the filled dot, otherwise an
// unread reply anywhere below paints the subtree's hollow mark. The facts are the latest
// applied state per row, from the list paint, the status poll, and the task_tree_changed /
// running_changed broadcasts. They also include the paused-worker fact that the list paint and
// the status poll record, and the shared unread map. The tree shape comes from the last grouped
// paint, which groups.js records here.
const ownIndicatorState = {};
let lastTreeChildrenOf = new Map();
let lastTreeParentOf = new Map();
let lastTreeIsExpanded = () => false;

// isExpanded reads the grouped paint's expansion state. The subtree waiting mark is the only icon that reads it.
function recordTreeShape(childrenOf, parentOf, isExpanded) {
  lastTreeChildrenOf = childrenOf;
  lastTreeParentOf = parentOf;
  lastTreeIsExpanded = isExpanded;
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

// The waiting facts below the row, at any depth. An archived row's fact is
// false, so it adds nothing and passes nothing up.
function subtreeWaiting(sid) {
  return treeChildIds(sid).some(child => !!sessionWaiting[child] || subtreeWaiting(child));
}

// The subtree waiting mark shows on a folded row with a listed row below that
// holds the fact. Expanding the row hides it, and folding it again shows it.
function subtreeWaitingMarkShown(sid) {
  return subtreeWaiting(sid) && !lastTreeIsExpanded(sid);
}

// A paused worker anywhere below the row: the same walk as subtreeHasState,
// over the trigger fact the list paint and the status poll record.
function subtreeHasPausedWorker(sid) {
  return treeChildIds(sid).some(child => pausedWorkerRows.has(child) || subtreeHasPausedWorker(child));
}

// The subtree stand-in sits in the same priority position as the row's own
// state. A running descendant at any depth shows the loader, and so does a
// paused worker at any depth. A running descendant is one whose own state is
// worker_only or thinking: a manager's turn or a worker's live Run, whose
// header timer (thinking_since) paints its own row's spinner. The waiting icon never
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

// The split the two unread marks read: the dot is the row's own fact. The
// subtree mark is the subtree's. One row's own unread outranks its subtree's.
function ownUnread(sid) {
  return !!sessionUnread[sid];
}

function paintSessionIndicator(sid) {
  // The display mapping (a row's own running Run reads as the spinner) applies
  // only to the row's own state. The subtree stand-in keeps its own icon: the
  // loader for a running descendant or a paused worker.
  const own = ownIndicatorState[sid] || 'idle';
  const state = own !== 'idle' ? displayIndicatorState(sid, own) : subtreeStandInState(sid);
  const spinner = document.getElementById('spinner-' + sid);
  const worker = document.getElementById('worker-indicator-' + sid);
  const waiting = document.getElementById('waiting-indicator-' + sid);
  const dot = document.getElementById('unread-' + sid);
  const subtreeMark = document.getElementById('subtree-unread-' + sid);
  if (spinner) spinner.classList.toggle('hidden', state !== 'thinking');
  if (worker) worker.classList.toggle('hidden', state !== 'worker_only');
  if (waiting) waiting.classList.toggle('hidden', state !== 'waiting');
  // The unread family reads the unread facts alone. No activity state gates
  // it. The dot reads the row's own fact. The subtree mark yields to it and
  // reads the subtree's.
  if (dot) dot.classList.toggle('hidden', !ownUnread(sid));
  if (subtreeMark) subtreeMark.classList.toggle('hidden',
      ownUnread(sid) || !subtreeUnread(sid));
  // The waiting marks read the waiting facts alone, as the unread dot does.
  const waitingMark = document.getElementById('waiting-marker-' + sid);
  const waitingSubtreeMark = document.getElementById('waiting-subtree-marker-' + sid);
  if (waitingMark) waitingMark.classList.toggle('hidden', !sessionWaiting[sid]);
  if (waitingSubtreeMark) waitingSubtreeMark.classList.toggle('hidden', !subtreeWaitingMarkShown(sid));
}

// Repaint one row and every ancestor whose loader or subtree mark may have
// changed. This runs right after markSessionRead clears the fact, so the
// opened row's dot and every ancestor's subtree mark clear in one paint.
function refreshSessionIndicator(sid) {
  paintSessionIndicator(sid);
  for (let parent = treeParentId(sid); parent; parent = treeParentId(parent)) {
    paintSessionIndicator(parent);
  }
}

function setSessionIndicator(sid, state) {
  ownIndicatorState[sid] = state;
  stampFactSeq('indicator', sid);
  refreshSessionIndicator(sid);
}

// A grouped paint renders each row with its own facts. The parent rows take
// their stand-in loader and their subtree marks here, once the rows exist.
function refreshTreeIndicators() {
  Object.keys(ownIndicatorState).forEach(sid => {
    if (treeChildIds(sid).length) paintSessionIndicator(sid);
  });
}

// "Read" means rendered: the read paths call this after the session's content
// painted (app.js's initial render, switchSession's winning generation, a
// reply the displayed view rendered). The local fact clears first, stamped so
// an older reply is refused, and the row repaints. The server-side flag clears
// only through the POST. A bare data fetch never clears it. Fire-and-forget: a
// lost POST leaves the server flag set, and the next reply that carries it
// paints the dot again, or reads it again on the displayed session.
//
// pageDot is the dot the row showed when the person opened the session, and
// the server logs it beside its own flag (unread_dot_diverged on a mismatch).
// It is null for a read whose row shows no dot by design: the page load (the
// server renders the open row read) and a reply in the displayed session.
function markSessionRead(sessionId, pageDot) {
  recordUnreadFact(sessionId, false);
  refreshSessionIndicator(sessionId);
  const query = pageDot === null ? '' : '?page_dot=' + pageDot;
  fetch('/api/sessions/' + sessionId + '/read' + query, {method: 'POST'}).catch((err) => {
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
  // Each domain refuses a reply captured before its newest applied fact: the
  // poll's snapshot can predate an unread_changed broadcast or a
  // running_changed frame the page already rendered.
  if (replyIsCurrent('unread', sid, requestSeq)) recordUnreadFact(sid, status.has_unread);
  // The waiting fact repaints the row and its ancestors: their subtree marks read it.
  if (replyIsCurrent('waiting', sid, requestSeq)) {
    recordWaitingFact(sid, renderedSessionStatuses[sid] !== 'archived' && status.has_waiting_items);
    refreshSessionIndicator(sid);
  }
  // The paused-worker fact lands before the repaint below reads it. The poll
  // reply carries no profile, so the worker identity comes from the list
  // paint's renderedWorkerRows.
  recordPausedWorkerFact(sid, renderedWorkerRows.has(sid), !!(status && status.has_pending_trigger));
  if (replyIsCurrent('indicator', sid, requestSeq)) {
    globalThis.setSessionIndicator(sid, getSessionIndicatorState(status));
  }
  globalThis.setSessionPendingTriggerIndicator(sid, status);
  globalThis.setSessionPendingTriggerTrayStatus(sid, status);
  globalThis.setSessionPendingPlanApprovalIndicator(sid, status);
  paintDelegateCardState(sid, status);
}

// The running_changed seam. A worker row's Run ending usually means a pause.
// The worker registers its wake-up trigger just before its turn ends, so the
// event reaches the browser before any poll could carry the new trigger fact.
// Repainting the row idle on the event would blink the row's spinner and the
// ancestors' loader off until the next poll. That repaint is left to the status
// poll the handler starts right after, which applies the new state and the
// trigger fact in one write. A really finished worker pays one status round
// trip of stale spinner and loader for this. Every other event — a busy start,
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

// The row states this page holds, keyed by session id, and the token naming the state they came from. A poll sends the
// token back, so the server answers only the rows that moved since. This map is the one owner of row state: a reply
// lands here, and the page paints from it.
const appliedSessionStatus = {};
let statusPollToken = '';

// Forget the applied row states, so the next poll asks for the whole set. A view change or a reconnect moves the row
// set the page polls, and rows it has not held must arrive in full.
function resetSessionStatusPoll() {
  for (const sid of Object.keys(appliedSessionStatus)) delete appliedSessionStatus[sid];
  statusPollToken = '';
}

function anyRunningSession() {
  return Object.values(appliedSessionStatus).some(st => st && st.has_running_tasks);
}

function pollSessionStatus() {
  if (statusPollInflight) return statusPollPromise;
  statusPollInflight = true;
  const requestSeq = factSeqAtRequest();
  const ids = sidebarSessionIds();
  // A poll scoped by filter answers a delta. A row the page has never held would not be in it, so a new id asks for
  // the whole set once. The views that name their rows by id answer them in full anyway.
  const scoped = sidebarRowsRefreshable();
  const since = scoped && ids.every(sid => sid in appliedSessionStatus) ? statusPollToken : '';
  statusPollPromise = fetchStatusUrls(sidebarStatusUrls('/api/sessions/status', ids, since))
    .then(answers => {
      for (const answer of answers) {
        for (const [sid, st] of Object.entries(answer.rows || {})) {
          appliedSessionStatus[sid] = st;
          applySessionStatus(sid, st, requestSeq);
        }
        for (const sid of answer.gone || []) delete appliedSessionStatus[sid];
        if (scoped && answer.token) statusPollToken = answer.token;
      }
      return anyRunningSession();
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
  sessionRowTitle,
  updateBackendHeaderControls,
  updateSidebarSessionName,
  getSessionIndicatorState,
  renderSessionIndicators,
  renderWaitingMarks,
  recordUnreadFact,
  applyListingFacts,
  forgetSessionFacts,
  factSeqAtRequest,
  replyIsCurrent,
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
  resetSessionStatusPoll,
  applyRunningChanged,
};
Sidebar.wire(API, {
  sidebarSessionIds,
  updateSpinner,
  treeChildIds,
  treeParentId,
});

})();
