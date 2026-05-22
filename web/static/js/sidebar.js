// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Tracks server-reported unread state per session so we can restore the
// unread dot after the spinner hides.
const sessionUnread = {};
globalThis.TuiStatusMap = globalThis.TuiStatusMap || {};
let tuiStatusPollInterval = null;

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
    const r = await fetch('/api/sessions/tui/status');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    globalThis.TuiStatusMap = await r.json();
    refreshTuiDots();
  } catch (err) {
    console.error('fetchTuiStatus failed:', err);
  }
}

function refreshTuiDots() {
  document.querySelectorAll('.tui-status-dot[data-session-id]').forEach(dot => {
    const id = dot.dataset.sessionId;
    const status = globalThis.TuiStatusMap[id] || {running: false, busy: false};
    dot.classList.toggle('running', !!status.running);
    dot.classList.toggle('busy', !!status.running && !!status.busy);
    dot.title = !status.running ? 'Claude stopped' : (status.busy ? 'Claude busy' : 'Claude idle');
  });
  updateTuiHeaderControls(globalThis.ACTIVE_BACKEND_TYPE || '', SESSION_ID);
}

function startTuiStatusPolling() {
  if (tuiStatusPollInterval) return;
  fetchTuiStatus();
  tuiStatusPollInterval = setInterval(fetchTuiStatus, 3000);
}

function updateTuiHeaderControls(backendType, sessionId) {
  const stopBtn = document.getElementById('stop-tui-btn');
  if (!stopBtn) return;
  const isTui = backendType === 'tui-cli';
  const stopped = isTui && globalThis.TuiStatusMap[sessionId]?.running === false;
  stopBtn.classList.toggle('hidden', !isTui || stopped);
  stopBtn.dataset.sessionId = isTui ? sessionId : '';
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

// ---------------------------------------------------------------------------
// SPA-style session switching
// ---------------------------------------------------------------------------
let switchGeneration = 0;
let switching = false;
// Pagination state for tail-loaded sessions
let sessionHasMore = false;
let sessionEarliestEventIndex = Infinity;
let sessionLoadingMore = false;
let activeSessionViewPollInterval = null;
let activeSessionViewPollInflight = false;
const ACTIVE_SESSION_VIEW_POLL_MS = 3000;
let sessionActionModalState = null;
let workersLoadedForSession = null;
let workersLoadInflightForSession = null;
let lazySessionDataTimer = null;

function getDefaultBackendId() {
  const backendIds = Object.keys(BACKEND_OPTIONS || {});
  return backendIds.length ? backendIds[0] : 'claude-opus-4.6';
}

function getActiveBackendId() {
  return globalThis.ACTIVE_BACKEND_ID || getDefaultBackendId();
}

function setActiveBackendId(backendId) {
  globalThis.ACTIVE_BACKEND_ID = backendId || getDefaultBackendId();
}

function updateActiveBackendBadges() {
  const activeBackendId = getActiveBackendId();
  const activeBackendLabel = BACKEND_OPTIONS[activeBackendId] || activeBackendId;

  const backendBadge = document.getElementById('backend-badge');
  if (backendBadge) backendBadge.textContent = activeBackendLabel;

  const inputModelBadge = document.getElementById('input-model-badge');
  if (inputModelBadge) inputModelBadge.textContent = activeBackendLabel;
}

function scheduleIdleTask(fn) {
  if (typeof requestIdleCallback === 'function') {
    return requestIdleCallback(fn, {timeout: 1500});
  }
  return setTimeout(fn, 0);
}

function resetLazySessionData() {
  if (lazySessionDataTimer) {
    if (typeof cancelIdleCallback === 'function') cancelIdleCallback(lazySessionDataTimer);
    else clearTimeout(lazySessionDataTimer);
    lazySessionDataTimer = null;
  }
  if (workersPollInterval) {
    clearInterval(workersPollInterval);
    workersPollInterval = null;
  }
  workersLoadedForSession = null;
  workersLoadInflightForSession = null;
  if (typeof stopAllThreadPolls === 'function') stopAllThreadPolls();
  if (typeof loadedThreads !== 'undefined') loadedThreads.clear();
}

function scheduleLazySessionDataLoad() {
  if (!SESSION_ID || lazySessionDataTimer) return;
  lazySessionDataTimer = scheduleIdleTask(() => {
    lazySessionDataTimer = null;
    pollActiveSessionView({force: true});
    ensureWorkersLoadedForActiveSession();
  });
}

function buildEmptySessionBootstrap(session) {
  const backend = session.backend || getDefaultBackendId();
  return {
    session,
    messages: [],
    pending_draft: null,
    event_count: session.archive_offset || 0,
    active_backend: backend,
    active_backend_type: BACKEND_TYPES ? (BACKEND_TYPES[backend] || '') : '',
    has_more: false,
  };
}

async function switchSession(sessionId) {
  // Welcome screen — no SPA state to swap, fall back to full load
  if (!SESSION_ID) { location.href = '/?session=' + sessionId; return; }
  // Already on this session
  if (sessionId === SESSION_ID) {
    // Same session — but if it's a stopped TUI, force WS reconnect to respawn tmux/claude.
    if (globalThis.TuiStatusMap[sessionId]?.running === false) {
      disconnectWS();
      if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
      connectWS();
      setTimeout(() => { if (typeof fetchTuiStatus === 'function') fetchTuiStatus(); }, 1500);
    }
    return;
  }

  switching = true;
  const gen = ++switchGeneration;

  // Save draft for current session
  if (DRAFT_KEY) {
    const v = document.getElementById('msg-input').value;
    if (v) localStorage.setItem(DRAFT_KEY, v);
    else localStorage.removeItem(DRAFT_KEY);
  }

  // Stop thinking indicator
  if (masterThinking) stopThinking();
  stopActiveSessionViewPolling();
  resetLazySessionData();

  // Clean up transient UI state from previous session
  resetVoiceState();
  uploadedFiles = [];
  renderFileChips();
  hideSlashPopup();
  document.getElementById('text-modal-overlay')?.setAttribute('style', 'display:none');

  // Close WebSocket (suppress auto-reconnect)
  disconnectWS();
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }

  // Reset streaming state
  catchupDone = false;
  pendingUserMsg = false;
  hideStreaming();

  // Fetch critical session bootstrap data
  let data;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/bootstrap');
    if (!res.ok) throw new Error(res.status);
    data = await res.json();
  } catch (err) {
    console.error('switchSession bootstrap fetch failed:', err);
    switching = false;
    location.href = '/?session=' + sessionId;
    return;
  }

  // Discard stale response from rapid clicks
  if (gen !== switchGeneration) return;  // newer switch owns the flag

  // Update globals
  SESSION_ID = sessionId;
  DRAFT_KEY = 'charliebot-draft-' + sessionId;
  THINKING_SINCE = data.session.thinking_since || null;
  eventCursor = data.event_count;
  usageTotalCost = 0;

  // Update URL
  history.pushState({session: sessionId}, '', '/?session=' + sessionId);

  // Render content
  renderSessionView(data);

  // Mark switched-to session as read (WS was closed so broadcast is lost)
  sessionUnread[sessionId] = false;
  const unreadDot = document.getElementById('unread-' + sessionId);
  if (unreadDot) unreadDot.classList.add('hidden');

  // Reconnect WebSocket
  reconnectDelay = 1000;
  connectWS();
  switching = false;

  // Restore draft for new session
  const draft = localStorage.getItem(DRAFT_KEY);
  const inp = document.getElementById('msg-input');
  if (inp) { inp.value = draft || ''; autoResize(inp); }

  // Resume thinking if session was mid-thought.
  // Keep send button enabled — see app.js comment.
  if (THINKING_SINCE) {
    thinkingStart = new Date(THINKING_SINCE).getTime();
    startThinking({keepSendEnabled: true});
  }

  updateSpinner();
  updateSidebarHighlight(sessionId);
  pollSessionStatus();
  ensureActiveSessionViewPolling();

  // Reset lazy-load state
  _backlogLoaded = false;
  scheduleLazySessionDataLoad();
}

