(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// SPA-style session switching
// ---------------------------------------------------------------------------
let switchGeneration = 0;
// Pagination state for tail-loaded sessions
let sessionHasMore = false;
let sessionOlderBeforeCursor = Infinity;
let sessionLoadingMore = false;
let suppressScrollLoad = false;
let viewportFillCount = 0;
let triggerZoneFillCount = 0;
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

let switchableBackends = [];

function setSwitchableBackends(backendIds) {
  switchableBackends = Array.isArray(backendIds) ? backendIds : [];
}

// True when this session is a cron-dedicated PM session whose backend switch
// writes through to the task yaml and rotates to a fresh session.
let backendSwitchRotates = false;

function setBackendSwitchRotates(value) {
  backendSwitchRotates = !!value;
}

// POST to switch this session's backend, then sync the header. On a rotating
// (write-through) switch the current session is archived and the scheduled task
// continues in a fresh session, so confirm first and navigate to the new
// session on success. On failure the server's `detail` is surfaced and the
// control reverts to the active value.
async function switchBackend(backendId) {
  if (!SESSION_ID || !backendId) return;
  const previousBadge = document.getElementById('backend-badge');
  if (backendSwitchRotates) {
    const confirmed = confirm(
      'Switching the model archives this session and continues the scheduled task in a fresh session with the new model. Continue?');
    if (!confirmed) {
      rollbackBackendSelect(previousBadge);
      return;
    }
  }
  try {
    const res = await fetch('/api/sessions/' + SESSION_ID + '/backend', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ backend: backendId }),
    });
    if (res.ok) {
      const data = await res.json();
      if (data.id && data.id !== SESSION_ID) {
        setActiveBackendId(data.backend);
        await switchSession(data.id);
        updateSidebarHighlight(data.id);
        switchSidebarFilter(currentFilter);
      } else {
        setActiveBackendId(data.backend);
        updateActiveBackendBadges();
      }
    } else {
      let detail = '';
      try {
        const body = await res.json();
        detail = body.detail || '';
      } catch (_err) { detail = ''; }
      rollbackBackendSelect(previousBadge);
      if (detail) showToast(detail, true);
      console.error('switchBackend failed:', res.status, detail || res.statusText);
    }
  } catch (err) {
    rollbackBackendSelect(previousBadge);
    showToast('Backend switch failed: network error. Please try again.', true);
    console.error('switchBackend failed:', err);
  }
}

function rollbackBackendSelect(badge) {
  // Restore the dropdown to the currently-active backend without firing change.
  // `badge` is the '#backend-badge' container; the live <select> is nested inside it.
  const select = badge && badge.querySelector && badge.querySelector('select[data-backend-switch]');
  if (select) select.value = getActiveBackendId();
}

function updateActiveBackendBadges() {
  const activeBackendId = getActiveBackendId();
  const activeBackendLabel = BACKEND_OPTIONS[activeBackendId] || activeBackendId;

  const backendBadge = document.getElementById('backend-badge');
  if (backendBadge) {
    if (switchableBackends.length > 1 && SESSION_ID) {
      renderSwitchableBackendBadge(backendBadge, switchableBackends, activeBackendId);
    } else {
      backendBadge.textContent = activeBackendLabel;
    }
  }

  const inputModelBadge = document.getElementById('input-model-badge');
  if (inputModelBadge) inputModelBadge.textContent = activeBackendLabel;
}

function renderSwitchableBackendBadge(badge, backendIds, activeId) {
  const options = backendIds.map((id) => {
    const label = BACKEND_OPTIONS[id] || id;
    return '<option value="' + id + '"' + (id === activeId ? ' selected' : '') + '>' + label + '</option>';
  }).join('');
  badge.innerHTML = '<select data-backend-switch class="bg-slate-800 border border-slate-600 rounded text-slate-200 text-xs px-1.5 py-0.5">' + options + '</select>';
  const select = badge.querySelector('select[data-backend-switch]');
  if (select) {
    select.value = activeId;
    select.addEventListener('change', () => {
      if (select.value !== activeId) switchBackend(select.value);
    });
    select.addEventListener('click', (e) => e.stopPropagation());
  }
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
  stopPageTimer('workers-list');
  workersLoadedForSession = null;
  workersLoadInflightForSession = null;
  workersListEtag = null;
  if (typeof stopAllThreadPolls === 'function') stopAllThreadPolls();
  if (typeof loadedThreads !== 'undefined') loadedThreads.clear();
}

