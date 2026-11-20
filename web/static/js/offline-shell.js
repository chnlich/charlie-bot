// ---------------------------------------------------------------------------
// Offline shell: the page's side of the service worker (offline-shell-worker.js)
// ---------------------------------------------------------------------------
// The page registers the worker once its catch-up completes and asks it to store the shell then, asks it to store
// the session page's on-demand files once the page is idle, refreshes the stored shell before a 4409 reload, reports
// the catch-up's end to the worker, and drops the stored copies on a 401. A browser without service workers keeps
// the page it always had.

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
  document.addEventListener('charliebot:page-current', requestOnDemandFiles, {once: true});
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
    registration.active.postMessage({type: 'warm', copies: offlineShellSeedCopies(), files: onDemandFileUrls()},
        [channel.port2]);
  }, (err) => reportWarm({error: String(err)}));
}

// ---------------------------------------------------------------------------
// On-demand files: the same-origin files a view or panel loads when it is first used
// ---------------------------------------------------------------------------
// Each file a loadPageScripts or loadPageStyles call names on the session page (utils.js), and each sidebar
// interaction file (sidebar/namespace.js). A view or panel opened offline needs its file stored, and the worker
// stores a file only when it answers one: so the worker stores this list once the page is current and idle, after
// the first screen and the catch-up have used the link. Cross-origin files stay out: the CDN loads are not the
// worker's to answer. offline_shell_page.test.js fails when a loader names a same-origin file this list lacks.
const ON_DEMAND_FILES = Object.freeze([
  '/static/js/chat/worker-summary.js',
  '/static/js/chat/triggers-tray.js',
  '/static/js/comment_post.js',
  '/static/js/chat/status-panel.js',
  '/static/js/chat/file-links.js',
  '/static/js/sidebar/archived.js',
  '/static/js/file-upload.js',
  '/static/features/drafts/draft-cards.js',
  '/static/features/artifacts/artifact-cards.js',
  '/static/features/artifacts/plan-panel.js',
  '/static/features/usage/ext_usage.js',
  '/static/features/explain/explain.js',
  '/static/features/cron/cron.js',
  '/static/features/terminal/terminal_b64.js',
  '/static/features/terminal/terminal_clipboard.js',
  '/static/features/terminal/terminal_mount.js',
  '/static/features/terminal/terminal_panel.js',
  '/static/features/voice/voice-input.js',
  '/static/js/sidebar/row-menu.js',
  '/static/js/sidebar/modals.js',
  '/static/js/sidebar/session-actions.js',
  '/static/js/sidebar/session-menus.js',
  '/static/js/sidebar/session-move.js',
  '/static/css/worker-summary.css',
  '/static/css/triggers-tray.css',
  '/static/css/file-upload.css',
  '/static/features/artifacts/artifacts.css',
  '/static/features/usage/usage.css',
  '/static/features/voice/voice-overlay.css',
  '/static/css/sidebar-interactions.css',
]);

// The versioned URLs the page requests for them (utils.js pageAssetUrl), so the worker's stored keys are the page's URLs.
function onDemandFileUrls() {
  return ON_DEMAND_FILES.map((path) => pageAssetUrl(path));
}

// The worker stores the list when the page is current and the browser is idle. The reply is one event per request,
// {stored: n} or {error}; the poor-network bench waits for it before a warm browser closes.
function requestOnDemandFiles() {
  requestIdleCallback(() => {
    navigator.serviceWorker.ready.then((registration) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = (event) => reportOnDemandFiles(event.data ?? {});
      registration.active.postMessage({type: 'store-files', files: onDemandFileUrls()}, [channel.port2]);
    }, (err) => reportOnDemandFiles({error: String(err)}));
  }, {timeout: 5000});
}

function reportOnDemandFiles(detail) {
  if (detail.error) console.error('offline shell on-demand files failed:', detail.error);
  document.dispatchEvent(new CustomEvent('charliebot:offline-shell-files-stored', {detail}));
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

// A reply the socket delivered leaves the session's stored copy behind: the copy holds the bootstrap the open
// read, and the reply reached the page after it. The page asks the worker to re-fetch that session's stored copy
// from the network while the link is up, so an offline reopen shows the newest reply the page received. A reply's
// frames land as a burst of commits (a streamed draft's commit, the round's separator), so the ask waits for a
// quiet gap and one fetch covers the burst. The page dispatches charliebot:offline-shell-copies-refreshed with
// the count of copies the worker re-stored once the refresh is done, so a waiter holds until the storage settled.
const STORED_COPY_REFRESH_DEBOUNCE_MS = 2000;
let storedCopyRefreshSessions = null;
let storedCopyRefreshTimer = null;

function refreshStoredSessionCopies(sessionId) {
  if (storedCopyRefreshSessions === null) storedCopyRefreshSessions = new Set();
  storedCopyRefreshSessions.add(sessionId);
  clearTimeout(storedCopyRefreshTimer);
  storedCopyRefreshTimer = setTimeout(() => {
    const sessions = storedCopyRefreshSessions;
    storedCopyRefreshSessions = null;
    const urls = [...sessions].map((id) => new URL('/api/sessions/' + id + '/bootstrap', location.href).href);
    withOfflineShellWorker((worker) => new Promise((resolve) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = (event) => resolve(event.data ?? {});
      worker.postMessage({type: 'refresh-copies', urls}, [channel.port2]);
    })).then((result) => {
      document.dispatchEvent(new CustomEvent('charliebot:offline-shell-copies-refreshed', {detail: result ?? {}}));
    }, (err) => console.error('stored copy refresh failed:', err));
  }, STORED_COPY_REFRESH_DEBOUNCE_MS);
}

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

// The reload after a 4409 close. A worker refreshes the shell and its files before the cached navigation changes.
function reloadAfterRefreshingShell() {
  if (!offlineShellSupported()) {
    location.reload();
    return;
  }
  withOfflineShellWorker((worker) => new Promise((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = (event) => resolve(event.data ?? {});
    worker.postMessage({type: 'warm', refresh: true, files: onDemandFileUrls()}, [channel.port2]);
  })).then((result) => {
    if (result === undefined || result.stored || result.unauthorized) {
      location.reload();
      return;
    }
    if (result.error) {
      reportShellRefreshFailure(new Error(result.error));
      return;
    }
    throw new Error('offline shell refresh returned no result');
  }).catch(reportShellRefreshFailure);
}