function renderSessionView(data) {
  const session = data.session;
  const messages = data.messages || [];
  setActiveBackendId(data.active_backend);
  setActiveRoundRatings(session.round_ratings || {});
  const backendType = data.active_backend_type || (BACKEND_TYPES ? BACKEND_TYPES[data.active_backend] : '') || '';
  if (globalThis.TuiSession) {
    globalThis.TuiSession.syncBackend(backendType, session.id);
  } else {
    globalThis.ACTIVE_BACKEND_TYPE = backendType;
  }
  updateTuiHeaderControls(backendType, session.id);

  // Store pagination state from tail-loaded response
  sessionHasMore = !!data.has_more;
  sessionEarliestEventIndex = Infinity;
  for (const m of messages) {
    if (m.event_index != null && m.event_index < sessionEarliestEventIndex) {
      sessionEarliestEventIndex = m.event_index;
    }
  }
  sessionLoadingMore = false;

  // Update header
  const headerName = document.getElementById('header-session-name');
  if (headerName) {
    headerName.textContent = session.name;
    headerName.setAttribute('onclick', "startRename(event, '" + session.id + "', '" + escapeHtml(session.name).replace(/'/g, "\\'") + "')");
  }

  // Update backend badge
  updateActiveBackendBadges();

  // Update events viewer link
  const evLink = document.querySelector('a[href*="/events"]');
  if (evLink) evLink.href = '/sessions/' + session.id + '/events';

  // Update usage
  renderUsageFromData(data.usage || null);

  // Build message HTML
  const container = document.getElementById('messages');
  if (!container) return;
  renderMessagesIntoContainer(container, messages, session.id);

  if (sessionHasMore) {
    const sentinel = document.createElement('div');
    sentinel.id = 'load-more-sentinel';
    sentinel.className = 'flex justify-center py-3 text-xs text-slate-500';
    sentinel.innerHTML = 'Loading older messages&hellip;';
    container.prepend(sentinel);
  }

  // Initialize streaming preview from pending draft (in-progress assistant
  // response carried over from a tail-loaded session).
  if (data.pending_draft && data.pending_draft.content) {
    showStreaming(data.pending_draft.content);
  } else {
    hideStreaming();
  }

  // Scroll to bottom
  container.scrollTop = container.scrollHeight;

  // Render workers tab
  renderWorkersTab(data.threads || [], session.id, data.triggers || []);

  updateWorkersTabBadge();

  // Restore whichever tab was active before the session switch
  const activeBtn = document.querySelector('#btn-chat-tex.bg-blue-600\\/20, #btn-chat.bg-blue-600\\/20, #btn-workers.bg-blue-600\\/20, #btn-chat-backlog.bg-blue-600\\/20');
  const activeTab = activeBtn ? activeBtn.id.replace('btn-', '') : 'chat';
  switchTab(activeTab);
}

// ---------------------------------------------------------------------------
// Scroll-to-top pagination (loads older messages)
// ---------------------------------------------------------------------------
function initScrollPagination() {
  const container = document.getElementById('messages');
  if (!container) return;
  let debounceTimer = null;
  container.addEventListener('scroll', () => {
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => loadOlderIfNeeded(container), 150);
  });
}

async function loadOlderIfNeeded(container) {
  if (!sessionHasMore || sessionLoadingMore) return;
  // Trigger when within 80px of the top
  if (container.scrollTop > 80) return;
  if (!Number.isFinite(sessionEarliestEventIndex)) return;

  sessionLoadingMore = true;
  const url = '/api/sessions/' + SESSION_ID + '/events?before=' + sessionEarliestEventIndex + '&limit=200';
  const abortCtrl = new AbortController();
  const timeout = setTimeout(() => abortCtrl.abort(), 10000);
  try {
    const res = await fetch(url, {signal: abortCtrl.signal});
    clearTimeout(timeout);
    if (!res.ok) throw new Error(res.status);
    const data = await res.json();

    sessionHasMore = !!data.has_more;

    // Track earliest event index from new messages
    const prevEarliest = sessionEarliestEventIndex;
    for (const m of data.messages) {
      if (m.event_index != null && m.event_index < sessionEarliestEventIndex) {
        sessionEarliestEventIndex = m.event_index;
      }
    }

    // If server says has_more but we made no progress, stop to avoid infinite loop
    if (sessionHasMore && sessionEarliestEventIndex === prevEarliest) {
      sessionHasMore = false;
    }

    // Build HTML for prepended messages
    const sentinel = document.getElementById('load-more-sentinel');
    const prevHeight = container.scrollHeight;

    // Remove old sentinel
    if (sentinel) sentinel.remove();

    // Insert new sentinel if more pages remain
    if (sessionHasMore) {
      const newSentinel = document.createElement('div');
      newSentinel.id = 'load-more-sentinel';
      newSentinel.className = 'flex justify-center py-3 text-xs text-slate-500';
      newSentinel.innerHTML = 'Loading older messages&hellip;';
      container.prepend(newSentinel);
    }

    // Build and prepend message elements
    const tempDiv = document.createElement('div');
    tempDiv.innerHTML = data.messages.map(msg => renderMessage(msg, SESSION_ID)).join('');

    // Insert after sentinel (or at top)
    const insertRef = document.getElementById('load-more-sentinel');
    while (tempDiv.lastChild) {
      if (insertRef) insertRef.after(tempDiv.lastChild);
      else container.prepend(tempDiv.lastChild);
    }

    // Preserve scroll position
    container.scrollTop = container.scrollHeight - prevHeight;
  } catch (err) {
    clearTimeout(timeout);
    console.error('loadOlderMessages failed:', err);
    sessionHasMore = false;
    const sentinel = document.getElementById('load-more-sentinel');
    if (sentinel) sentinel.remove();
  } finally {
    sessionLoadingMore = false;
  }
}

function renderUsageFromData(usage) {
  const indicator = document.getElementById('usage-indicator');
  if (!usage) { if (indicator) indicator.classList.add('hidden'); return; }
  if (indicator) indicator.classList.remove('hidden');

  const contextTokens = usage.context_tokens || 0;
  const contextLimit = usage.context_limit || 200000;
  const pct = contextLimit > 0 ? (contextTokens / contextLimit * 100) : 0;

  const bar = document.getElementById('usage-bar');
  if (bar) {
    bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
    bar.className = 'h-full rounded-full transition-all duration-300 '
      + (pct > 80 ? 'bg-red-500' : pct > 50 ? 'bg-yellow-500' : 'bg-blue-500');
  }
  const text = document.getElementById('usage-text');
  if (text) text.textContent = formatTokens(contextTokens) + ' / ' + formatTokens(contextLimit);
  const cost = document.getElementById('usage-cost');
  if (cost) cost.textContent = '$' + (usage.total_cost_usd || 0).toFixed(2);
}

function formatTriggerTimeLabel(status, fireAt) {
  if (status === 'cancelled') return 'cancelled';
  const prefix = status === 'fired' ? 'fired at ' : 'fires at ';
  const d = new Date(fireAt);
  const mm = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  const hh = String(d.getHours()).padStart(2, '0');
  const mi = String(d.getMinutes()).padStart(2, '0');
  return prefix + mm + '/' + dd + ' ' + hh + ':' + mi;
}

function renderWorkersTab(threads, sessionId, triggers) {
  const container = document.getElementById('tab-workers');
  if (!container) return;

  triggers = triggers || [];

  if ((!threads || !threads.length) && !triggers.length) {
    container.innerHTML = '<div id="no-workers-placeholder" class="flex items-center justify-center h-full text-slate-500 text-sm">No worker threads</div>';
    return;
  }

  const threadCards = (threads || []).map(t => {
    const statusColors = {running: 'bg-blue-500', completed: 'bg-green-500', failed: 'bg-red-500', cancelled: 'bg-slate-500', idle: 'bg-slate-500'};
    const dotColor = statusColors[t.status] || 'bg-slate-500';
    const pulse = t.status === 'running' ? ' animate-pulse' : '';
    const created = new Date(t.created_at);
    const mm = String(created.getMonth() + 1).padStart(2, '0');
    const dd = String(created.getDate()).padStart(2, '0');
    const hh = String(created.getHours()).padStart(2, '0');
    const mi = String(created.getMinutes()).padStart(2, '0');
    const timeStr = mm + '/' + dd + ' ' + hh + ':' + mi;
    let duration = '';
    if (t.completed_at) {
      const secs = Math.floor((new Date(t.completed_at) - created) / 1000);
      duration = ' &middot; ' + Math.floor(secs / 60) + 'm' + (secs % 60) + 's';
    }
    const cancelBtn = t.status === 'running'
      ? '<button id="cancel-btn-' + t.id + '" onclick="event.stopPropagation(); cancelThread(\'' + t.id + '\', \'' + sessionId + '\')" class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel"><svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/></svg></button>'
      : '';
    return '<div class="bg-slate-800 rounded-xl border border-slate-700 overflow-hidden">'
      + '<div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750" onclick="toggleThreadDetail(\'' + t.id + '\', \'' + sessionId + '\')">'
      + '<span id="thread-dot-' + t.id + '" class="w-2 h-2 rounded-full flex-shrink-0 ' + dotColor + pulse + '"></span>'
      + '<div class="flex-1 min-w-0">'
      + '<p class="text-sm truncate cursor-pointer hover:text-blue-400 transition-colors" title="Click to view full description" onclick="event.stopPropagation(); showTextModal(\'Worker Description\', this.dataset.full)" data-full="' + escapeHtml(t.description || '').replace(/"/g, '&quot;') + '">' + escapeHtml(t.description || '') + '</p>'
      + '<p id="thread-status-' + t.id + '" class="text-xs text-slate-500">' + (t.status || 'idle') + ' &middot; ' + timeStr + duration + (t.backend ? ' &middot; ' + (BACKEND_OPTIONS[t.backend] || t.backend) : '') + '</p>'
      + '</div>'
      + cancelBtn
      + '<svg class="w-4 h-4 text-slate-500 transition-transform thread-chevron" id="chevron-' + t.id + '" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/></svg>'
      + '</div>'
      + '<div id="thread-detail-' + t.id + '" class="hidden border-t border-slate-700">'
      + '<div id="thread-events-' + t.id + '" class="p-4 max-h-96 overflow-y-auto"><p class="text-xs text-slate-500">Loading events...</p></div>'
      + '</div></div>';
  });

  const triggerCards = triggers.map(tr => {
    const borderClass = tr.status === 'pending' ? 'border-amber-500/50 border-dashed'
      : tr.status === 'fired' ? 'border-green-500/50' : 'border-slate-600';
    const iconColor = tr.status === 'pending' ? 'text-amber-400'
      : tr.status === 'fired' ? 'text-green-400' : 'text-slate-500';
    const strikeClass = tr.status === 'cancelled' ? ' line-through text-slate-500' : '';
    const timeLabel = formatTriggerTimeLabel(tr.status, tr.fire_at);
    const cancelBtn = tr.status === 'pending'
      ? '<button onclick="event.stopPropagation(); cancelTrigger(\'' + tr.id + '\', \'' + sessionId + '\')" class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel trigger"><svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/></svg></button>'
      : '';
    return '<div id="trigger-card-' + tr.id + '" class="bg-slate-800 rounded-xl border ' + borderClass + ' overflow-hidden">'
      + '<div class="flex items-center gap-3 px-4 py-3">'
      + '<svg id="trigger-dot-' + tr.id + '" class="w-4 h-4 flex-shrink-0 ' + iconColor + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
      + '<circle cx="12" cy="12" r="10" stroke-width="2"/>'
      + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/>'
      + '</svg>'
      + '<div class="flex-1 min-w-0">'
      + '<p class="text-sm truncate' + strikeClass + '">' + escapeHtml(tr.message || '') + '</p>'
      + '<p id="trigger-status-' + tr.id + '" class="text-xs text-slate-500" data-fire-at="' + escapeHtml(tr.fire_at || '') + '">' + timeLabel + '</p>'
      + '</div>'
      + cancelBtn
      + '</div></div>';
  });

  container.innerHTML = threadCards.join('') + triggerCards.join('');
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
  setSessionIndicator(sid, visible ? 'thinking' : 'idle');
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

function updateSpinner() {
  var anyWorkerRunning = Array.from(document.querySelectorAll('[id^="thread-status-"]'))
    .some(function(el) { return el.textContent.startsWith('running'); });
  setSessionIndicator(SESSION_ID, getSessionIndicatorState({
    thinking_since: masterThinking ? (THINKING_SINCE || true) : null,
    has_running_tasks: anyWorkerRunning,
  }));
}

function stopActiveSessionViewPolling() {
  if (activeSessionViewPollInterval) {
    clearInterval(activeSessionViewPollInterval);
    activeSessionViewPollInterval = null;
  }
}

function ensureActiveSessionViewPolling() {
  if (!SESSION_ID || (!masterThinking && !THINKING_SINCE)) {
    stopActiveSessionViewPolling();
    return;
  }
  if (activeSessionViewPollInterval) return;
  activeSessionViewPollInterval = setInterval(pollActiveSessionView, ACTIVE_SESSION_VIEW_POLL_MS);
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
    usageTotalCost = data.usage ? (data.usage.total_cost_usd || 0) : 0;
    renderUsageFromData(data.usage);

    if (!THINKING_SINCE && masterThinking) {
      stopThinking();
    }
  } catch (err) {
    console.error('pollActiveSessionView failed:', err);
  } finally {
    activeSessionViewPollInflight = false;
    ensureActiveSessionViewPolling();
  }
}

// Poll-based sidebar status (corrects WS drift)
let statusPollInterval = null;
let statusPollInflight = false;
let statusPollMs = 3000;
let statusPollPromise = Promise.resolve(false);
let statusPollQueued = false;

function applySessionStatus(sid, status) {
  sessionUnread[sid] = status.has_unread;
  setSessionIndicator(sid, getSessionIndicatorState(status));
  setSessionPendingTriggerIndicator(sid, status);
}

function refreshSessionStatusNow(opts) {
  if (opts && opts.refreshWorkers) pollWorkers();
  if (!statusPollInflight) return pollSessionStatus();

  statusPollQueued = true;
  return statusPollPromise.then(() => {
    if (!statusPollQueued) return false;
    statusPollQueued = false;
    return pollSessionStatus();
  });
}

function pollSessionStatus() {
  if (statusPollInflight) return statusPollPromise;
  statusPollInflight = true;
  statusPollPromise = fetch('/api/sessions/status')
    .then(r => r.ok ? r.json() : null)
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

// Poll-based workers tab updates (replaces WS-driven addWorkerCard/updateWorkerStatus)
let workersPollInterval = null;

function renderWorkersListItems(items, sessionId) {
  const threads = [];
  const triggers = [];
  for (const item of items || []) {
    if (item.type === 'trigger') triggers.push(item);
    else threads.push(item);
  }
  renderWorkersTab(threads, sessionId, triggers);
  updateWorkersTabBadge();
}

function restartWorkersPolling() {
  if (workersPollInterval) clearInterval(workersPollInterval);
  workersPollInterval = setInterval(pollWorkers, 3000);
}

async function ensureWorkersLoadedForActiveSession(opts) {
  const force = opts && opts.force;
  const pollSessionId = SESSION_ID;
  if (!pollSessionId) return;
  if (!force && workersLoadedForSession === pollSessionId) return;
  if (workersLoadInflightForSession === pollSessionId) return;
  workersLoadInflightForSession = pollSessionId;
  try {
    const res = await fetch('/api/threads/' + pollSessionId + '/list');
    if (!res.ok) throw new Error(res.status);
    const items = await res.json();
    if (pollSessionId !== SESSION_ID) return;
    renderWorkersListItems(items || [], pollSessionId);
    workersLoadedForSession = pollSessionId;
    restartWorkersPolling();
  } catch (err) {
    console.error('loadWorkers failed:', err);
  } finally {
    if (workersLoadInflightForSession === pollSessionId) workersLoadInflightForSession = null;
  }
}

function pollWorkers() {
  const pollSessionId = SESSION_ID;
  if (!pollSessionId) return;
  if (workersLoadedForSession !== pollSessionId) {
    ensureWorkersLoadedForActiveSession({force: true});
    return;
  }
  fetch('/api/threads/' + pollSessionId + '/list')
    .then(r => r.ok ? r.json() : null)
    .then(items => {
      if (!items || pollSessionId !== SESSION_ID) return;
      for (const item of items) {
        if (item.type === 'trigger') {
          const existing = document.getElementById('trigger-dot-' + item.id);
          if (!existing) {
            addTriggerCard(item.id, item.message, item.fire_at, item.created_at, item.status);
          } else {
            updateTriggerStatus(item.id, item.status);
          }
        } else {
          const existing = document.getElementById('thread-dot-' + item.id);
          if (!existing) {
            addWorkerCard(item.id, item.description, item.created_at, item.backend || '');
            if (item.status !== 'running') updateWorkerStatus(item.id, item.status);
          } else {
            updateWorkerStatus(item.id, item.status);
          }
        }
      }
    })
    .catch(err => console.error('pollWorkers failed:', err));
}

function updateWorkersTabBadge() {
  var btn = document.getElementById('btn-workers');
  if (!btn) return;
  var count = document.querySelectorAll('[id^="thread-dot-"]').length
    + document.querySelectorAll('[id^="trigger-dot-"]').length;
  var badge = btn.querySelector('span');
  if (count > 0) {
    if (!badge) {
      badge = document.createElement('span');
      badge.className = 'ml-1 text-xs bg-slate-600 px-1.5 py-0.5 rounded-full';
      btn.appendChild(badge);
    }
    badge.textContent = count;
  } else if (badge) {
    badge.remove();
  }
}

// ---------------------------------------------------------------------------
// Workers tab live updates
// ---------------------------------------------------------------------------
const finalFetchDone = new Set();

const STATUS_DOT_COLORS = {
  running: 'bg-blue-500', completed: 'bg-green-500',
  failed: 'bg-red-500', cancelled: 'bg-slate-500', idle: 'bg-slate-500',
};

function updateWorkerStatus(threadId, status) {
  const dot = document.getElementById('thread-dot-' + threadId);
  const text = document.getElementById('thread-status-' + threadId);
  if (!dot || !text) return;
  dot.className = 'w-2 h-2 rounded-full flex-shrink-0 ' + (STATUS_DOT_COLORS[status] || 'bg-slate-500');
  // preserve timestamp portion if present
  const cur = text.textContent;
  const dotIdx = cur.indexOf(' · ');
  const suffix = dotIdx !== -1 ? cur.substring(dotIdx) : '';
  text.textContent = status + suffix;
  const cancelBtn = document.getElementById('cancel-btn-' + threadId);
  if (cancelBtn) cancelBtn.style.display = status === 'running' ? '' : 'none';

  // If worker transitions back to running, allow a future final fetch
  if (status === 'running') {
    finalFetchDone.delete(threadId);
    return;
  }

  // When worker finishes, stop auto-poll and do one final fetch
  stopThreadPoll(threadId);
  if (!finalFetchDone.has(threadId)) {
    finalFetchDone.add(threadId);
    loadedThreads.delete(threadId);
    // If detail is currently expanded, do a final fetch
    const detail = document.getElementById('thread-detail-' + threadId);
    if (detail && !detail.classList.contains('hidden')) {
      fetchAndRenderEvents(threadId, SESSION_ID).catch(e => console.warn('Final event fetch failed:', e));
    }
  }
}

function addWorkerCard(threadId, description, createdAt, backend) {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  // Remove placeholder if present
  document.getElementById('no-workers-placeholder')?.remove();
  // Don't add duplicate
  if (document.getElementById('thread-dot-' + threadId)) return;
  const card = document.createElement('div');
  card.className = 'bg-slate-800 rounded-xl border border-slate-700 overflow-hidden';
  const nowStr = (() => {
    const d = createdAt ? new Date(createdAt) : new Date();
    const mm = String(d.getMonth() + 1).padStart(2, '0');
    const dd = String(d.getDate()).padStart(2, '0');
    const hh = String(d.getHours()).padStart(2, '0');
    const mi = String(d.getMinutes()).padStart(2, '0');
    return `${mm}/${dd} ${hh}:${mi}`;
  })();
  card.innerHTML = `
    <div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750"
         onclick="toggleThreadDetail('${threadId}', '${SESSION_ID}')">
      <span id="thread-dot-${threadId}" class="w-2 h-2 rounded-full flex-shrink-0 bg-blue-500 animate-pulse"></span>
      <div class="flex-1 min-w-0">
        <p class="text-sm truncate cursor-pointer hover:text-blue-400 transition-colors" title="Click to view full description" onclick="event.stopPropagation(); showTextModal('Worker Description', this.dataset.full)" data-full="${escapeHtml(description)}">${escapeHtml(description)}</p>
        <p id="thread-status-${threadId}" class="text-xs text-slate-500">running &middot; ${nowStr}${backend ? ' &middot; ' + (BACKEND_OPTIONS[backend] || backend) : ''}</p>
      </div>
      <button id="cancel-btn-${threadId}" onclick="event.stopPropagation(); cancelThread('${threadId}', '${SESSION_ID}')"
              class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/>
        </svg>
      </button>
      <svg class="w-4 h-4 text-slate-500 transition-transform thread-chevron" id="chevron-${threadId}"
           fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
      </svg>
    </div>
    <div id="thread-detail-${threadId}" class="hidden border-t border-slate-700">
      <div id="thread-events-${threadId}" class="p-4 max-h-96 overflow-y-auto">
        <p class="text-xs text-slate-500">Loading events...</p>
      </div>
    </div>`;
  container.prepend(card);
  updateWorkersTabBadge();
}

// ---------------------------------------------------------------------------
// Trigger card functions
// ---------------------------------------------------------------------------

function addTriggerCard(triggerId, message, fireAt, createdAt, status) {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  document.getElementById('no-workers-placeholder')?.remove();
  if (document.getElementById('trigger-dot-' + triggerId)) return;

  const borderClass = status === 'pending' ? 'border-amber-500/50 border-dashed'
    : status === 'fired' ? 'border-green-500/50' : 'border-slate-600';
  const iconColor = status === 'pending' ? 'text-amber-400'
    : status === 'fired' ? 'text-green-400' : 'text-slate-500';
  const strikeClass = status === 'cancelled' ? 'line-through text-slate-500' : '';
  const timeLabel = formatTriggerTimeLabel(status, fireAt);

  const cancelBtn = status === 'pending'
    ? '<button onclick="event.stopPropagation(); cancelTrigger(\'' + triggerId + '\', \'' + SESSION_ID + '\')" class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel trigger"><svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/></svg></button>'
    : '';

  const card = document.createElement('div');
  card.className = 'bg-slate-800 rounded-xl border ' + borderClass + ' overflow-hidden';
  card.id = 'trigger-card-' + triggerId;
  card.innerHTML = '<div class="flex items-center gap-3 px-4 py-3">'
    + '<svg id="trigger-dot-' + triggerId + '" class="w-4 h-4 flex-shrink-0 ' + iconColor + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<circle cx="12" cy="12" r="10" stroke-width="2"/>'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/>'
    + '</svg>'
    + '<div class="flex-1 min-w-0">'
    + '<p class="text-sm truncate ' + strikeClass + '">' + escapeHtml(message) + '</p>'
    + '<p id="trigger-status-' + triggerId + '" class="text-xs text-slate-500" data-fire-at="' + escapeHtml(fireAt || '') + '">' + timeLabel + '</p>'
    + '</div>'
    + cancelBtn
    + '</div>';
  container.appendChild(card);
  updateWorkersTabBadge();
}

function updateTriggerStatus(triggerId, status) {
  const icon = document.getElementById('trigger-dot-' + triggerId);
  const text = document.getElementById('trigger-status-' + triggerId);
  const card = document.getElementById('trigger-card-' + triggerId);
  if (!icon) return;

  const iconColor = status === 'pending' ? 'text-amber-400'
    : status === 'fired' ? 'text-green-400' : 'text-slate-500';
  icon.className = 'w-4 h-4 flex-shrink-0 ' + iconColor;

  if (card) {
    card.className = 'bg-slate-800 rounded-xl border overflow-hidden '
      + (status === 'pending' ? 'border-amber-500/50 border-dashed'
        : status === 'fired' ? 'border-green-500/50' : 'border-slate-600');
  }

  if (text) {
    const fireAt = text.dataset.fireAt || '';
    if (status === 'cancelled') {
      text.textContent = 'cancelled';
    } else if (fireAt) {
      text.textContent = formatTriggerTimeLabel(status, fireAt);
    }
  }

  // Hide cancel button for non-pending
  if (status !== 'pending' && card) {
    const cancelBtn = card.querySelector('button[title="Cancel trigger"]');
    if (cancelBtn) cancelBtn.style.display = 'none';
  }

  // Add strikethrough for cancelled
  if (status === 'cancelled' && card) {
    const msg = card.querySelector('p.text-sm');
    if (msg) { msg.classList.add('line-through', 'text-slate-500'); }
  }
}

function cancelTrigger(triggerId, sessionId) {
  fetch('/api/internal/triggers/' + sessionId + '/' + triggerId + '/cancel', {method: 'POST'})
    .then(r => { if (r.ok) updateTriggerStatus(triggerId, 'cancelled'); })
    .catch(err => console.error('Cancel trigger failed:', err));
}

// ---------------------------------------------------------------------------
// Thinking indicator
// ---------------------------------------------------------------------------
let thinkingInterval = null;
let thinkingStart = null;

function startThinking(opts) {
  masterThinking = true;
  thinkingStart = thinkingStart || Date.now();
  document.getElementById('thinking').classList.remove('hidden');
  updateThinkingTime();
  thinkingInterval = setInterval(updateThinkingTime, 1000);
  if (!(opts && opts.keepSendEnabled)) {
    document.getElementById('send-btn').disabled = true;
    document.getElementById('send-btn').classList.add('opacity-50');
  }
  setSessionIndicator(SESSION_ID, 'thinking');
  ensureActiveSessionViewPolling();
}

function stopThinking(opts) {
  masterThinking = false;
  document.getElementById('thinking').classList.add('hidden');
  if (thinkingInterval) { clearInterval(thinkingInterval); thinkingInterval = null; }
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

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------
async function createSession() {
  try {
    const backendSel = document.getElementById('new-session-backend');
    const backend = backendSel ? backendSel.value : undefined;
    const res = await fetch('/api/sessions/', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ backend }),
    });
    if (!res.ok) throw new Error(`Create session failed: ${res.status}`);
    const data = await res.json();
    if (!SESSION_ID) {
      location.href = '/?session=' + data.id;
      return;
    }

    switching = true;
    ++switchGeneration;
    if (DRAFT_KEY) {
      const input = document.getElementById('msg-input');
      const draft = input ? input.value : '';
      if (draft) localStorage.setItem(DRAFT_KEY, draft);
      else localStorage.removeItem(DRAFT_KEY);
    }
    if (masterThinking) stopThinking();
    stopActiveSessionViewPolling();
    resetLazySessionData();
    resetVoiceState();
    uploadedFiles = [];
    renderFileChips();
    hideSlashPopup();
    disconnectWS();
    if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
    catchupDone = false;
    pendingUserMsg = false;
    hideStreaming();

    const bootstrap = buildEmptySessionBootstrap(data);
    SESSION_ID = data.id;
    DRAFT_KEY = 'charliebot-draft-' + data.id;
    THINKING_SINCE = data.thinking_since || null;
    eventCursor = bootstrap.event_count;
    usageTotalCost = 0;
    history.pushState({session: data.id}, '', '/?session=' + data.id);
    renderSessionView(bootstrap);
    reconnectDelay = 1000;
    connectWS();
    switching = false;

    const input = document.getElementById('msg-input');
    if (input) {
      input.value = '';
      autoResize(input);
      input.focus();
    }
    setSidebarFilterPill('all');
    updateSidebarHighlight(data.id);
    setTimeout(() => switchSidebarFilter('all'), 0);
    scheduleLazySessionDataLoad();
  } catch (err) {
    console.error('Create session failed:', err);
  }
}

function markSessionRead(id) {
  // Optimistically hide the unread dot
  const dot = document.getElementById('unread-' + id);
  if (dot) dot.classList.add('hidden');
  // Fire-and-forget API call
  fetch(`/api/sessions/${id}/read`, { method: 'POST' }).catch(() => {});
}

async function archiveSession(id) {
  try {
    const res = await fetch(`/api/sessions/${id}`, { method: 'DELETE' });
    if (!res.ok) throw new Error(`Archive failed: ${res.status}`);
    if (SESSION_ID === id) {
      location.href = '/?session=';
    } else {
      switchSidebarFilter(currentFilter);
    }
  } catch (err) {
    console.error('Archive failed:', err);
  }
}

async function unarchiveSession(id) {
  try {
    await fetch(`/api/sessions/${id}/unarchive`, { method: 'POST' });
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Unarchive failed:', err);
  }
}

async function stopActiveTui() {
  if (!SESSION_ID) return;
  if (!confirm('Stop the claude process for this session? You can reopen to resume.')) return;
  const sessionId = SESSION_ID;
  try {
    const res = await fetch(`/api/sessions/${sessionId}/tui/stop`, { method: 'POST' });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(body.detail || `HTTP ${res.status}`);
    }
    const data = await res.json();
    if (data.stopped !== true) throw new Error('Stop endpoint did not return stopped=true');
    globalThis.TuiStatusMap[sessionId] = {running: false, busy: false};
    refreshTuiDots();
    if (globalThis.TuiSession && globalThis.TuiSession.showStoppedBanner) {
      globalThis.TuiSession.showStoppedBanner();
    }
  } catch (err) {
    showToast('Stop Claude failed: ' + err.message, true);
    console.error('Stop Claude failed:', err);
  }
}

