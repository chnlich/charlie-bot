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
// link returns. An idle page has no poll to prove the link returned, so this retry starts the recovery.
// From there, the browser's connection setup decides when the handshake leaves. A hidden page keeps the
// exponential backoff — becoming visible reconnects it at once.
const RECONNECT_QUICK_RETRY_MS = 250;
// A server that is down refuses each attempt at once, so the quick retry is capped. After these
// attempts, the exponential backoff owns the reconnect until a socket opens again.
const RECONNECT_QUICK_RETRY_LIMIT = 5;
let quickRetryTimer = null;
let quickRetriesLeft = RECONNECT_QUICK_RETRY_LIMIT;
// A close with code 1000 is a server stop, and a release's restart is ready a few seconds later. A visible page then
// retries at this fixed interval until a socket opens. The window bounds it: after this long without a connection the
// page falls back to the quick retry and the backoff, so a stalled switch or a rollback does not hold every visible
// tab at two attempts a second. The window's end is 0 when no 1000 close is waiting on a connection.
const RECONNECT_RESTART_RETRY_MS = 500;
const RECONNECT_RESTART_WINDOW_MS = 60000;
let restartWindowEnd = 0;
// A visible page replaces a connect attempt that has not opened within this many milliseconds with a fresh one. The
// kernel retransmits a lost SYN on a backoff that can run past the link's return by seconds; a fresh attempt sends its
// first SYN at once. The timer belongs to the attempt in progress, and the attempt's open, close or teardown clears it.
const CONNECT_DEADLINE_MS = 4000;
let connectDeadlineTimer = null;
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
// The subscription whose frames the session view accepts. It is null while the view belongs to no
// subscription: between a switch's click and its `subscribed` frame, and after a subscribe error.
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
// handler registered with {sidebar: true} serves a sidebar frame: one the
// server broadcasts on the shared sidebar channel, naming its own session. It
// runs whatever subscription the view holds, so the list stays live across a
// switch's frame drop.
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

// A switch drops the frames of the session it leaves from its click. The click wipes the message
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
// during the switch window — its pending reconnect handshakes for the displayed session. That
// handshake binds subscription 0 once the wait is released.
function resumeViewSubscription() {
  switchWaitSession = null;
  if (boundSession === SESSION_ID && ws && wsOpen) {
    viewSubscription = boundSubscription;
  }
}

// Request the target session's subscription from the bootstrap's event position. `report` carries
// the switch context the failure notice reports. A subscribe outside a switch (a new task node's
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
  else if (!ws) connectWS();
  // Otherwise the socket still in its handshake sends it once open. A page the offline shell served
  // opens its socket here: its boot ran with no session to name, so nothing connected one before the
  // open path queued this subscribe.
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

function clearConnectDeadline() {
  if (connectDeadlineTimer) { clearTimeout(connectDeadlineTimer); connectDeadlineTimer = null; }
}

// Close the page's socket and keep it closed. The no-session view calls this: with no session
// displayed there is nothing to render, so an archived session's live output would spend the link
// for nothing. The close is the page's own — it announces nothing and schedules no reconnect — and
// the next session's open loads the page, whose first socket carries the server's snapshot.
function closeSessionSocket() {
  cancelReconnect();
  clearConnectDeadline();
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
// out its reconnect backoff reconnects now instead, and its backoff resets.
// After a blackout, the page's first answered request restores the socket
// without waiting the delay out. A switch window holds its own timer only by
// chance. A poke during one reconnects for the session being left, and the
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
  if (quickRetryTimer || document.visibilityState === 'hidden') return;
  if (Date.now() < restartWindowEnd) {
    quickRetryTimer = setTimeout(() => {
      quickRetryTimer = null;
      connectWS();
    }, RECONNECT_RESTART_RETRY_MS);
    return;
  }
  if (quickRetriesLeft <= 0) return;
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
    // The page's first answer after a socket closed is its return point when the page was hidden (client-events.js).
    if (response.ok) {
      document.dispatchEvent(new CustomEvent('charliebot:page-answer'));
      noteRequestSuccess();
    }
    return response;
  });
}

