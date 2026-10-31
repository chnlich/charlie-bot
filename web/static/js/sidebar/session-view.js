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
let triggerZoneFillCount = 0;
let lazySessionDataTimer = null;
function getDefaultBackendId() {
  return DEFAULT_BACKEND_ID;
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

// POST to switch this session's backend, then sync the header. On failure the
// server's `detail` is surfaced and the control reverts to the active value.
async function switchBackend(backendId) {
  if (!SESSION_ID || !backendId) return;
  const previousBadge = document.getElementById('backend-badge');
  try {
    const res = await fetch('/api/sessions/' + SESSION_ID + '/backend', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ backend: backendId }),
    });
    if (res.ok) {
      const data = await res.json();
      setActiveBackendId(data.backend);
      updateActiveBackendBadges();
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
}

function disposeActiveTurnEngine() {
  const container = document.getElementById('messages');
  const engine = Chat.activeTurnEngineFor(container);
  if (engine) engine.dispose();
}

// Runs on every path that leaves the active session view: the per-session
// pollers and turn engine must be down before the next view reuses the
// composer state cleared here. A package that holds per-view composer state
// drops it on the charliebot:session-view-teardown page event.
function teardownActiveSessionView() {
  stopActiveSessionViewPolling();
  stopTranscriptPolling();
  resetLazySessionData();
  disposeActiveTurnEngine();
  document.dispatchEvent(new CustomEvent('charliebot:session-view-teardown'));
  uploadedFiles = [];
  renderFileChips();
}

function scheduleLazySessionDataLoad() {
  if (!SESSION_ID || lazySessionDataTimer) return;
  lazySessionDataTimer = scheduleIdleTask(() => {
    lazySessionDataTimer = null;
    pollActiveSessionView({force: true});
    // A package that loads per-session data once per session load or switch
    // listens for this event. detail.isCurrent() tells, when its response lands,
    // whether the same session and switch generation still own the view.
    const sessionId = SESSION_ID;
    const generation = switchGeneration;
    document.dispatchEvent(new CustomEvent('charliebot:session-data-load', {detail: {
      sessionId,
      isCurrent: () => generation === switchGeneration && sessionId === SESSION_ID,
    }}));
  });
}

// ---------------------------------------------------------------------------
// Worker transcript mode (the live record in the main chat column)
// ---------------------------------------------------------------------------
// A task-tree worker node's messages are its Runs' transcript. They render
// through the main chat column and stay live through the transcript poll (the
// server's memoized projection answers an unchanged read with one stat pass).
// The poll carries thinking_since (the header timer), the signallable run (the
// stop control), and the streaming draft, so a worker page needs no other live
// channel.
const TRANSCRIPT_POLL_MS = 2000;
let transcriptMode = null;       // {sessionId} while the view shows one
let transcriptRevision = '';
let transcriptMessageCount = 0;
let transcriptInflight = false;
let transcriptActiveRunId = null;

function setWorkerTranscriptMode(mode) {
  transcriptMode = mode;         // {sessionId} for a worker node
  transcriptRevision = '';
  transcriptMessageCount = 0;
  transcriptActiveRunId = null;
  if (mode) startTranscriptPolling();
}

// usage.js (loaded first) paints the streaming node through this registration:
// a worker transcript's draft paints tool rows instead of parsed text.
Chat.setStreamDraftModeSupplier(transcriptTarget);

function setInputAreaVisible(visible) {
  const area = document.getElementById('input-area');
  if (area) area.classList.toggle('hidden', !visible);
}

function startTranscriptPolling() {
  if (!pageTimerRegistered('transcript-poll')) {
    startPageTimer('transcript-poll', pollTranscript, TRANSCRIPT_POLL_MS);
  }
}

function stopTranscriptPolling() {
  stopPageTimer('transcript-poll');
  transcriptMode = null;
  transcriptRevision = '';
  transcriptMessageCount = 0;
  transcriptActiveRunId = null;
}

function transcriptTarget() {
  return transcriptMode;
}