function disposeActiveTurnEngine() {
  const container = document.getElementById('messages');
  const engine = globalThis.Chat && Chat.TurnEngine && container
    ? Chat.TurnEngine.activeFor(container)
    : null;
  if (engine) engine.dispose();
}

// Runs on every path that leaves the active session view: the per-session
// pollers and turn engine must be down before the next view reuses the
// composer state cleared here.
function teardownActiveSessionView() {
  stopActiveSessionViewPolling();
  resetLazySessionData();
  disposeActiveTurnEngine();
  resetVoiceState();
  uploadedFiles = [];
  renderFileChips();
  hideSlashPopup();
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
    oldest_message_ordinal: 0,
    active_backend: backend,
    active_backend_type: BACKEND_TYPES ? (BACKEND_TYPES[backend] || '') : '',
    switchable_backends: [],
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

  if (masterThinking) stopThinking();
  teardownActiveSessionView();
  document.getElementById('text-modal-overlay')?.setAttribute('style', 'display:none');

  // Close WebSocket (suppress auto-reconnect)
  disconnectWS();
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }

  // Reset streaming state
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
  if (typeof planPanel !== 'undefined') planPanel.onActiveSessionChanged();
  DRAFT_KEY = 'charliebot-draft-' + sessionId;
  THINKING_SINCE = data.session.thinking_since || null;
  eventCursor = data.event_count;

  // Update URL
  history.pushState({session: sessionId}, '', '/?session=' + sessionId);

  // Render content
  globalThis.renderSessionView(data);

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
  setSwitchableBackends(data.switchable_backends);
  setBackendSwitchRotates(data.backend_switch_rotates);
  setActiveRoundRatings(session.round_ratings || {});
  const backendType = data.active_backend_type || (BACKEND_TYPES ? BACKEND_TYPES[data.active_backend] : '') || '';
  if (globalThis.TuiSession) {
    globalThis.TuiSession.syncBackend(backendType, session.id);
  } else {
    globalThis.ACTIVE_BACKEND_TYPE = backendType;
  }
  updateBackendHeaderControls(backendType, session.id);

  // Store the raw-event cursor from the tail-loaded response.
  sessionHasMore = !!data.has_more;
  sessionOlderBeforeCursor = data.oldest_message_ordinal;
  sessionLoadingMore = false;
  suppressScrollLoad = false;
  viewportFillCount = 0;

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

  // Build message HTML — the turn window engine takes over when the DOM
  // supports it; every session switch tears the previous engine down with it.
  const container = document.getElementById('messages');
  if (!container) return;
  const turnEngine = globalThis.Chat && Chat.TurnEngine
    ? Chat.TurnEngine.mountIfAvailable(container, messages, session.id)
    : null;
  if (!turnEngine) renderMessagesIntoContainer(container, messages, session.id);

  if (sessionHasMore) ensureSentinel(container, 'idle');

  // Initialize streaming preview from pending draft (in-progress assistant
  // response carried over from a tail-loaded session).
  if (data.pending_draft && (data.pending_draft.content || data.pending_draft.thinking)) {
    showStreaming(data.pending_draft);
  } else {
    hideStreaming();
  }

  // Scroll to bottom — the engine already pinned its projection at mount.
  if (!turnEngine) container.scrollTop = container.scrollHeight;

  if (Array.isArray(data.threads) || Array.isArray(data.triggers)) {
    renderWorkersTab(data.threads || [], session.id, data.triggers || []);
  } else {
    renderWorkersTabUnknown();
  }

  // Restore whichever tab was active before the session switch
  const activeBtn = document.querySelector('#btn-terminal.bg-blue-600\\/20, #btn-chat-tex.bg-blue-600\\/20, #btn-chat.bg-blue-600\\/20, #btn-workers.bg-blue-600\\/20, #btn-chat-backlog.bg-blue-600\\/20');
  const activeTab = activeBtn ? activeBtn.id.replace('btn-', '') : 'chat';
  switchTab(activeTab);

  // A tail page shorter than the viewport leaves the container unscrollable, so
  // no scroll event ever fires and the idle sentinel would wait forever. This
  // attempt returns immediately when the container is already scrollable.
  if (sessionHasMore) loadOlderIfNeeded(container);
}

