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
  // The page closed its own open socket (a switch, a session open or the welcome
  // view): the client-socket telemetry separates these closes from network
  // ones, and the readback counts only the network episodes.
  if (wsOpen) {
    reconnectAttempts = 0;
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
}

function connectWS() {
  if (!SESSION_ID) return;
  cancelReconnect();
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
    document.dispatchEvent(new CustomEvent('charliebot:client-socket-open', {detail: {attempts: reconnectAttempts}}));
    reconnectAttempts = 0;
    wsConnectHandlers.forEach((handler) => handler());
  };

  socket.onmessage = (e) => {
    if (isStaleSocket(socket, targetSession, generation)) return;
    let data;
    try { data = JSON.parse(e.data); } catch { return; }
    if (!catchupDone) {
      if (data && data.type === 'catchup_complete') {
        catchupDone = true;
        document.dispatchEvent(new CustomEvent('charliebot:client-socket-catchup', {detail: {bytes: catchupBytes}}));
      } else {
        catchupBytes += replayBytes.encode(e.data).length;
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
    reconnectTimer = setTimeout(connectWS, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 30000);
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

function handleStalePageClose() {
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
