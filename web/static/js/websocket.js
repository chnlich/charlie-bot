// ---------------------------------------------------------------------------
// WebSocket streaming
// ---------------------------------------------------------------------------
// Wire format: the server feeds raw chat events through MessageAggregator and
// broadcasts `message` and `stream` deltas in addition to non-aggregated
// events. A `message_update` delta replaces a committed message by its id.
// Rendering is fully driven by deltas; raw assistant/user/
// scheduled_trigger events are not sent. Other raw events (master_done,
// task_delegated, …) flow but only for state side-effects (stopThinking,
// spinner, etc), which the handlers of websocket-handlers.js apply.
let ws = null;
// Whether the current socket has opened. Only an open socket's close is a disconnect the
// client-socket telemetry announces: a reconnect attempt that never opened belongs to the
// disconnect already announced.
let wsOpen = false;
let reconnectDelay = 1000;
let reconnectTimer = null;
// The connect attempts since the last successful open or the last page-driven
// close: the client-socket telemetry's reconnect_count, so an episode reports
// how many attempts its reconnect took.
let reconnectAttempts = 0;
// Set when the socket closed for a reason the page did not start. The next open takes it, because the frames sent
// during the outage are not replayed.
let networkLossPending = false;
// Set when the page may have missed a sidebar frame: a socket that had opened closed (a switch closes and reopens the
// socket too), or the page had no session socket. The socket replays no task_tree_changed frame, so the next open
// resyncs the session list (websocket-handlers.js).
let sessionListResyncPending = false;
// The quick retry: a visible page retries a closed socket at a short fixed delay instead of waiting out
// a backoff it cannot observe the link through. A reset blackout surfaces a socket's close only when the
// link returns, and an idle page has no poll to prove the link returned, so this retry starts the
// recovery; from there the browser's connection setup decides when the handshake leaves. A hidden page
// keeps the exponential backoff — becoming visible reconnects it at once.
const RECONNECT_QUICK_RETRY_MS = 250;
// A server that is down refuses each attempt at once, so the quick retry is capped: after these attempts
// the exponential backoff owns the reconnect until a socket opens again.
const RECONNECT_QUICK_RETRY_LIMIT = 5;
let quickRetryTimer = null;
let quickRetriesLeft = RECONNECT_QUICK_RETRY_LIMIT;
// Request ids identify the local optimistic sends whose server echoes are due.
const pendingUserRequestIds = new Set();
let wsGeneration = 0;

// Event type -> {handler, advancesCursor}. A package registers the handlers of
// its own event types when its script loads, and websocket-handlers.js
// registers core's. A handler runs for an event of this session after the
// event cursor advances. A handler registered with {advancesCursor: false}
// runs before that point, and its event leaves the cursor where it is.
const wsMessageHandlers = new Map();

function registerWsMessageHandler(type, handler, options) {
  const advancesCursor = options?.advancesCursor;
  if (typeof type !== 'string' || !type || typeof handler !== 'function' || typeof advancesCursor !== 'boolean') {
    throw new Error('Invalid WebSocket message handler registration');
  }
  if (wsMessageHandlers.has(type)) throw new Error('Duplicate WebSocket message handler: ' + type);
  wsMessageHandlers.set(type, {handler, advancesCursor});
}

// Handlers that run on every (re)connect of this session's socket, after it
// opens, so a package re-syncs state derived from the event stream.
const wsConnectHandlers = [];

function registerWsConnectHandler(handler) {
  if (typeof handler !== 'function') throw new Error('Invalid WebSocket connect handler registration');
  if (wsConnectHandlers.includes(handler)) throw new Error('Duplicate WebSocket connect handler');
  wsConnectHandlers.push(handler);
}

function isStaleSocket(socket, targetSession, generation) {
  return socket !== ws || generation !== wsGeneration || targetSession !== SESSION_ID;
}

// Teardown paths detach all four handlers before close, so a closing socket
// cannot fire events into the session that replaced it. The onerror leg keeps
// its handlers attached: its onclose drives reconnect backoff.
function detachSocketHandlers(socket) {
  socket.onopen = null;
  socket.onmessage = null;
  socket.onclose = null;
  socket.onerror = null;
}

