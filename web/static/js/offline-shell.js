// ---------------------------------------------------------------------------
// Offline shell: the page's side of the service worker (offline-shell-worker.js)
// ---------------------------------------------------------------------------
// The page registers the worker once its catch-up completes and asks it to store the shell then, refreshes
// the stored shell before each reload that replaces its code, reports the catch-up's end to the worker, and
// drops the stored copies on a 401. A browser without service workers keeps the page it always had.

const OFFLINE_SHELL_WORKER_PATH = '/offline-shell-worker.js';

function offlineShellSupported() {
  return 'serviceWorker' in navigator;
}

// Whether a worker controls this page: page_load reports it as `offline_shell`.
function offlineShellPresent() {
  return offlineShellSupported() && navigator.serviceWorker.controller !== null;
}

// The warm's outcome, one event per page load: the worker's reply once it has stored this release's
// shell and files ({stored: true}, {stored: false} for a 401, or {stored: false, error} for another failure),
// or {supported: false} when this browser has no service worker to ask. The poor-network bench waits for it
// before it closes a warm browser, so a shell the warm was storing survives the close.
function reportWarm(detail) {
  document.dispatchEvent(new CustomEvent('charliebot:offline-shell-warm', {detail}));
}

function registerOfflineShell() {
  if (!offlineShellSupported()) {
    reportWarm({supported: false});
    return;
  }
  // The page's open owns the link until its catch-up completes: the worker script this registration
  // fetches and the shell warm-up are the offline shell's own bytes, so both wait for the page-current
  // event (sidebar/last-state.js). A worker registered then controls the next navigation in its scope,
  // which keeps its first open's paint the page's own.
  document.addEventListener('charliebot:page-current', warmOfflineShell, {once: true});
}

function warmOfflineShell() {
  navigator.serviceWorker.register(OFFLINE_SHELL_WORKER_PATH, {scope: '/?session='})
    .catch((err) => console.error('offline shell worker registration failed:', err));
  navigator.serviceWorker.ready.then((registration) => {
    if (!registration.active) {
      reportWarm({supported: false});
      return;
    }
    const channel = new MessageChannel();
    channel.port1.onmessage = (event) => reportWarm(event.data ?? {});
    registration.active.postMessage({type: 'warm', copies: offlineShellSeedCopies()}, [channel.port2]);
  }, (err) => reportWarm({error: String(err)}));
}

// The sidebar's listing URL for an active session. sidebar/filters.js registers it at load, after this file loads,
// so this file names only its own entries.
let offlineShellListingUrl;

function registerOfflineShellListingUrl(listingUrlOf) {
  offlineShellListingUrl = listingUrlOf;
}

// Server-rendered data did not pass through the worker's fetch handler, so seed the same URLs from the page that
// painted them. The shell's first-screen payload and initial sidebar rows are the offline copies for its next open.
function offlineShellSeedCopies() {
  const copies = [];
  if (SESSION_ID && SESSION_BOOTSTRAP) {
    copies.push({
      url: new URL('/api/sessions/' + SESSION_ID + '/bootstrap', location.href).href,
      body: SESSION_BOOTSTRAP,
    });
  }
  if (INITIAL_SESSIONS !== null && INITIAL_LOAD_ERRORS.length === 0) {
    const activeSessionId = SESSION_ID || new URL(location.href).searchParams.get('session');
    copies.push({
      url: new URL(offlineShellListingUrl(activeSessionId), location.href).href,
      body: INITIAL_SESSIONS,
    });
  }
  return copies;
}

// The page reports its catch-up complete (sidebar/last-state.js): no request of the open is on the link
// any more, so the worker refreshes the copies this open read from (offline-shell-worker.js).
function reportCatchupComplete() {
  withOfflineShellWorker((worker) => worker.postMessage({type: 'catchup-complete'}));
}

document.addEventListener('charliebot:page-current', () => reportCatchupComplete());

// Runs `action` with the worker that holds this page, when one is registered and active.
function withOfflineShellWorker(action) {
  if (!offlineShellSupported()) return Promise.resolve();
  return navigator.serviceWorker.getRegistration().then((registration) => {
    if (!registration || !registration.active) return undefined;
    return action(registration.active);
  });
}

// The stored copies go on a 401: the key rotated or was revoked, so no copy may answer any longer. config.js
// announces every 401 on the window.
function dropOfflineShellCopies() {
  withOfflineShellWorker((worker) => worker.postMessage({type: 'drop-copies'}));
}

window.addEventListener('charliebot:unauthorized', () => dropOfflineShellCopies());

function reportShellRefreshFailure(error) {
  console.error('offline shell refresh failed:', error);
  document.dispatchEvent(new CustomEvent('charliebot:offline-shell-refresh-failed', {
    detail: {error: String(error)},
  }));
}

// A reload that replaces the page's code (websocket.js, utils.js). The worker refreshes the stored shell first, so the
// navigation answers with the release's shell (landing requirement B). `beforeNavigate` runs once the refresh has
// answered and returns false to cancel the reload. The page-load cause mark is written just before the navigation,
// so the mark names this reload and not one that never started. The promise settles when the reload navigated, was
// cancelled, or failed to refresh.
function reloadAfterRefreshingShell(cause, beforeNavigate = () => true) {
  const navigate = () => {
    if (!beforeNavigate()) return;
    document.dispatchEvent(new CustomEvent('charliebot:page-load-cause', {detail: {cause}}));
    location.reload();
  };
  if (!offlineShellSupported()) {
    navigate();
    return Promise.resolve();
  }
  return withOfflineShellWorker((worker) => new Promise((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = (event) => resolve(event.data ?? {});
    worker.postMessage({type: 'warm', refresh: true}, [channel.port2]);
  })).then((result) => {
    if (result === undefined || result.stored || result.unauthorized) {
      navigate();
      return;
    }
    if (result.error) {
      reportShellRefreshFailure(new Error(result.error));
      return;
    }
    throw new Error('offline shell refresh returned no result');
  }).catch(reportShellRefreshFailure);
}
