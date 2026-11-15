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
//
// One socket per tab: a session switch keeps the open socket and rides a
// `subscribe` frame — session_id, cursor, subscription — instead of closing
// and reconnecting. Only a network close reconnects, with a handshake for the
// current session. The server answers `subscribed` before replaying from the
// cursor and closes the replay with a `catchup_complete` that carries the
// session, the subscription number and the event total.
let ws = null;
// Whether the current socket has opened. Only an open socket's close is a disconnect the
// client-socket telemetry announces: a reconnect attempt that never opened belongs to the
// disconnect already announced.
let wsOpen = false;
let reconnectDelay = 1000;
let reconnectTimer = null;
// The connect attempts since the last successful open: the client-socket telemetry's
// reconnect_count, so an episode reports how many attempts its reconnect took.
let reconnectAttempts = 0;
// Set when the socket closed for a reason the page did not start. The next open takes it, because the frames sent
// during the outage are not replayed.
let networkLossPending = false;
// Set when the page may have missed a sidebar frame: a socket that had opened closed for a network reason, or the
// page had no session socket. The socket replays no task_tree_changed frame, so the next open resyncs the session
// list (websocket-handlers.js). The no-session view's own close sets nothing: the open that follows it is a page
// load, whose rows carry the server's snapshot.
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
// The socket's own connect counter: it names a socket stale the moment a newer connect attempt
// replaces it. The page's subscription counter below is a different numbering.
let wsGeneration = 0;
// The page's subscription counter: each subscribe request takes the next number, and the
// handshake's own replay is subscription 0 of its socket.
let pageSubscription = 0;
// The subscribe request awaiting the server's `subscribed` frame: {session_id, cursor,
// subscription, from_session, generation}. It rides the open socket, or the socket still in its
// handshake sends it once open.
let pendingSubscribe = null;
// The session a started switch is waiting to subscribe to, set at the switch's click and cleared
// when the view takes a subscription or the subscribe fails. Non-null while a switch sits between
// its click and its subscription.
let switchWaitSession = null;
// The session and subscription the socket is bound to: the handshake session with subscription 0,
// then the session and number of the last accepted `subscribed` frame.
let boundSession = null;
let boundSubscription = 0;
// The subscription whose frames the session view accepts, or null while the view belongs to no
// subscription — between a switch's click and its `subscribed` frame, and after a subscribe error.
let viewSubscription = null;
// The replay the socket is currently receiving: its subscription number, the bytes collected for
// it so far, and whether its `catchup_complete` has landed. A `subscribed` frame opens the next
// replay's bucket.
let replaySubscription = 0;
let replayBytes = 0;
let replayDone = false;

// Event type -> {handler, advancesCursor, sidebar}. A package registers the handlers of
// its own event types when its script loads, and websocket-handlers.js
// registers core's. A handler runs for an event of this session after the
// event cursor advances. A handler registered with {advancesCursor: false}
// runs before that point, and its event leaves the cursor where it is. A
// handler registered with {sidebar: true} serves a sidebar frame — one the
// server broadcasts on the shared sidebar channel, naming its own session —
// and runs whatever subscription the view holds, so the list stays live
// across a switch's frame drop.
const wsMessageHandlers = new Map();

function registerWsMessageHandler(type, handler, options) {
  const advancesCursor = options?.advancesCursor;
  if (typeof type !== 'string' || !type || typeof handler !== 'function' || typeof advancesCursor !== 'boolean') {
    throw new Error('Invalid WebSocket message handler registration');
  }
  if (wsMessageHandlers.has(type)) throw new Error('Duplicate WebSocket message handler: ' + type);
  wsMessageHandlers.set(type, {handler, advancesCursor, sidebar: options?.sidebar === true});
}

// Handlers that run on every (re)connect of this session's socket, after it
// opens, so a package re-syncs state derived from the event stream.
const wsConnectHandlers = [];

function registerWsConnectHandler(handler) {
  if (typeof handler !== 'function') throw new Error('Invalid WebSocket connect handler registration');
  if (wsConnectHandlers.includes(handler)) throw new Error('Duplicate WebSocket connect handler');
  wsConnectHandlers.push(handler);
}