function disconnectWS() {
  wsGeneration++;
  const socket = ws;
  ws = null;
  // Each session numbers its drafts from zero, so the accumulated draft belongs to the page only until it
  // leaves this session. A reconnect through the backoff keeps it; the catch-up resends the whole draft.
  if (typeof resetStreamDraft === 'function') resetStreamDraft();
  // The page closed its own open socket (a switch, a session open or the welcome
  // view): the client-socket telemetry separates these closes from network
  // ones, and the readback counts only the network episodes.
  if (wsOpen) {
    reconnectAttempts = 0;
    // A socket that was receiving frames may have received a sidebar frame since its last event, and none is
    // replayed: the list reloads at the next open.
    sessionListResyncPending = true;
    document.dispatchEvent(new CustomEvent('charliebot:client-socket-close', {detail: {cause: 'switch', code: null}}));
  }
  wsOpen = false;
  if (!socket) return;
  detachSocketHandlers(socket);
  try { socket.close(); } catch {}
}

// Session teardown and connectWS re-entry call this so a pending backoff
// timer cannot fire connectWS into the session that was replaced.
function cancelReconnect() {
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  if (quickRetryTimer) { clearTimeout(quickRetryTimer); quickRetryTimer = null; }
}

// A successful request of any kind proves the link returned. A socket waiting
// out its reconnect backoff reconnects now instead, and its backoff resets:
// after a blackout the page's first answered request restores the socket
// without waiting the delay out. A switch window holds no timer — its teardown
// paths cancel the reconnect — so the poke never fires into the session a
// switch is replacing.
function noteRequestSuccess() {
  if (!reconnectTimer) return;
  cancelReconnect();
  reconnectDelay = 1000;
  connectWS();
}

// A closed socket follows this retry, not the backoff. The exponential timer stays scheduled as the
// hidden page's path and as the backstop should the quick retry never run; whichever fires first
// cancels the other through connectWS.
function scheduleQuickRetry() {
  if (quickRetryTimer || quickRetriesLeft <= 0 || document.visibilityState === 'hidden') return;
  quickRetriesLeft--;
  quickRetryTimer = setTimeout(() => {
    quickRetryTimer = null;
    connectWS();
  }, RECONNECT_QUICK_RETRY_MS);
}

// Every fetch the page makes reports its success here, whatever endpoint issued
// it. The page always defines fetch; the capability check keeps the vm harness
// contexts that run this file without one loadable.
const pageFetch = globalThis.fetch;
if (typeof pageFetch === 'function') {
  globalThis.fetch = (...args) => pageFetch(...args).then((response) => {
    if (response.ok) noteRequestSuccess();
    return response;
  });
}

function connectWS() {
  if (!SESSION_ID) {
    // Without a session there is no socket, so no sidebar frame reaches the page: the list the welcome view shows
    // misses every row change until the first session opens, and that open resyncs it.
    sessionListResyncPending = true;
    return;
  }
  cancelReconnect();
  // A socket that had opened can die without a close event (a phone backgrounding does this), and it took the frames
  // it received with it: the socket this call opens resyncs the list too.
  if (wsOpen) sessionListResyncPending = true;
  reconnectAttempts++;
  const targetSession = SESSION_ID;
  const generation = ++wsGeneration;
  const wsUrl = wsUrlWithToken(
      `/ws/sessions/${SESSION_ID}?cursor=${encodeURIComponent(eventCursor)}` +
      `&asset_version=${encodeURIComponent(STATIC_ASSET_VERSION)}`);
  const socket = new WebSocket(wsUrl);
  ws = socket;
  wsOpen = false;
  // The replay's bytes: everything this socket receives from its open until
  // the catchup_complete frame ends the replay, UTF-8 encoded.
  const replayBytes = new TextEncoder();
  let catchupBytes = 0;
  let catchupDone = false;

  socket.onopen = () => {
    if (isStaleSocket(socket, targetSession, generation)) {
      detachSocketHandlers(socket);
      try { socket.close(); } catch {}
      return;
    }
    console.log('WS connected');
    wsOpen = true;
    reconnectDelay = 1000;
    quickRetriesLeft = RECONNECT_QUICK_RETRY_LIMIT;
    document.dispatchEvent(new CustomEvent('charliebot:client-socket-open', {detail: {attempts: reconnectAttempts}}));
    reconnectAttempts = 0;
    // A snapshot: the connect handlers read the same value whether this one or
    // websocket-handlers.js's runs first, and only that one clears the flag.
    const afterNetworkLoss = networkLossPending;
    wsConnectHandlers.forEach((handler) => handler({afterNetworkLoss}));
  };

  socket.onmessage = (e) => {
    if (isStaleSocket(socket, targetSession, generation)) return;
    let data;
    try { data = JSON.parse(e.data); } catch { return; }
    // The replay's frames and their end reach last-state.js, which decides what the page holds as current.
    if (!catchupDone) {
      if (data && data.type === 'catchup_complete') {
        catchupDone = true;
        document.dispatchEvent(new CustomEvent('charliebot:client-socket-catchup', {
          detail: {bytes: catchupBytes, eventTotal: data.event_total},
        }));
      } else {
        const frameBytes = replayBytes.encode(e.data).length;
        catchupBytes += frameBytes;
        document.dispatchEvent(new CustomEvent('charliebot:client-socket-replay', {detail: {bytes: frameBytes}}));
      }
    }
    handleWSEvent(data, targetSession, generation);
  };

  socket.onclose = (event) => {
    if (isStaleSocket(socket, targetSession, generation)) return;
    if (wsOpen) {
      document.dispatchEvent(new CustomEvent('charliebot:client-socket-close', {
        detail: {cause: 'network', code: event ? event.code : null},
      }));
    }
    wsOpen = false;
    // 4409: the server's static asset version moved past this page's, so the tab kept running
    // pre-deploy JavaScript. Reload instead of reconnecting with the old code.
    if (event && event.code === 4409) {
      console.log('WS closed: this page is older than the server, reloading');
      handleStalePageClose();
      return;
    }
    console.log('WS closed, reconnecting in', reconnectDelay, 'ms');
    networkLossPending = true;
    sessionListResyncPending = true;
    reconnectTimer = setTimeout(connectWS, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 30000);
    scheduleQuickRetry();
  };

  socket.onerror = () => { if (!isStaleSocket(socket, targetSession, generation)) socket.close(); };
}