function connectWS() {
  if (!SESSION_ID) {
    // Without a session there is no socket, so no sidebar frame reaches the page. The list the welcome
    // view shows misses every row change until the first session opens. That open resyncs it.
    sessionListResyncPending = true;
    return;
  }
  cancelReconnect();
  // A socket that had opened can die without a close event (a phone backgrounding does this), and it
  // took the frames it received with it. The socket this call opens resyncs the list too.
  if (wsOpen) sessionListResyncPending = true;
  reconnectAttempts++;
  const targetSession = SESSION_ID;
  const generation = ++wsGeneration;
  const wsUrl = wsUrlWithToken(
      `/ws/sessions/${SESSION_ID}?cursor=${encodeURIComponent(eventCursor)}` +
      `&protocol=${encodeURIComponent(WEB_PROTOCOL)}`);
  const socket = new WebSocket(wsUrl);
  ws = socket;
  wsOpen = false;
  // The attempt this one replaces is cancelled with its deadline. A hidden page sets no deadline for this attempt.
  clearConnectDeadline();
  if (document.visibilityState !== 'hidden') {
    connectDeadlineTimer = setTimeout(() => {
      connectDeadlineTimer = null;
      detachSocketHandlers(socket);
      try { socket.close(); } catch {}
      connectWS();
    }, CONNECT_DEADLINE_MS);
  }
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
    clearConnectDeadline();
    console.log('WS connected');
    wsOpen = true;
    reconnectDelay = 1000;
    quickRetriesLeft = RECONNECT_QUICK_RETRY_LIMIT;
    restartWindowEnd = 0;
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
    const frameBytes = replayBytesEncoder.encode(e.data).length;
    if (!replayDone) {
      replayBytes += frameBytes;
    }
    // The view accepts only frames of the subscription it took. A switch drops every frame of the
    // session it left from its click until its `subscribed` frame binds the target. A socket whose
    // binding session is not the displayed one has nothing this view reads. The sidebar's frames name
    // their own session and keep flowing. The list they paint therefore stays live across the switch,
    // the way it does across the socket a network close leaves behind.
    const registered = wsMessageHandlers.get(type);
    if ((viewSubscription === null || boundSession !== SESSION_ID) && !(registered && registered.sidebar)) return;
    if (!replayDone) {
      document.dispatchEvent(new CustomEvent('charliebot:client-socket-replay', {detail: {bytes: frameBytes}}));
    }
    handleWSEvent(data);
  };

  socket.onclose = (event) => {
    if (isStaleSocket(socket, generation)) return;
    clearConnectDeadline();
    if (wsOpen) {
      document.dispatchEvent(new CustomEvent('charliebot:client-socket-close', {
        detail: {code: event ? event.code : null},
      }));
    }
    wsOpen = false;
    // 4409: the server's web protocol moved past this page's, so the tab runs JavaScript the server no longer
    // answers. Reload instead of reconnecting with the old code.
    if (event && event.code === 4409) {
      console.log('WS closed: this page speaks an older web protocol, reloading');
      handleStalePageClose();
      return;
    }
    if (event && event.code === 1000 && Date.now() >= restartWindowEnd) {
      restartWindowEnd = Date.now() + RECONNECT_RESTART_WINDOW_MS;
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
      uptimeMs: data.uptime_ms,
    },
  }));
  // The server runs another release than this page's code. A hidden page swaps to it now; a visible one keeps its code
  // and swaps at its next hide.
  if (data.asset_version !== STATIC_ASSET_VERSION) {
    releaseCodePending = true;
    reloadToCurrentCode();
  }
}

// ---------------------------------------------------------------------------
// Code reloads: the 4409 close and the hidden swap
// ---------------------------------------------------------------------------
// Two things replace this page's code. A 4409 close means the server's web protocol moved past the page's, so the
// page reloads whatever its visibility. A catch-up that names another release swaps the code of a hidden page only: a
// visible page keeps its code until it hides. Both refresh the stored shell first, then navigate (offline-shell.js).
// The reload guard caps the automatic reloads at one per tab per 60 seconds, so a stale cache cannot turn them into a
// loop. A visible tab the guard holds shows the banner. A hidden tab waits out the window and reloads if still hidden.
const STALE_PAGE_RELOAD_KEY = 'charliebot-stale-page-reload-at';
const STALE_PAGE_RELOAD_GUARD_MS = 60000;
let protocolStale = false;
let releaseCodePending = false;
let swapInFlight = false;
let guardTimer = null;

function staleReloadWaitMs() {
  const lastReload = Number(sessionStorage.getItem(STALE_PAGE_RELOAD_KEY));
  if (!lastReload) return 0;
  return Math.max(0, lastReload + STALE_PAGE_RELOAD_GUARD_MS - Date.now());
}

function markStaleReload() {
  sessionStorage.setItem(STALE_PAGE_RELOAD_KEY, String(Date.now()));
}

function handleStalePageClose() {
  protocolStale = true;
  reloadToCurrentCode();
}

function reloadToCurrentCode() {
  if (protocolStale) {
    const wait = staleReloadWaitMs();
    if (wait > 0) {
      if (document.hidden) holdForGuard(wait);
      else showStalePageBanner();
      return;
    }
    // A protocol reload is granted now: the mark counts it, so a second close while the shell refreshes waits.
    markStaleReload();
    reloadAfterRefreshingShell('stale_asset');
    return;
  }
  if (!releaseCodePending || swapInFlight || !document.hidden) return;
  const wait = staleReloadWaitMs();
  if (wait > 0) {
    holdForGuard(wait);
    return;
  }
  swapInFlight = true;
  // A return before the navigation starts cancels the swap: the page keeps its code and the next hide tries again.
  // Only a navigation that starts counts against the guard.
  reloadAfterRefreshingShell('stale_asset', () => {
    if (!document.hidden) return false;
    markStaleReload();
    return true;
  }).finally(() => { swapInFlight = false; });
}

// One timer for the guard's wait. It runs the reload decision again, so the decision is taken at the window's end.
function holdForGuard(wait) {
  if (guardTimer !== null) return;
  guardTimer = setTimeout(() => {
    guardTimer = null;
    reloadToCurrentCode();
  }, wait);
}

// A hidden page that is shown or hidden again re-decides its pending swap: a cancelled swap runs at the next hide.
document.addEventListener('visibilitychange', () => {
  if (document.hidden) reloadToCurrentCode();
});

document.addEventListener('charliebot:offline-shell-refresh-failed', showStalePageBanner);

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
  button.onclick = () => reloadAfterRefreshingShell('stale_asset');
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

  // The server folds a streamed draft's commit into the frame after it in the same event. The commit is the assistant
  // message the page streamed, so the page rebuilds its role and streamed-content flag, and hands it to the message
  // handler before the frame's own handler runs.
  if (ev.commit) {
    handleWSEvent({
      type: 'message',
      message: Object.assign({}, ev.commit, {role: 'assistant', content_streamed: true}),
    });
  }

  const registered = wsMessageHandlers.get(t);
  if (registered && !registered.advancesCursor) {
    registered.handler(ev);
    return;
  }

  _bumpEventCursor(ev);

  if (registered) registered.handler(ev);
}