let deleteConfirmKeyHandler = null;

function closeDeleteConfirmModal() {
  document.getElementById('delete-confirm-overlay')?.remove();
  if (deleteConfirmKeyHandler) {
    document.removeEventListener('keydown', deleteConfirmKeyHandler);
    deleteConfirmKeyHandler = null;
  }
}

function confirmDeletePermanently(sessionId) {
  closeDeleteConfirmModal();

  const overlay = document.createElement('div');
  overlay.id = 'delete-confirm-overlay';
  overlay.className = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';
  overlay.innerHTML = `
    <div class="bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72 text-center"
         onclick="event.stopPropagation()">
      <svg class="w-8 h-8 text-red-400 mx-auto mb-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2"
              d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/>
      </svg>
      <p class="text-sm font-semibold text-slate-200 mb-1">Delete permanently?</p>
      <p class="text-xs text-slate-400 mb-4">This will permanently delete this session and all its data. This cannot be undone.</p>
      <div class="flex gap-2 justify-center">
        <button id="confirm-cancel-btn" class="px-3 py-1.5 text-xs rounded-lg bg-slate-700 hover:bg-slate-600 text-slate-300 transition-colors">Cancel</button>
        <button id="confirm-delete-btn" class="px-3 py-1.5 text-xs rounded-lg bg-red-600 hover:bg-red-500 text-white transition-colors">Delete</button>
      </div>
    </div>`;

  overlay.querySelector('#confirm-cancel-btn').addEventListener('click', closeDeleteConfirmModal);
  overlay.querySelector('#confirm-delete-btn').addEventListener('click', async () => {
    closeDeleteConfirmModal();
    try {
      const res = await fetch(`/api/sessions/${sessionId}/permanent`, { method: 'DELETE' });
      if (!res.ok) throw new Error(`Permanent delete failed: ${res.status}`);
      if (SESSION_ID === sessionId) {
        location.href = '/?session=';
      } else {
        switchSidebarFilter(currentFilter);
      }
    } catch (err) {
      console.error('Permanent delete failed:', err);
    }
  });
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay) closeDeleteConfirmModal();
  });
  deleteConfirmKeyHandler = (e) => {
    if (e.key === 'Escape') closeDeleteConfirmModal();
  };
  document.addEventListener('keydown', deleteConfirmKeyHandler);

  document.body.appendChild(overlay);
}