function isStaleSocket(socket, generation) {
  return socket !== ws || generation !== wsGeneration;
}

// Teardown paths detach all four handlers before close, so a closing socket
// cannot fire events into the socket that replaced it. The onerror leg keeps
// its handlers attached: its onclose drives reconnect backoff.
function detachSocketHandlers(socket) {
  socket.onopen = null;
  socket.onmessage = null;
  socket.onclose = null;
  socket.onerror = null;
}

// Whether a click on the displayed session may re-request its subscription: only after a refused
// subscribe, when the view holds no subscription and no switch is waiting for one. A switch in
// flight owns the wait, and re-subscribing now would render this session's frames under its
// placeholder.
function canRetrySessionSubscription() {
  return viewSubscription === null && switchWaitSession === null;
}

// A switch drops the frames of the session it leaves from its click: the click wipes the message
// area and moves the displayed session, so a late stream or commit of the left session may not
// render into the target's view. The drop holds until the target's `subscribed` frame binds.
function holdSessionFrames(targetSessionId) {
  switchWaitSession = targetSessionId;
  viewSubscription = null;
}

// Bind the view to a session and subscription, and open the replay bucket that subscription's
// catch-up fills.
function bindSubscription(sessionId, subscription) {
  boundSession = sessionId;
  boundSubscription = subscription;
  viewSubscription = subscription;
  replaySubscription = subscription;
  replayBytes = 0;
  replayDone = false;
}

// A failed switch's wait ends with the view back on the session it started from: the socket's
// standing subscription serves it again. When no open socket holds that subscription — one closed
// during the switch window — its pending reconnect handshakes for the displayed session and binds
// subscription 0 once the wait is released.
function resumeViewSubscription() {
  switchWaitSession = null;
  if (boundSession === SESSION_ID && ws && wsOpen) {
    viewSubscription = boundSubscription;
  }
}

// Request the target session's subscription from the bootstrap's event position. `report` carries
// the switch context the failure notice reports; a subscribe outside a switch (a new task node's
// path reports none) passes null and its failure leaves the notice without a switch record.
function requestSessionSubscription(sessionId, cursor, report) {
  pageSubscription++;
  pendingSubscribe = {
    session_id: sessionId,
    cursor: cursor,
    subscription: pageSubscription,
    from_session: report ? report.from_session : null,
    generation: report ? report.generation : 0,
  };
  if (ws && wsOpen) sendPendingSubscribe();
  // Otherwise the socket still in its handshake sends it once open.
}

function sendPendingSubscribe() {
  const request = pendingSubscribe;
  ws.send(JSON.stringify({
    type: 'subscribe',
    session_id: request.session_id,
    cursor: request.cursor,
    subscription: request.subscription,
  }));
}

// Teardown paths cancel the reconnect timers so a pending one cannot fire connectWS into a
// state that no longer wants a socket.
function cancelReconnect() {
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  if (quickRetryTimer) { clearTimeout(quickRetryTimer); quickRetryTimer = null; }
}

// Close the page's socket and keep it closed. The no-session view calls this: with no session
// displayed there is nothing to render, so an archived session's live output would spend the link
// for nothing. The close is the page's own — it announces nothing and schedules no reconnect — and
// the next session's open loads the page, whose first socket carries the server's snapshot.
function closeSessionSocket() {
  cancelReconnect();
  wsGeneration++;
  const socket = ws;
  ws = null;
  wsOpen = false;
  pendingSubscribe = null;
  switchWaitSession = null;
  viewSubscription = null;
  boundSession = null;
  boundSubscription = 0;
  // Each session numbers its drafts from zero, so the accumulated draft belongs to the page only
  // until it leaves this session.
  if (typeof resetStreamDraft === 'function') resetStreamDraft();
  if (!socket) return;
  detachSocketHandlers(socket);
  try { socket.close(); } catch {}
}