// ---------------------------------------------------------------------------
// Scroll-to-top pagination (loads older messages)
// ---------------------------------------------------------------------------
function ensureSentinel(container, state) {
  let sentinel = document.getElementById('load-more-sentinel');
  if (!sentinel) {
    sentinel = document.createElement('div');
    sentinel.id = 'load-more-sentinel';
    sentinel.className = 'flex justify-center py-3 text-xs text-slate-500';
    container.prepend(sentinel);
  }
  sentinel.setAttribute('data-state', state);
  if (state === 'idle') {
    sentinel.innerHTML = 'Scroll up for older messages';
    sentinel.style.cursor = 'default';
    sentinel.onclick = null;
  } else if (state === 'loading') {
    sentinel.innerHTML = 'Loading older messages&hellip;';
    sentinel.style.cursor = 'default';
    sentinel.onclick = null;
  } else if (state === 'failed') {
    sentinel.innerHTML = 'Failed to load older messages &mdash; click to retry';
    sentinel.style.cursor = 'pointer';
    sentinel.onclick = () => loadOlderIfNeeded(container);
  }
  return sentinel;
}

function initScrollPagination() {
  const container = document.getElementById('messages');
  if (!container) return;
  let debounceTimer = null;
  container.addEventListener('scroll', () => {
    if (suppressScrollLoad) {
      suppressScrollLoad = false;
      if (debounceTimer) clearTimeout(debounceTimer);
      return;
    }
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => loadOlderIfNeeded(container), 150);
  });
}