async function pollTranscript(opts) {
  const target = transcriptTarget();
  if (!target || transcriptInflight) return;
  if (SESSION_ID !== target.sessionId) return;
  if (!(opts && opts.force) && document.hidden) return;
  transcriptInflight = true;
  try {
    const params = new URLSearchParams({
      after: String(transcriptMessageCount),
      revision: transcriptRevision,
    });
    const res = await fetch('/api/sessions/' + target.sessionId + '/transcript?' + params.toString());
    if (!res.ok) throw new Error(res.status);
    const data = await res.json();
    if (SESSION_ID !== target.sessionId) return;
    applyTranscriptUpdate(data);
  } catch (err) {
    console.error('Transcript poll failed:', err);
  } finally {
    transcriptInflight = false;
  }
}

function applyTranscriptUpdate(data) {
  Chat.applyWorkerSummary(data.summary);
  const container = document.getElementById('messages');
  if (container) {
    if (data.reset) {
      // A reset re-renders the whole transcript (the committed prefix is no
      // longer current — a Run header's state moved, or the delivery close
      // landed). The full committed history is now on screen, so the chat
      // tail's scroll-up pagination stands down. The reader's place survives
      // the repaint: pinned readers stay at the bottom, one reading history
      // keeps their message at the same viewport offset.
      const engine = Chat.activeTurnEngineFor(container);
      // The follow decision is the engine's shouldFollow(): a fitting reply
      // opened at the bottom is not pinned. Its deferred follow must survive
      // the reset remount the same way the pin does.
      const wasPinned = engine ? engine.shouldFollow() : shouldAutoScroll(container);
      const reading = engine
        ? engine.captureReadingAnchor()
        : captureReadingPosition(container);
      sessionHasMore = false;
      sessionOlderBeforeCursor = Infinity;
      sessionLoadingMore = false;
      if (engine) {
        // No older page remains, so the sentinel leaves the page. A reset
        // invalidates the engine's whole projection; remount on the new
        // messages instead of repainting under a live engine.
        engine.holdWorkerSentinel(null);
        Chat.TurnEngine.mountIfAvailable(
            container, data.messages || [], SESSION_ID,
            {pinned: wasPinned, readingAnchor: wasPinned ? null : reading, view: engine.view});
      } else {
        renderMessagesIntoContainer(container, data.messages || [], SESSION_ID);
        if (wasPinned) {
          container.scrollTop = container.scrollHeight;
        } else {
          // A reader in history keeps their message's viewport offset; when
          // the reset removed that message the browser's own post-render
          // position is the closest surviving state.
          restoreReadingPosition(container, reading);
        }
      }
    } else if ((data.messages || []).length) {
      for (const msg of data.messages) appendMessageObject(msg);
    }
  }
  transcriptMessageCount = data.total != null ? data.total
    : transcriptMessageCount + (data.messages ? data.messages.length : 0);
  transcriptRevision = data.revision || transcriptRevision;
  transcriptActiveRunId = data.active_run_id || null;
  if (data.pending_draft && (data.pending_draft.content || data.pending_draft.thinking
      || (data.pending_draft.tools || []).length)) {
    showStreaming(data.pending_draft);
  } else {
    hideStreaming();
  }
  if (typeof data.thinking_since !== 'undefined') {
    const since = data.thinking_since;
    if (since && !THINKING_SINCE) {
      THINKING_SINCE = since;
      startThinking({keepSendEnabled: true});
    } else if (!since && THINKING_SINCE) {
      THINKING_SINCE = null;
      stopThinking({preserveSessionIndicator: true});
    } else if (since) {
      THINKING_SINCE = since;
    }
  }
}

// The stop control on a worker node stops the active Run through its own
// cancel route; the chat stop button (cancelMaster) dispatches here.
async function cancelTranscriptTarget() {
  const target = transcriptTarget();
  if (!target) return false;
  if (!transcriptActiveRunId) return true;  // nothing signallable: the state will clear on its own
  const url = '/api/sessions/' + target.sessionId + '/runs/' + transcriptActiveRunId + '/cancel';
  try {
    const res = await fetch(url, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({request_id: 'ui-stop-' + Date.now()}),
    });
    if (res.ok) return true;
    let detail = '';
    try { detail = (await res.json()).detail || ''; } catch (_err) { detail = ''; }
    showToast('Stop failed: ' + (detail || ('HTTP ' + res.status)), true);
    console.error('Stop run failed:', res.status, detail);
  } catch (err) {
    showToast('Stop failed: network error.', true);
    console.error('Stop run failed:', err);
  }
  return true;
}