// ---------------------------------------------------------------------------
// Sidebar filter & star
// ---------------------------------------------------------------------------
let currentFilter = 'all';

function setSidebarFilterPill(filter) {
  currentFilter = filter;
  document.querySelectorAll('.filter-pill').forEach(btn => {
    btn.classList.remove('bg-blue-600/20', 'text-blue-300');
    btn.classList.add('text-slate-400');
  });
  const active = document.getElementById('filter-' + filter);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('text-slate-400');
  }
  const addBtn = document.getElementById('cron-add-btn');
  if (addBtn) addBtn.classList.toggle('hidden', filter !== 'scheduled');
}

function switchSidebarFilter(filter) {
  setSidebarFilterPill(filter);
  // Fetch sessions for this filter
  const urls = {
    all: '/api/sessions/',
    starred: '/api/sessions/starred',
    archived: '/api/sessions/archived',
    scheduled: '/api/sessions/scheduled',
  };
  fetch(urls[filter])
    .then(res => res.json())
    .then(sessions => renderSessionList(sessions, filter))
    .catch(err => console.error('Filter fetch failed:', err));
}

// ---------------------------------------------------------------------------
// Session search
// ---------------------------------------------------------------------------
let searchDebounceTimer = null;

function handleSidebarSearch(query) {
  clearTimeout(searchDebounceTimer);
  const pills = document.querySelector('.filter-pill')?.parentElement;
  const addBtn = document.getElementById('cron-add-btn');
  if (query.trim()) {
    // Hide filter pills while searching
    if (pills) pills.style.display = 'none';
    searchDebounceTimer = setTimeout(() => {
      fetch('/api/sessions/search?q=' + encodeURIComponent(query.trim()))
        .then(res => res.json())
        .then(sessions => renderSessionList(sessions, 'search'))
        .catch(err => console.error('Search failed:', err));
    }, 300);
  } else {
    // Restore filter pills and current filter
    if (pills) pills.style.display = '';
    switchSidebarFilter(currentFilter);
  }
}