// A successful request of any kind proves the link returned. A socket waiting
// out its reconnect backoff reconnects now instead, and its backoff resets:
// after a blackout the page's first answered request restores the socket
// without waiting the delay out. A switch window holds its own timer only by
// chance: a poke during one reconnects for the session being left, and the
// switch's subscribe takes that socket over once its bootstrap completes.
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
  const replayBytesEncoder = new TextEncoder();
  replayBytes = 0;
  replayDone = false;
  replaySubscription = 0;

  socket.onopen = () => {
    if (isStaleSocket(socket, generation)) {
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
    if (pendingSubscribe && pendingSubscribe.session_id !== targetSession) {
      // The reconnect handshook for the session the switch left: the switch's subscribe takes
      // this socket over once its bootstrap completed.
      sendPendingSubscribe();
      return;
    }
    if (pendingSubscribe) {
      // The handshake replays the pending subscribe's own session from the same cursor the
      // bootstrap seeded, so the request is already covered.
      pendingSubscribe = null;
      switchWaitSession = null;
    }
    if (targetSession === SESSION_ID && switchWaitSession === null) {
      bindSubscription(targetSession, 0);
    }
  };

  socket.onmessage = (e) => {
    if (isStaleSocket(socket, generation)) return;
    let data;
    try { data = JSON.parse(e.data); } catch { return; }
    const type = data && data.type;
    if (type === 'subscribed') {
      handleSubscribed(data);
      return;
    }
    if (type === 'error' && pendingSubscribe && data.subscription === pendingSubscribe.subscription) {
      // The subscribe request failed: the server kept its previous subscription, and the view
      // keeps dropping session frames until a retry or a reconnect binds one again.
      const request = pendingSubscribe;
      pendingSubscribe = null;
      switchWaitSession = null;
      sessionSubscribeFailed(request);
      return;
    }
    if (type === 'catchup_complete') {
      handleCatchupComplete(data);
      return;
    }
    // A replay's frames are counted into the subscription's byte total until its `catchup_complete`.
    if (!replayDone) {
      replayBytes += replayBytesEncoder.encode(e.data).length;
    }
    // The view accepts only frames of the subscription it took: a switch drops every frame of the
    // session it left from its click until its `subscribed` frame binds the target, and a socket
    // whose binding session is not the displayed one has nothing this view reads. The sidebar's
    // frames name their own session and keep flowing, so the list they paint stays live across the
    // switch the way it does across the socket a network close leaves behind.
    const registered = wsMessageHandlers.get(type);
    if ((viewSubscription === null || boundSession !== SESSION_ID) && !(registered && registered.sidebar)) return;
    handleWSEvent(data);
  };

  socket.onclose = (event) => {
    if (isStaleSocket(socket, generation)) return;
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

  socket.onerror = () => { if (!isStaleSocket(socket, generation)) socket.close(); };
}

// The server bound the socket to the requested session: bind the view to it and open the replay
// bucket the replay that follows fills. A `subscribed` frame for any other number belongs to a
// superseded request, whose replay the view drops with the rest of its frames.
function handleSubscribed(data) {
  if (!pendingSubscribe || data.subscription !== pendingSubscribe.subscription ||
      data.session_id !== pendingSubscribe.session_id) {
    return;
  }
  pendingSubscribe = null;
  switchWaitSession = null;
  bindSubscription(data.session_id, data.subscription);
}

// A replay's completion carries the session, the subscription number and the session's event
// total. Each subscription reports its own replay once; a frame for an already-counted or unknown
// subscription belongs to a superseded replay and reports nothing.
function handleCatchupComplete(data) {
  if (replayDone || data.subscription !== replaySubscription) return;
  replayDone = true;
  document.dispatchEvent(new CustomEvent('charliebot:client-socket-catchup', {
    detail: {
      subscription: data.subscription,
      session_id: data.session_id,
      bytes: replayBytes,
      eventTotal: data.event_total,
    },
  }));
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

function handleWSEvent(ev) {
  const t = ev.type;

  if (t === 'ping') return;

  const registered = wsMessageHandlers.get(t);
  if (registered && !registered.advancesCursor) {
    registered.handler(ev);
    return;
  }

  _bumpEventCursor(ev);

  if (registered) registered.handler(ev);
}