// ---------------------------------------------------------------------------
// Active session view poll
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// Thinking indicator
// ---------------------------------------------------------------------------
// The thinking indicator owns only the header timer and the session-view poll:
// it never touches the send button. The button's disabled state has one writer
// — the in-flight lock (refreshSendLock in file-upload.js) — so a master run
// leaves typing available no matter how long it runs. The keepSendEnabled
// option stays in the call convention every start site passes; nothing reads
// it any more.
function startThinking(opts) {
  masterThinking = true;
  thinkingStart = thinkingStart || Date.now();
  document.getElementById('thinking').classList.remove('hidden');
  updateThinkingTime();
  startPageTimer('thinking-tick', updateThinkingTime, 1000);
  globalThis.ensureActiveSessionViewPolling();
}

// Resumes the indicator when a page load or SPA switch lands while the master
// is mid-thought. THINKING_SINCE is the server-stamped start that the session
// view and status polls refresh. The send button stays with the in-flight
// lock, so typing remains available while the run is still processing.
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
  // No button write here: master_done or assistant_error can land inside an
  // upload or input-work window, and re-enabling would punch through the in-flight
  // lock. Only refreshSendLock writes the button.
  stopActiveSessionViewPolling();
  if (!switching && !(opts && opts.preserveSessionIndicator)) Sidebar.updateSpinner();
}

function updateThinkingTime() {
  if (!thinkingStart) return;
  const secs = Math.floor((Date.now() - thinkingStart) / 1000);
  document.getElementById('thinking-time').textContent = secs + 's';
}