async function toggleSessionStar(id, currentlyStarred) {
  const endpoint = currentlyStarred ? 'unstar' : 'star';
  // Optimistic UI update
  const btn = document.getElementById('star-' + id);
  if (btn) {
    const svg = btn.querySelector('svg');
    if (currentlyStarred) {
      svg.setAttribute('fill', 'none');
      btn.classList.remove('text-yellow-400', '!opacity-100');
      btn.classList.add('hover:text-yellow-400');
      btn.setAttribute('onclick', `event.preventDefault(); event.stopPropagation(); toggleSessionStar('${id}', false)`);
    } else {
      svg.setAttribute('fill', 'currentColor');
      btn.classList.add('text-yellow-400', '!opacity-100');
      btn.classList.remove('hover:text-yellow-400');
      btn.setAttribute('onclick', `event.preventDefault(); event.stopPropagation(); toggleSessionStar('${id}', true)`);
    }
  }
  try {
    await fetch(`/api/sessions/${id}/${endpoint}`, { method: 'POST' });
    // If viewing starred filter and we just unstarred, remove from list
    if (currentFilter === 'starred' && currentlyStarred) {
      switchSidebarFilter(currentFilter);
    }
  } catch (err) {
    console.error('Star toggle failed:', err);
  }
}

function escapeHtmlAttr(str) {
  return escapeHtml(str).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

async function showGroupSelector(sessionId, currentGroup) {
  // Fetch existing groups
  let groups = [];
  try {
    const res = await fetch('/api/sessions/groups');
    if (!res.ok) throw new Error(`Fetch groups failed: ${res.status}`);
    groups = await res.json();
  } catch (err) {
    console.error('Fetch groups failed:', err);
    return;
  }

  // Remove any existing modal
  document.getElementById('group-modal-overlay')?.remove();

  const overlay = document.createElement('div');
  overlay.id = 'group-modal-overlay';
  overlay.className = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';

  const groupButtons = groups.map(g => {
    const isActive = g === currentGroup;
    const activeClass = isActive ? 'bg-purple-600/30 text-purple-300 border-purple-500/50' : 'bg-slate-700 hover:bg-slate-600 text-slate-300 border-transparent';
    return `<button data-group="${escapeHtmlAttr(g)}" class="w-full text-left px-3 py-2 rounded-lg text-sm border transition-colors ${activeClass}">${escapeHtml(g)}</button>`;
  }).join('');

  overlay.innerHTML = `
    <div class="bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72"
         onclick="event.stopPropagation()">
      <p class="text-sm text-slate-300 mb-3 font-semibold">Set Group</p>
      <div class="flex flex-col gap-1.5 mb-3 max-h-48 overflow-y-auto">
        ${currentGroup ? `<button data-group="" class="w-full text-left px-3 py-2 rounded-lg text-sm bg-slate-700 hover:bg-red-600/20 hover:text-red-300 text-slate-400 transition-colors">Remove group</button>` : ''}
        ${groupButtons}
      </div>
      <div class="flex gap-2">
        <input id="new-group-input" type="text" placeholder="New group name..."
               class="flex-1 bg-slate-700 border border-slate-600 rounded-lg px-3 py-1.5 text-sm text-slate-200 placeholder-slate-500 focus:outline-none focus:border-purple-500">
        <button id="new-group-btn" class="px-3 py-1.5 text-sm rounded-lg bg-purple-600 hover:bg-purple-500 text-white transition-colors">Add</button>
      </div>
    </div>`;

  // Handle existing group clicks
  overlay.querySelectorAll('[data-group]').forEach(btn => {
    btn.addEventListener('click', () => {
      const group = btn.dataset.group || null;
      document.getElementById('group-modal-overlay')?.remove();
      setSessionGroup(sessionId, group);
    });
  });

  // Handle new group
  const addNewGroup = () => {
    const input = document.getElementById('new-group-input');
    const name = input.value.trim();
    if (!name) return;
    document.getElementById('group-modal-overlay')?.remove();
    setSessionGroup(sessionId, name);
  };
  overlay.querySelector('#new-group-btn').addEventListener('click', addNewGroup);
  overlay.querySelector('#new-group-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') addNewGroup();
    if (e.key === 'Escape') document.getElementById('group-modal-overlay')?.remove();
  });

  // Close on overlay click
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay) overlay.remove();
  });
  // Close on Escape
  const escHandler = (e) => {
    if (e.key === 'Escape') {
      document.getElementById('group-modal-overlay')?.remove();
      document.removeEventListener('keydown', escHandler);
    }
  };
  document.addEventListener('keydown', escHandler);

  document.body.appendChild(overlay);
  document.getElementById('new-group-input').focus();
}