// ---------------------------------------------------------------------------
// Stale-page reload guard
// ---------------------------------------------------------------------------
// The 4409 close asks this page to reload, and no-store page responses mean the reload gets the
// current code. The guard caps the automatic reload at one per tab per 60 seconds. A layer
// that still serves old HTML (a stale cache in the path) therefore cannot turn the close
// into a reload loop.
const STALE_PAGE_RELOAD_KEY = 'charliebot-stale-page-reload-at';
const STALE_PAGE_RELOAD_GUARD_MS = 60000;

// A hidden tab reloads once it is visible.
function handleStalePageClose() {
  deferWhileHidden('stale-page-reload', reloadStalePage);
}

function reloadStalePage() {
  const now = Date.now();
  const lastReload = Number(sessionStorage.getItem(STALE_PAGE_RELOAD_KEY));
  if (lastReload && now - lastReload < STALE_PAGE_RELOAD_GUARD_MS) {
    showStalePageBanner();
    return;
  }
  sessionStorage.setItem(STALE_PAGE_RELOAD_KEY, String(now));
  // The page-load telemetry names this reload's cause on the other side.
  document.dispatchEvent(new CustomEvent('charliebot:page-load-cause', {detail: {cause: 'stale_asset'}}));
  location.reload();
}

function showStalePageBanner() {
  const banner = document.createElement('div');
  banner.style.cssText =
    'position:fixed;top:0;left:0;right:0;z-index:10000;display:flex;align-items:center;' +
    'justify-content:center;gap:16px;padding:10px;background:#991b1b;color:#fecaca;font-size:14px;';
  banner.textContent = 'This page is out of date. Reload to continue.';
  const button = document.createElement('button');
  button.style.cssText =
    'padding:4px 12px;border-radius:6px;border:none;background:#fca5a5;color:#7f1d1d;' +
    'font-size:13px;font-weight:600;cursor:pointer;';
  button.textContent = 'Reload';
  button.onclick = () => location.reload();
  banner.appendChild(button);
  document.body.appendChild(banner);
}

function _bumpEventCursor(ev) {
  // Track the highest event_index we've observed so reconnection asks the
  // server for events strictly after it. The index lives on the top level
  // for raw events and inside `message` for aggregator deltas.
  let idx = null;
  if (typeof ev.event_index === 'number') idx = ev.event_index;
  else if (ev.message && typeof ev.message.event_index === 'number') idx = ev.message.event_index;
  if (idx !== null) {
    eventCursor = Math.max(eventCursor, idx + 1);
  }
}

function handleWSEvent(ev, socketSessionId, socketGeneration) {
  if (socketSessionId !== SESSION_ID || socketGeneration !== wsGeneration) return;
  const t = ev.type;

  if (t === 'catchup_complete') {
    // Still part of the wire protocol; carries no client state any more.
    return;
  }
  if (t === 'ping') return;

  const registered = wsMessageHandlers.get(t);
  if (registered && !registered.advancesCursor) {
    registered.handler(ev);
    return;
  }

  _bumpEventCursor(ev);

  if (registered) registered.handler(ev);
}