async function postChatCancel(sessionId) {
  try {
    const res = await fetch(`/api/chat/${sessionId}/cancel`, { method: 'POST' });
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

async function cancelMaster() {
  // A worker node's stop button signals the active Run through its own
  // cancel route (session-view.js), not the chat cancel.
  if (typeof transcriptTarget === 'function' && transcriptTarget()) {
    return cancelTranscriptTarget();
  }
  const target = composerTargetSession();
  if (!target.id) return;
  // Stop acts on the session the page displays at press; a switch that has
  // not landed holds it, and a click that moves on drops it with a note.
  return bindComposerAction(target.id, 'interrupt',
      () => postChatCancel(target.id),
      () => showToast('Stop dropped: the view moved on before the session loaded', true));
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

// ---------------------------------------------------------------------------
// Switch phases + bootstrap cache. Five one-line phase records per switch
// (started/completed/superseded/failed/render_error), plus a short-lived cache
// so a superseded generation's fetched bootstrap serves an immediate re-click
// of the same session.
// ---------------------------------------------------------------------------
const BOOTSTRAP_CACHE_TTL_MS = 60000;
const BOOTSTRAP_CACHE_MAX_ENTRIES = 20;
const bootstrapCache = new Map();

function cacheBootstrap(sessionId, data) {
  const now = Date.now();
  for (const [key, entry] of bootstrapCache) {
    if (now - entry.timestamp > BOOTSTRAP_CACHE_TTL_MS) bootstrapCache.delete(key);
  }
  while (bootstrapCache.size >= BOOTSTRAP_CACHE_MAX_ENTRIES) {
    bootstrapCache.delete(bootstrapCache.keys().next().value);
  }
  bootstrapCache.set(sessionId, {data, timestamp: now});
}

function freshCachedBootstrap(sessionId) {
  const entry = bootstrapCache.get(sessionId);
  if (!entry || Date.now() - entry.timestamp > BOOTSTRAP_CACHE_TTL_MS) return null;
  return entry.data;
}

// Each phase record goes out as a page event, so a diagnostics package may
// record it. A listener's failure stays inside dispatchEvent and never breaks a switch.
function reportSwitchEvent(phase, fromSessionId, sessionId, generation, extra = {}) {
  const record = {
    phase,
    from_session: fromSessionId,
    to_session: sessionId,
    generation,
    winner_generation: null,
    elapsed_ms: 0,
    error: null,
    client_ts: new Date().toISOString(),
    ...extra,
  };
  document.dispatchEvent(new CustomEvent('charliebot:session-switch-phase', {detail: record}));
}

function switchTargetName(sessionId) {
  const row = document.getElementById('session-' + sessionId);
  const nameEl = row ? row.querySelector('.session-name') : null;
  return (nameEl && nameEl.textContent) || sessionId;
}

// Click-time immediate feedback: the placeholder row (thinking pulse-dot
// style) replaces the stale session DOM before any network wait. The
// displayed-session module paints the header at the click, and
// renderSessionView repaints both with authoritative data later.
// The streaming element survives the wipe —
// hideStreaming/showStreaming resolve it by id after the render.
function showSwitchPlaceholder(sessionId) {
  const container = document.getElementById('messages');
  if (!container) return;
  const streamEl = document.getElementById('streaming-msg');
  container.innerHTML = '';
  if (streamEl) container.appendChild(streamEl);
  const row = document.createElement('div');
  row.id = 'switch-placeholder';
  row.className = 'px-4 py-2 flex items-center gap-2 text-sm text-slate-400';
  const dot = document.createElement('span');
  dot.className = 'w-2 h-2 rounded-full bg-blue-400 animate-pulse-dot';
  const label = document.createElement('span');
  label.textContent = 'Opening ' + switchTargetName(sessionId) + '…';
  row.appendChild(dot);
  row.appendChild(label);
  container.appendChild(row);
}

async function switchSession(sessionId) {
  // Welcome screen — no SPA state to swap, fall back to full load
  if (!SESSION_ID) { location.href = '/?session=' + sessionId; return; }
  // Already on this session
  if (sessionId === SESSION_ID) {
    return;
  }

  switching = true;
  const gen = ++switchGeneration;
  const fromSessionId = SESSION_ID;
  const switchStartedAt = Date.now();

  saveDraftNow();

  // Click-time feedback: the displayed session moves now — header name and tab
  // title — and the sidebar highlight follows below. Every click gets them,
  // even one later superseded; only the latest generation renders session
  // content. The move first resolves the composer actions the previous click
  // held, so a held message posts before this click swaps the composer.
  setDisplayedSession(sessionId, switchTargetName(sessionId));

  // The composer moves to the target's draft at once. Typing during the
  // switch window stores into the target's draft, and the landing leaves the
  // composer as the user left it.
  DRAFT_KEY = 'charliebot-draft-' + sessionId;
  const targetDraft = localStorage.getItem(DRAFT_KEY);
  const draftInput = document.getElementById('msg-input');
  if (draftInput) { draftInput.value = targetDraft || ''; autoResize(draftInput); }

  if (masterThinking) stopThinking();
  teardownActiveSessionView();
  document.getElementById('text-modal-overlay')?.setAttribute('style', 'display:none');

  // Close WebSocket (suppress auto-reconnect)
  disconnectWS();
  cancelReconnect();

  // Reset streaming state; request ids stay pending until their echoes arrive.
  hideStreaming();

  // Click-time feedback: placeholder over the stale DOM, header name and
  // sidebar highlight move now — every click gets them, even one later
  // superseded; only the latest generation renders session content.
  showSwitchPlaceholder(sessionId);
  updateSidebarHighlight(sessionId);
  reportSwitchEvent('started', fromSessionId, sessionId, gen);

  // A fresh cache hit skips the round-trip; staleness self-heals because
  // event_count below seeds the WebSocket replay cursor.
  let data = freshCachedBootstrap(sessionId);
  if (!data) {
    try {
      const res = await fetch('/api/sessions/' + sessionId + '/bootstrap');
      if (!res.ok) throw new Error(res.status);
      data = await res.json();
    } catch (err) {
      console.error('switchSession bootstrap fetch failed:', err);
      reportSwitchEvent('failed', fromSessionId, sessionId, gen, {
        error: String(err?.message || err),
        elapsed_ms: Date.now() - switchStartedAt,
      });
      // A newer click owns the view and its held actions: a failure of a
      // superseded switch navigates nowhere.
      if (gen !== switchGeneration) return;
      // The page navigates to a full load of the target. A composer action
      // held by this switch resolves first: a held message posts to the
      // session it bound before the navigation drops the page.
      abandonHeldComposerActions(sessionId);
      switching = false;
      location.href = '/?session=' + sessionId;
      return;
    }
  }

  // Discard stale response from rapid clicks
  if (gen !== switchGeneration) {
    // Keep the fetched bootstrap so an immediate re-click skips the fetch.
    cacheBootstrap(sessionId, data);
    reportSwitchEvent('superseded', fromSessionId, sessionId, gen, {
      winner_generation: switchGeneration,
      elapsed_ms: Date.now() - switchStartedAt,
    });
    return;  // newer switch owns the flag
  }

  // Update globals
  SESSION_ID = sessionId;
  document.dispatchEvent(new CustomEvent('charliebot:active-session-changed'));
  THINKING_SINCE = data.session.thinking_since || null;
  eventCursor = data.event_count;

  // Update URL
  history.pushState({session: sessionId}, '', '/?session=' + sessionId);

  // Render content
  try {
    globalThis.renderSessionView(data);
  } catch (err) {
    reportSwitchEvent('render_error', fromSessionId, sessionId, gen, {
      error: String(err?.message || err),
      elapsed_ms: Date.now() - switchStartedAt,
    });
    throw err;
  }

  // Mark switched-to session as read (WS was closed so broadcast is lost);
  // stamped through the shared seam so any in-flight older reply is refused.
  // The server-side flag clears only now, on the winning generation's landed
  // render — superseded generations and render errors returned above.
  recordUnreadFact(sessionId, false);
  // The same paint clears the opened row's dot and every ancestor's subtree mark.
  refreshSessionIndicator(sessionId);
  markSessionRead(sessionId);

  // Reconnect WebSocket
  reconnectDelay = 1000;
  connectWS();
  switching = false;
  reportSwitchEvent('completed', fromSessionId, sessionId, gen, {
    elapsed_ms: Date.now() - switchStartedAt,
  });

  // The switch landed: composer actions pressed inside the window run now,
  // against the freshly rendered view.
  runHeldComposerActions(sessionId);

  resumeThinkingIfMidThought();

  pollSessionStatus();
  ensureActiveSessionViewPolling();

  // A package that loads its data once per session listens for this event.
  document.dispatchEvent(new CustomEvent('charliebot:session-switched'));
  scheduleLazySessionDataLoad();
}

// The open paths' mount choice in one function: first load, session switch
// and new-session bootstrap all render through renderSessionView. An idle
// master session with a committed reply mounts anchored at that reply's top
// edge, so the reply's opening block is on the first screen. Every other
// open keeps the bottom-pinned default; null means default. THINKING_SINCE
// is the page global every open path sets from its payload before rendering.
function openMountOptions(session, data, messages) {
  if (session.profile === 'worker') return null;
  if (THINKING_SINCE) return null;
  if (data.pending_draft && (data.pending_draft.content || data.pending_draft.thinking)) return null;
  const lastAssistant = messages.filter((msg) => msg.role === 'assistant').pop();
  if (!lastAssistant) return null;
  return {pinned: false, readingAnchor: {kind: 'message', id: lastAssistant.id, offset: 0}};
}

// A worker leaf's closing line in its own chat projection: the close event
// projects to the system message "Task <outcome>: <summary>", and the leaf's
// delivery banner shows that summary.
function renderSessionView(data) {
  const session = data.session;
  const messages = data.messages || [];
  setActiveBackendId(data.active_backend);
  setSwitchableBackends(data.switchable_backends);
  setActiveRoundRatings(session.round_ratings || {});
  const backendType = data.active_backend_type || (BACKEND_TYPES ? BACKEND_TYPES[data.active_backend] : '') || '';
  updateBackendHeaderControls(backendType);

  // For-you rules and the summary card need this open's session before the
  // engine mounts: the worker's default view and its card depend on it. Only
  // a worker's bootstrap carries worker_summary.
  Chat.ForYou.setSession(session);
  Chat.mountWorkerSummary(data.worker_summary || null);
  const isWorker = session.profile === 'worker';

  // Store the raw-event cursor from the tail-loaded response.
  sessionHasMore = !!data.has_more;
  sessionOlderBeforeCursor = data.oldest_message_ordinal;
  sessionLoadingMore = false;
  suppressScrollLoad = false;

  // Update header — the displayed-session module owns the header name and the
  // tab title; a render refreshes the name only.
  refreshDisplayedSessionName(session.id, session.name);

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
  // The For you view opens at the bottom, where its newest message is: the
  // reply an idle open anchors on is one For you does not show. A held Status
  // choice is an overlay, so the engine mounts with the session's message view.
  const view = Chat.ForYou.viewFor(session);
  const engineView = Chat.ForYou.messageViewFor(session);
  const mountOptions = engineView === Chat.ForYou.FOR_YOU ? null : openMountOptions(session, data, messages);
  const turnEngine = globalThis.Chat && Chat.TurnEngine
    ? Chat.TurnEngine.mountIfAvailable(container, messages, session.id, Object.assign({view: engineView}, mountOptions))
    : null;
  if (!turnEngine) renderMessagesIntoContainer(container, messages, session.id);
  Chat.ForYou.renderToggle(turnEngine ? view : Chat.ForYou.EVERYTHING);

  if (sessionHasMore) {
    // The mount's own projection may have started the first page already.
    const sentinel = ensureSentinel(container, sessionLoadingMore ? 'loading' : 'idle');
    // A worker page's engine keeps its sentinel off the For-you list.
    if (turnEngine && isWorker) turnEngine.holdWorkerSentinel(sentinel);
  }

  // A worker node renders its Run transcript through the main chat column
  // (this file's transcript poll keeps it live) and takes no message input.
  // Set the transcript mode before the streaming paint: the draft's paint
  // renders worker tool rows when the mode is on.
  setWorkerTranscriptMode(isWorker ? {sessionId: session.id} : null);
  setInputAreaVisible(!isWorker);

  // Initialize streaming preview from pending draft (in-progress assistant
  // response carried over from a tail-loaded session). A worker transcript
  // streams tool activity only, so a draft carrying buffered tools shows even
  // without text — the paint renders those rows.
  if (data.pending_draft && (data.pending_draft.content || data.pending_draft.thinking
      || (isWorker && (data.pending_draft.tools || []).length))) {
    showStreaming(data.pending_draft);
  } else {
    hideStreaming();
  }

  // Scroll to bottom — the engine positions itself at mount (bottom pin, or
  // reading anchor); only the legacy render still needs this write.
  if (!turnEngine) container.scrollTop = container.scrollHeight;

  // The pending-triggers tray opens for this session (a worker session's
  // hidden input area does not hide it). The tray owns its endpoint fetch;
  // the status poll drives refetches.
  globalThis.renderPendingTriggersTray(session.id);

  // The status panel opens for this session: a web session's status, its history's
  // web_status messages, and the descendant rows from one GET.
  globalThis.openSessionStatusPanel(session, messages);

  // Restore whichever tab was active before the session switch
  switchTab(tabAfterSessionSwitch());

  // A tail page shorter than the viewport leaves the container unscrollable, so
  // no scroll event ever fires and the idle sentinel would wait forever: run the
  // fill check. The turn engine runs it after each of its projections too, which
  // covers a container that is hidden now and shown later.
  fillIfShort(container, false);
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

// The fill rule. Load the next older page when all of these hold. Older history
// remains, no page is in flight, the list is visible (clientHeight > 0), and its
// content is no taller than its viewport plus 80px. Such a list cannot scroll,
// so no scroll event will ever ask for the page. The sizes tell it, not
// scrollTop: a freshly mounted engine has not written its bottom pin yet.
// The turn engine calls this after every projection, so the two flags are read
// before any layout size. Returns the load's promise, or null when the rule
// does not hold.
function fillIfShort(container, isContinuation) {
  if (!sessionHasMore || sessionLoadingMore) return null;
  const viewport = container.clientHeight;
  if (viewport === 0 || container.scrollHeight > viewport + 80) return null;
  return loadOlderIfNeeded(container, isContinuation);
}

Chat.registerProjectionCheck((container) => { fillIfShort(container, false); });

// `isContinuation` marks a call that continues a chain. Any other call (a user
// gesture, a click on the failed sentinel, a fresh fill) starts a burst and
// resets the trigger-zone counter.
async function loadOlderIfNeeded(container, isContinuation) {
  if (!isContinuation) triggerZoneFillCount = 0;
  const engine = Chat.activeTurnEngineFor(container);
  if (engine && engine.view === 'foryou' && Chat.ForYou.isWorker()) return;
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

  // In-flight token: the landing paths below apply only while this session
  // and switch generation still own the view.
  const flight = {sessionId: SESSION_ID, generation: switchGeneration};
  sessionLoadingMore = flight;
  ensureSentinel(container, 'loading');
  const url = '/api/sessions/' + SESSION_ID + '/events?before=' + sessionOlderBeforeCursor + '&limit=40';
  const abortCtrl = new AbortController();
  const timeout = setTimeout(() => abortCtrl.abort(), 10000);
  let pageLanded = false;
  const stale = () => flight.generation !== switchGeneration || flight.sessionId !== SESSION_ID;
  try {
    const res = await fetch(url, {signal: abortCtrl.signal});
    clearTimeout(timeout);
    if (!res.ok) throw new Error(res.status);
    const data = await res.json();
    if (!Number.isFinite(data.next_before)) throw new Error('events page missing next_before');

    // A session switch superseded this flight: drop the page before any state
    // or DOM write (including the auto-continue chain below).
    if (stale()) return;

    sessionHasMore = !!data.has_more;

    const prevBeforeCursor = sessionOlderBeforeCursor;
    sessionOlderBeforeCursor = data.next_before;

    // If server says has_more but we made no progress, stop to avoid infinite loop
    if (sessionHasMore && sessionOlderBeforeCursor === prevBeforeCursor) {
      sessionHasMore = false;
    }

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
        if (Chat.ForYou.isWorker()) engine.holdWorkerSentinel(null);
        const removedHeight = heightBeforeSentinelRemoval - container.scrollHeight;
        if (removedHeight) {
          engine.writeScrollTop(Math.max(0, container.scrollTop - removedHeight));
        }
      }
      // A page that merges entirely into the leading pending segment shifts
      // nothing. A viewport parked at scrollTop 0 then stays there, where no
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
    // Stale flight: the new session's view owns the sentinel — do not paint
    // this session's failure onto it.
    if (stale()) return;
    console.error('loadOlderMessages failed:', err);
    // Failure must NOT clear sessionHasMore — only a server has_more:false
    // or the no-progress guard may do that. Switch sentinel to failed state
    // so the user can retry the same cursor.
    if (sessionHasMore) {
      ensureSentinel(container, 'failed');
    }
  } finally {
    if (sessionLoadingMore === flight) sessionLoadingMore = false;
  }

  // Auto-continue: only after a page actually lands. A failed page ends the
  // chain: the failed sentinel offers a retry by click, and the engine's next
  // projection of a short list asks the fill check again. Two cases:
  // - fill: the list is visible but too short to scroll, so no scroll event will
  //   ever fire; the fill check loads the next page. No count bounds this chain:
  //   each page moves the cursor strictly back, so it ends when the server
  //   reports has_more false or the cursor stops moving.
  // - trigger zone: the list scrolls but the viewport is still parked within
  //   80px of the top — e.g. pages that merged into the leading pending segment
  //   never moved the position; keep fetching until the position leaves the zone
  //   (at most 30 consecutive). A hidden list takes neither case.
  if (pageLanded && sessionHasMore) {
    const fill = fillIfShort(container, true);
    if (fill) {
      await fill;
    } else if (container.clientHeight > 0 && container.scrollTop <= 80) {
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

  const contextTokens = usage.context_tokens;
  const contextFull = usage.context_full;
  const contextCompactAt = usage.context_compact_at;
  const hasContext = contextTokens != null && contextFull != null;
  const hasCost = usage.total_cost_usd != null;

  // The header shows readings only. A record with no reading at all keeps the
  // indicator hidden, and a missing reading hides its own cell instead of an
  // empty meter or "unknown".
  if (indicator) indicator.classList.toggle('hidden', !hasContext && !hasCost);

  const bar = document.getElementById('usage-bar');
  const compactLine = document.getElementById('usage-compact-line');
  // The meter track is the bar's parent element, so hiding that parent hides
  // the track and its fill together.
  const meter = bar ? bar.parentElement : null;
  if (meter) meter.classList.toggle('hidden', !hasContext);
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
        // No line: fall back to the default thresholds (50% / 80% of full).
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
    // A hidden cell still holds the reading the compaction confirmation names.
    text.classList.toggle('hidden', !hasContext);
    text.textContent = hasContext
      ? formatTokens(contextTokens) + ' / ' + formatTokens(contextFull)
      : 'unknown';
  }
  const cost = document.getElementById('usage-cost');
  if (cost) {
    cost.classList.toggle('hidden', !hasCost);
    cost.textContent = formatUsageCostValue(usage.total_cost_usd);
  }
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------
// Both creation entry points (the New Session button and a row's hover "+")
// send the task creation body. It carries a client request key, the parent id
// (null for a root), always the manager profile, and an empty goal. A replay
// under the same parent returns the same node. The server names the node from
// its session counter, as it names a session.
function newTaskRequestId() {
  if (globalThis.crypto && typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  return 'ui-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 10);
}

function taskCreateBody(taskParentId, backend, group) {
  return {
    request_id: newTaskRequestId(),
    task_parent_id: taskParentId,
    profile: 'manager',
    task: {goal: ''},
    backend,
    // JSON.stringify drops an undefined value: a create without a group sends
    // the exact body it always has.
    group,
  };
}

async function createSession() {
  await createTaskNode(null);
}

async function createChildSession(parentId) {
  await createTaskNode(parentId);
}

// One-click create from a named group's header: the new session is born into
// that group (the server writes group at creation time) and opens like New
// Session does.
async function createSessionInGroup(group) {
  await createTaskNode(null, group);
}

// The one create-session POST both entry points make. A scheduled task's born-
// bound node and a plain task node carry the same body and headers, so the
// request shape is stated once here. Returns the created session's payload.
async function createSessionNode(taskParentId, group) {
  const backendSel = document.getElementById('new-session-backend');
  const backend = backendSel ? backendSel.value : undefined;
  const res = await fetch('/api/sessions/', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(taskCreateBody(taskParentId, backend, group)),
  });
  if (!res.ok) throw new Error(`Create session failed: ${res.status}`);
  return res.json();
}

async function createTaskNode(taskParentId, group) {
  try {
    const data = await createSessionNode(taskParentId, group);
    // The new session must be visible when the list repaints: open its group.
    if (group && typeof Sidebar.expandSessionGroup === 'function') Sidebar.expandSessionGroup(group);
    // The new child must be visible when the list repaints: open its parent.
    if (taskParentId && typeof Sidebar.expandTreeNode === 'function') Sidebar.expandTreeNode(taskParentId);
    if (!SESSION_ID) {
      location.href = '/?session=' + data.id;
      return;
    }

    switching = true;
    ++switchGeneration;
    saveDraftNow();
    // The new session becomes the displayed one at creation time. The
    // composer moves to its (empty) draft now, so the landing leaves the
    // composer as the user left it.
    setDisplayedSession(data.id, data.name);
    DRAFT_KEY = 'charliebot-draft-' + data.id;
    const draftInput = document.getElementById('msg-input');
    if (draftInput) { draftInput.value = ''; autoResize(draftInput); }
    if (masterThinking) stopThinking();
    teardownActiveSessionView();
    disconnectWS();
    cancelReconnect();
    hideStreaming();

    const bootstrap = buildEmptySessionBootstrap(data);
    SESSION_ID = data.id;
    document.dispatchEvent(new CustomEvent('charliebot:active-session-changed'));
    THINKING_SINCE = data.thinking_since || null;
    eventCursor = bootstrap.event_count;
    history.pushState({session: data.id}, '', '/?session=' + data.id);
    globalThis.renderSessionView(bootstrap);
    reconnectDelay = 1000;
    connectWS();
    switching = false;

    const input = document.getElementById('msg-input');
    if (input) input.focus();
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
  cancelReconnect();

  SESSION_ID = null;
  globalThis.renderPendingTriggersTray(null);
  globalThis.clearSessionStatusPanel();
  document.dispatchEvent(new CustomEvent('charliebot:active-session-changed'));
  // Back to the welcome page: the module restores the original tab title.
  setDisplayedSession(null, null);
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

// The session rows and group headers that groups.js renders.
registerSidebarControl('switchSession', switchSession);
registerSidebarControl('createChildSession', createChildSession);
registerSidebarControl('createSessionInGroup', createSessionInGroup);

const API = {
  getDefaultBackendId,
  getActiveBackendId,
  setActiveBackendId,
  setSwitchableBackends,
  updateActiveBackendBadges,
  scheduleLazySessionDataLoad,
  switchSession,
  renderSessionView,
  openMountOptions,
  setWorkerTranscriptMode,
  setInputAreaVisible,
  transcriptTarget,
  cancelTranscriptTarget,
  initScrollPagination,
  renderUsageFromData,
  createSession,
  createChildSession,
  createSessionInGroup,
  renderNoActiveSessionView,
  stopActiveSessionViewPolling,
  ensureActiveSessionViewPolling,
  pollActiveSessionView,
  startThinking,
  resumeThinkingIfMidThought,
  stopThinking,
  updateThinkingTime,
  cancelMaster,
};
Sidebar.wire(API, {
  switchBackend,
  loadOlderIfNeeded,
  createSessionNode,
});

})();