async function setSessionGroup(sessionId, group) {
  try {
    const res = await fetch(`/api/sessions/${sessionId}/group`, {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({group}),
    });
    if (!res.ok) throw new Error(`Set group failed: ${res.status}`);
    // Refresh the sidebar
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Set group failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Grouped scheduled task rendering
// ---------------------------------------------------------------------------
function renderScheduledSessionItem(s) {
  const isActive = SESSION_ID === s.id;
  const indicatorState = getSessionIndicatorState(s);
  const activeClass = isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
  const starFill = s.starred ? 'currentColor' : 'none';
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const starSvg = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;
  const gearBtn = s.scheduled_task ? `
    <button onclick="event.preventDefault(); event.stopPropagation(); openCronEditor('${escapeHtml(s.scheduled_task)}')"
            class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Edit task config">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><circle cx="12" cy="12" r="3"/></svg>
    </button>` : '';
  const actions = `
    <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
            class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
      <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
    </button>
    <button onclick="event.preventDefault(); event.stopPropagation(); startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
            class="opacity-0 group-hover:opacity-100 p-1 hover:text-blue-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Rename">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
    </button>
    <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
            class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
    </button>
    ${gearBtn}`;
  return `<a href="/?session=${s.id}&filter=scheduled"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}"
     ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
     onclick="event.preventDefault(); switchSession('${s.id}')"
     id="session-${s.id}">
    <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
    <svg id="worker-indicator-${s.id}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/></svg>
    <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && indicatorState === 'idle' ? '' : 'hidden'}"></span>
    ${renderPendingTriggerIndicator(s)}
    <svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task || '')}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>
    ${renderTuiStatusDot(s)}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : ''}
      ${s.last_run_status ? `<span class="block text-xs ${s.last_run_status === 'success' ? 'text-green-400' : s.last_run_status === 'running' ? 'text-yellow-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400')}">Last: ${escapeHtml(s.last_run_status)}${s.last_scheduled_run ? ', ' + formatLastRun(s.last_scheduled_run) : ''}${s.last_run_status === 'failed' && s.schedule_allow_failure ? ' (review needed)' : ''}</span>` : ''}
    </span>
    ${actions}
  </a>`;
}

function renderGroupedScheduledList(sessions) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    nav.innerHTML = '<p class="text-slate-500 text-sm px-3 py-2">No scheduled sessions</p>';
    return;
  }
  // Group by project
  const groups = {};
  sessions.forEach(s => {
    const key = s.schedule_project || '';
    if (!groups[key]) groups[key] = [];
    groups[key].push(s);
  });
  // Sort: named groups alphabetically, '' (no project) last
  const sortedKeys = Object.keys(groups).sort((a, b) => {
    if (a === '') return 1;
    if (b === '') return -1;
    return a.localeCompare(b);
  });
  // Load collapsed state from localStorage (collapsed by default)
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('cron-group-collapsed') || '{}'); } catch (e) {}

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No project)';
    const groupSessions = groups[key];
    const enabledCount = groupSessions.filter(s => s.schedule_enabled !== false).length;
    const totalCount = groupSessions.length;
    const isCollapsed = collapsedState[key] !== false; // collapsed by default
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtml(key);

    html += `<div class="cron-group" data-group-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           onclick="toggleCronGroup('${safeKey}')">
        <svg class="w-3 h-3 text-slate-500 transition-transform cron-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        <span class="text-xs text-slate-500 ml-auto">${enabledCount}/${totalCount} enabled</span>
      </div>
      <div class="cron-group-items ${isCollapsed ? 'hidden' : ''}" data-group-items="${safeKey}">
        ${groupSessions.map(s => renderScheduledSessionItem(s)).join('')}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
}

function toggleCronGroup(key) {
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('cron-group-collapsed') || '{}'); } catch (e) {}
  const wasCollapsed = collapsedState[key] !== false;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem('cron-group-collapsed', JSON.stringify(collapsedState));

  const items = document.querySelector(`[data-group-items="${key}"]`);
  if (items) items.classList.toggle('hidden');
  const group = document.querySelector(`[data-group-key="${key}"]`);
  if (group) {
    const chevron = group.querySelector('.cron-group-chevron');
    if (chevron) chevron.classList.toggle('rotate-90');
  }
}

// ---------------------------------------------------------------------------
// Grouped session list rendering (by session.group)
// ---------------------------------------------------------------------------
function renderGroupedSessionList(sessions, filter) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    nav.innerHTML = '<p class="text-slate-500 text-sm px-3 py-2">No sessions yet</p>';
    return;
  }
  // Group by s.group
  const groups = {};
  sessions.forEach(s => {
    const key = s.group || '';
    if (!groups[key]) groups[key] = [];
    groups[key].push(s);
  });
  // Sort: named groups alphabetically, '' (no group) last
  const sortedKeys = Object.keys(groups).sort((a, b) => {
    if (a === '') return 1;
    if (b === '') return -1;
    return a.localeCompare(b);
  });
  // Load collapsed state from localStorage (expanded by default)
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('session-group-collapsed') || '{}'); } catch (e) {}

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No group)';
    const groupSessions = groups[key];
    const isCollapsed = collapsedState[key] === true; // expanded by default
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtmlAttr(key);

    const groupActions = key ? `
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); renameGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-blue-400 transition-opacity" title="Rename group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
      </button>
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); deleteGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-red-400 transition-opacity" title="Delete group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
      </button>` : '';

    html += `<div class="session-group group" data-sgroup-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           data-sgroup-toggle-key="${safeKey}"
           onclick="toggleSessionGroup(this.dataset.sgroupToggleKey)">
        <svg class="w-3 h-3 text-slate-500 transition-transform session-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        ${groupActions}
        <span class="text-xs text-slate-500 ml-auto">${groupSessions.length}</span>
      </div>
      <div class="session-group-items ${isCollapsed ? 'hidden' : ''}" data-sgroup-items="${safeKey}">
        ${groupSessions.map(s => renderSessionItem(s, filter)).join('')}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
}