async function loadOlderIfNeeded(container, isViewportFill) {
  if (!isViewportFill) {
    viewportFillCount = 0;
    triggerZoneFillCount = 0;
  }
  if (!sessionHasMore || sessionLoadingMore) return;
  // Trigger when within 80px of the top
  if (container.scrollTop > 80) return;
  // An unusable cursor can never produce a page. Surface it instead of returning
  // silently — a silent return renders as "the sentinel does nothing", which is
  // exactly how a stale backend's renamed cursor field stayed invisible.
  if (!Number.isFinite(sessionOlderBeforeCursor)) {
    console.error('loadOlderMessages: unusable pagination cursor', sessionOlderBeforeCursor);
    ensureSentinel(container, 'failed');
    return;
  }

  sessionLoadingMore = true;
  ensureSentinel(container, 'loading');
  const url = '/api/sessions/' + SESSION_ID + '/events?before=' + sessionOlderBeforeCursor + '&limit=40';
  const abortCtrl = new AbortController();
  const timeout = setTimeout(() => abortCtrl.abort(), 10000);
  let pageLanded = false;
  try {
    const res = await fetch(url, {signal: abortCtrl.signal});
    clearTimeout(timeout);
    if (!res.ok) throw new Error(res.status);
    const data = await res.json();
    if (!Number.isFinite(data.next_before)) throw new Error('events page missing next_before');

    sessionHasMore = !!data.has_more;

    const prevBeforeCursor = sessionOlderBeforeCursor;
    sessionOlderBeforeCursor = data.next_before;

    // If server says has_more but we made no progress, stop to avoid infinite loop
    if (sessionHasMore && sessionOlderBeforeCursor === prevBeforeCursor) {
      sessionHasMore = false;
    }

    const engine = globalThis.Chat && Chat.TurnEngine ? Chat.TurnEngine.activeFor(container) : null;

    const sentinel = document.getElementById('load-more-sentinel');

    if (engine) {
      // Incremental per-page ingestion: store prepend + boundary-span
      // derivation + height-estimate scroll adjustment, all engine-side. Keep
      // the pagination sentinel in place so its fixed top height cancels out
      // of the engine's scroll-anchor delta.
      const shift = engine.prependMessages(data.messages);
      if (!sessionHasMore && sentinel) {
        const heightBeforeSentinelRemoval = container.scrollHeight;
        sentinel.remove();
        const removedHeight = heightBeforeSentinelRemoval - container.scrollHeight;
        if (removedHeight) {
          engine.writeScrollTop(Math.max(0, container.scrollTop - removedHeight));
        }
      }
      // A page that merges entirely into the leading pending segment shifts
      // nothing, so a viewport parked at scrollTop 0 stays there — where no
      // scroll event can ever fire and pagination stalls. Lift by 1px so the
      // next wheel gesture always produces a scroll event.
      if (shift === 0 && sessionHasMore && container.scrollTop <= 0) {
        engine.writeScrollTop(1);
      }
    } else {
      // The legacy path builds at container top, so remove the sentinel while
      // inserting the detached message nodes.
      const prevHeight = container.scrollHeight;
      if (sentinel) sentinel.remove();

      // Build and prepend message elements that are not already present. Page
      // boundaries can re-emit a message whose aggregator id is already shown.
      const newMessages = data.messages.filter(msg => !isRenderedMessage(msg));
      const tempDiv = renderMessagesToDetachedContainer(newMessages, SESSION_ID);

      // Insert at top (sentinel was removed; recreate below if needed)
      while (tempDiv.lastChild) {
        container.prepend(tempDiv.lastChild);
      }

      applyTurnOutline(container);

      // Preserve scroll position — suppress the scroll event this dispatches
      suppressScrollLoad = true;
      container.scrollTop = container.scrollHeight - prevHeight;
      // Same 1px lift as the engine path: a page that only merged into the
      // leading segment restored scrollTop to <= 0, where no scroll event can
      // ever fire. Lift by 1px so the next wheel gesture keeps pagination alive.
      if (sessionHasMore && container.scrollTop <= 0) {
        container.scrollTop = 1;
      }
      setTimeout(() => { suppressScrollLoad = false; }, 0);
    }

    // Recreate sentinel if more pages remain
    if (sessionHasMore) {
      ensureSentinel(container, 'idle');
    } else if (!engine && sentinel) {
      sentinel.remove();
    }
    pageLanded = true;
  } catch (err) {
    clearTimeout(timeout);
    console.error('loadOlderMessages failed:', err);
    // Failure must NOT clear sessionHasMore — only a server has_more:false
    // or the no-progress guard may do that. Switch sentinel to failed state
    // so the user can retry the same cursor.
    if (sessionHasMore) {
      ensureSentinel(container, 'failed');
    }
  } finally {
    sessionLoadingMore = false;
  }

  // Bounded auto-continue: only after a page actually lands (not on failure —
  // the failed sentinel must wait for a user click, not auto-retry). Both
  // burst counters reset on a non-fill (user-gesture) call. Two cases:
  // - viewport fill: the container is not scrollable, so no scroll event will
  //   ever fire; fetch until it overflows (at most 5 consecutive).
  // - trigger zone: the container is scrollable but the viewport is still
  //   parked within 80px of the top — e.g. pages that merged into the leading
  //   pending segment never moved the position; keep fetching until the
  //   position leaves the zone (at most 30 consecutive).
  if (pageLanded && sessionHasMore) {
    if (container.scrollHeight <= container.clientHeight + 80) {
      if (viewportFillCount < 5) {
        viewportFillCount++;
        await loadOlderIfNeeded(container, true);
      }
    } else if (container.scrollTop <= 80) {
      if (triggerZoneFillCount < 30) {
        triggerZoneFillCount++;
        await loadOlderIfNeeded(container, true);
      }
    }
  }
}

