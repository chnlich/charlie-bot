// ---------------------------------------------------------------------------
// Abortable view requests
// ---------------------------------------------------------------------------
// The session view's GETs — the switch bootstrap, the message pagination, the
// per-session polls, and the packages' per-session fetches — take their abort
// signal here. A session switch aborts every request still in flight for the
// view it leaves, superseded bootstraps included: over HTTP/2 the browser then
// sends RST_STREAM and the server stops writing, so on a slow, stalling link a
// switch waits only for its own bytes. A fetch that starts after an abort
// takes the fresh controller created here on demand.
let viewController = new AbortController();

function viewRequestSignal() {
  if (viewController.signal.aborted) viewController = new AbortController();
  return viewController.signal;
}

function abortViewRequests() {
  viewController.abort();
  viewController = new AbortController();
}

// ---------------------------------------------------------------------------
// Stored copies and the offline fact (contract with the offline shell's service worker, task 2)
// ---------------------------------------------------------------------------
// A stored copy is a first-screen or sidebar-list answer the service worker served from the browser's storage.
// The page tells a copy from a network answer by the wire alone:
// - A 200 answer from a copy carries `X-CharlieBot-Stored-At: <RFC 3339 time it was stored>`. A network answer
//   carries no such header.
// - A request with `X-CharlieBot-Network-Only: 1` must reach the network: the worker answers it with no copy and
//   without its wait limit. The page sends it when it discards a copy and for the sidebar list refresh that proves
//   the page caught up (sidebar/last-state.js).
// - While the page is offline and holds no copy, the worker lets the request fail (fetch rejects). The page reads a
//   rejected first-screen request made while offline as no local copy.
const STORED_AT_HEADER = 'X-CharlieBot-Stored-At';
const NETWORK_ONLY_HEADER = 'X-CharlieBot-Network-Only';

function isStoredResponse(response) {
  return response.headers.get(STORED_AT_HEADER) !== null;
}

function networkOnlyHeaders() {
  return {[NETWORK_ONLY_HEADER]: '1'};
}

function pageIsOffline() {
  return navigator.onLine === false;
}