function toggleSessionGroup(key) {
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('session-group-collapsed') || '{}'); } catch (e) {}
  const wasCollapsed = collapsedState[key] === true;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem('session-group-collapsed', JSON.stringify(collapsedState));

  const items = Array.from(document.querySelectorAll('.session-group-items'))
    .find(el => el.dataset.sgroupItems === key);
  if (items) items.classList.toggle('hidden');
  const group = Array.from(document.querySelectorAll('.session-group'))
    .find(el => el.dataset.sgroupKey === key);
  if (group) {
    const chevron = group.querySelector('.session-group-chevron');
    if (chevron) chevron.classList.toggle('rotate-90');
  }
}

async function renameGroup(oldName) {
  const newName = prompt(`Rename group "${oldName}" to:`, oldName);
  if (!newName || newName.trim() === '' || newName.trim() === oldName) return;
  const res = await fetch('/api/sessions/groups/rename', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({old_name: oldName, new_name: newName.trim()}),
  });
  if (!res.ok) throw new Error(`Rename group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

async function deleteGroup(groupName) {
  if (!confirm(`Remove group "${groupName}"? Sessions will be ungrouped.`)) return;
  const res = await fetch('/api/sessions/groups/delete', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({group: groupName}),
  });
  if (!res.ok) throw new Error(`Delete group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

function renderSessionItem(s, filter) {
  const starSvg = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;
  const isActive = SESSION_ID === s.id;
  const indicatorState = getSessionIndicatorState(s);
  const activeClass = isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
  const starFill = s.starred ? 'currentColor' : 'none';
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const timeStr = s.updated_at ? relativeTime(s.updated_at) : '';
  const timeIso = s.updated_at || '';
  const groupBtn = `
    <button data-current-group="${s.group ? escapeHtmlAttr(s.group) : ''}"
            onclick="event.preventDefault(); event.stopPropagation(); showGroupSelector('${s.id}', this.dataset.currentGroup || null)"
            class="opacity-0 group-hover:opacity-100 p-1 hover:text-purple-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Set group">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z"/></svg>
    </button>`;
  let actions = '';
  if (filter === 'archived') {
    const ratingBadge = s.rating === 'thumbs_up' ? '<span class="text-xs flex-shrink-0" title="Rated: thumbs up">👍</span>'
      : s.rating === 'neutral' ? '<span class="text-xs flex-shrink-0" title="Rated: neutral">—</span>'
      : s.rating === 'thumbs_down' ? '<span class="text-xs flex-shrink-0" title="Rated: thumbs down">👎</span>'
      : '';
    actions = `
      ${ratingBadge}
      <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
              class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
        <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
      </button>
      ${groupBtn}
      <button onclick="event.preventDefault(); event.stopPropagation(); unarchiveSession('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 hover:text-green-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Unarchive">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5-5m0 0l5 5m-5-5v12"/></svg>
      </button>
      <button onclick="event.preventDefault(); event.stopPropagation(); confirmDeletePermanently('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 text-slate-500 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Delete permanently">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
      </button>`;
  } else {
    const gearBtn = (filter === 'scheduled' && s.scheduled_task) ? `
      <button onclick="event.preventDefault(); event.stopPropagation(); openCronEditor('${escapeHtml(s.scheduled_task)}')"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Edit task config">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><circle cx="12" cy="12" r="3"/></svg>
      </button>` : '';
    actions = `
      <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
              class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
        <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
      </button>
      <button onclick="event.preventDefault(); event.stopPropagation(); startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
              class="opacity-0 group-hover:opacity-100 p-1 hover:text-blue-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Rename">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
      </button>
      ${groupBtn}
      <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
      </button>
      ${gearBtn}`;
  }
  return `<a href="/?session=${s.id}&filter=${filter}"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}"
     ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
     onclick="event.preventDefault(); switchSession('${s.id}')"
     id="session-${s.id}">
    <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
    <svg id="worker-indicator-${s.id}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/></svg>
    <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && indicatorState === 'idle' ? '' : 'hidden'}"></span>
    ${renderPendingTriggerIndicator(s)}
    ${s.scheduled_task ? `<svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task)}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>` : ''}
    ${renderTuiStatusDot(s)}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${filter === 'scheduled' && s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : `<span class="block text-xs text-slate-500 session-time" data-time="${timeIso}">${timeStr}</span>`}
    </span>
    ${actions}
  </a>`;
}