function renderUsageFromData(usage) {
  const indicator = document.getElementById('usage-indicator');
  if (!usage) { if (indicator) indicator.classList.add('hidden'); return; }
  if (indicator) indicator.classList.remove('hidden');

  const contextTokens = usage.context_tokens;
  const contextFull = usage.context_full;
  const contextCompactAt = usage.context_compact_at;
  const hasContext = contextTokens != null && contextFull != null;

  const bar = document.getElementById('usage-bar');
  const compactLine = document.getElementById('usage-compact-line');
  if (bar) {
    if (hasContext) {
      const pct = contextFull > 0 ? (contextTokens / contextFull * 100) : 0;
      bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
      let color;
      if (contextCompactAt != null) {
        // Colour relative to the compaction line: below 50% of it blue,
        // 50%–100% yellow, past it red.
        if (contextTokens > contextCompactAt) color = 'bg-red-500';
        else if (contextTokens >= contextCompactAt / 2) color = 'bg-yellow-500';
        else color = 'bg-blue-500';
      } else {
        // No line: fall back to today's thresholds (50% / 80% of full).
        color = pct > 80 ? 'bg-red-500' : pct > 50 ? 'bg-yellow-500' : 'bg-blue-500';
      }
      bar.className = PROGRESS_BAR_FILL_CLASS + ' ' + color;
    } else {
      // No per-request source: hide the bar rather than draw a 0% fill.
      bar.style.width = '0%';
      bar.className = PROGRESS_BAR_FILL_CLASS + ' hidden';
    }
  }
  if (compactLine) {
    if (hasContext && contextCompactAt != null && contextFull > 0) {
      const linePct = Math.min(contextCompactAt / contextFull * 100, 100);
      compactLine.style.left = linePct.toFixed(1) + '%';
      compactLine.title = String(contextCompactAt);
      compactLine.className = 'absolute top-0 h-full w-0.5 bg-white';
    } else {
      compactLine.style.left = '0%';
      compactLine.title = '';
      compactLine.className = 'absolute top-0 h-full w-0.5 bg-white hidden';
    }
  }
  const text = document.getElementById('usage-text');
  if (text) {
    text.textContent = hasContext
      ? formatTokens(contextTokens) + ' / ' + formatTokens(contextFull)
      : 'unknown';
  }
  const cost = document.getElementById('usage-cost');
  if (cost) cost.textContent = formatUsageCostValue(usage.total_cost_usd);
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
      headers: JSON_HEADERS,
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
    teardownActiveSessionView();
    disconnectWS();
    if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
    pendingUserMsg = false;
    hideStreaming();

    const bootstrap = buildEmptySessionBootstrap(data);
    SESSION_ID = data.id;
    if (typeof planPanel !== 'undefined') planPanel.onActiveSessionChanged();
    DRAFT_KEY = 'charliebot-draft-' + data.id;
    THINKING_SINCE = data.thinking_since || null;
    eventCursor = bootstrap.event_count;
    history.pushState({session: data.id}, '', '/?session=' + data.id);
    globalThis.renderSessionView(bootstrap);
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

function renderNoActiveSessionView() {
  switching = true;
  ++switchGeneration;

  if (masterThinking) stopThinking({preserveSessionIndicator: true});
  teardownActiveSessionView();
  hideStreaming();
  disconnectWS();
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  pendingUserMsg = false;

  SESSION_ID = null;
  if (typeof planPanel !== 'undefined') planPanel.onActiveSessionChanged();
  DRAFT_KEY = null;
  THINKING_SINCE = null;
  eventCursor = 0;
  history.pushState({session: null}, '', '/');

  const main = document.querySelector('main');
  if (!main) throw new Error('Main element missing');
  const welcomeTpl = document.getElementById('welcome-view');
  if (!welcomeTpl) throw new Error('welcome-view template missing');
  main.innerHTML = welcomeTpl.innerHTML;
  switching = false;
}


// One name list: Object.assign puts each export on Sidebar, and the same keys
// become bare globals. Adding a function here is enough for both.
const API = {
  getDefaultBackendId,
  getActiveBackendId,
  setActiveBackendId,
  setSwitchableBackends,
  setBackendSwitchRotates,
  updateActiveBackendBadges,
  switchBackend,
  scheduleLazySessionDataLoad,
  switchSession,
  renderSessionView,
  initScrollPagination,
  loadOlderIfNeeded,
  renderUsageFromData,
  createSession,
  renderNoActiveSessionView,
};
Object.assign(Sidebar, API);
Sidebar.expose(Object.keys(API));

})();
