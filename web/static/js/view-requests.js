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