function renderSessionList(sessions, filter) {
  if (filter === 'scheduled') {
    renderGroupedScheduledList(sessions);
    return;
  }
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    const labels = {
      all: 'No sessions yet',
      starred: 'No starred sessions',
      archived: 'No archived sessions',
      scheduled: 'No scheduled sessions',
      search: 'No matching sessions',
    };
    nav.innerHTML = `<p class="text-slate-500 text-sm px-3 py-2">${labels[filter]}</p>`;
    return;
  }
  // Always use grouped rendering for non-search tabs
  if (filter !== 'search') {
    renderGroupedSessionList(sessions, filter);
    return;
  }
  nav.innerHTML = sessions.map(s => renderSessionItem(s, filter)).join('');
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
}

// Inline rename
let renameSessionId = null;

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
  document.getElementById('cron-prompt').value = task.prompt || '';
  document.getElementById('cron-repo').value = task.repo || '';
  document.getElementById('cron-project').value = task.project || '';
  document.getElementById('cron-timezone').value = task.timezone || 'America/Los_Angeles';
  document.getElementById('cron-enabled').checked = task.enabled !== false;
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
  document.getElementById('cron-prompt').value = '';
  document.getElementById('cron-repo').value = '';
  document.getElementById('cron-project').value = '';
  document.getElementById('cron-timezone').value = 'America/Los_Angeles';
  document.getElementById('cron-enabled').checked = true;
  document.getElementById('cron-delete-btn').classList.add('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function closeCronModal() {
  document.getElementById('cron-modal').classList.add('hidden');
}

async function saveCronTask() {
  const name = document.getElementById('cron-name').value.trim();
  const cron = document.getElementById('cron-expr').value.trim();
  const prompt = document.getElementById('cron-prompt').value.trim();
  const repo = document.getElementById('cron-repo').value.trim() || null;
  const project = document.getElementById('cron-project').value.trim() || null;
  const timezone = document.getElementById('cron-timezone').value.trim();
  const enabled = document.getElementById('cron-enabled').checked;

  let res;
  try {
    if (cronEditMode === 'edit') {
      res = await fetch(`/api/cron/tasks/${encodeURIComponent(cronOriginalName)}`, {
        method: 'PUT',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({cron, prompt, repo, project, timezone, enabled}),
      });
    } else {
      res = await fetch('/api/cron/tasks', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({name, cron, prompt, repo, project, timezone, enabled}),
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
