// ---------------------------------------------------------------------------
// Offline shell: the page's side of the service worker (offline-shell-worker.js)
// ---------------------------------------------------------------------------
// The page registers the worker once its catch-up completes and asks it to store the shell then, asks it to store
// the session page's on-demand files once the page is idle, reloads past the stored shell on a 4409 close, reports
// the catch-up's end to the worker, reports the start and the end of its user-waiting requests so the worker holds
// its installs off them, and drops the stored copies on a 401. A browser without service workers keeps the page it
// always had.

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
// or {supported: false} when this browser has no service worker to ask. Each report carries `update`: the
// states of the update the warm waited for (whenUpdateTakesOver), or null. The poor-network bench waits for
// the report before it closes a warm browser, so a shell the warm was storing survives the close.
function reportWarm(detail) {
  document.dispatchEvent(new CustomEvent('charliebot:offline-shell-warm', {detail: {...detail, update: updateStates}}));
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
  currentWorkerRegistration().then((registration) => {
    if (registration === undefined) throw new Error('the offline shell has no registration');
    if (!registration.active) {
      reportWarm({supported: false});
      return;
    }
    postWarm(registration, true);
  }).catch((err) => reportWarm({error: String(err)}));
}

// The warm goes to the registration's active worker. An older release's worker answers from its own stored shell (its
// cache is named for its release), so a reply from another release means the update has not taken over yet: the page
// checks for the update, waits for the worker that takes over, and warms that one.
function postWarm(registration, mayCheckUpdate) {
  const channel = new MessageChannel();
  channel.port1.onmessage = (event) => {
    const report = event.data ?? {};
    if (report.version === STATIC_ASSET_VERSION) {
      reportWarm(report);
    } else if (!mayCheckUpdate) {
      reportWarm({stored: false, error: `the worker is release ${report.version}, not ${STATIC_ASSET_VERSION}`});
    } else {
      registration.update()
          .then(() => whenUpdateTakesOver(registration))
          .then((taken) => postWarm(taken, false))
          .catch((err) => reportWarm({error: String(err)}));
    }
  };
  registration.active.postMessage({type: 'warm', copies: offlineShellSeedCopies(), files: onDemandFileUrls()},
      [channel.port2]);
}

// The registration the page's messages to the current release's worker go to, once any update it holds has taken
// over. The register call checks the server for a new worker script. Until an update takes over, the worker the
// registration still reports as active is the release the update replaces: it answers a warm from its own shell,
// and the new worker drops that shell when it activates. PAGE_UPDATE_CAP_MS bounds the wait: past it the warm fails
// with an error, so the warm never waits on an update that does not finish. The page's next load warms again.
//
// The cap's measured basis, on the 100 kbit/s link: from the page's first sight of the installed worker to its
// activation, the takeover took 4.1 to 8.8 s in eight of nine release reps of a takeover run on eba6eb936a, and 13.0 s
// in the ninth, which the former 10 s cap failed. The stored group on 3fe3942324 measured 4.1 to 12.8 s (median 7.5 s,
// p90 11.9 s) in ten reps. The gap is consistent with the activation waiting for the replaced worker's in-flight
// install fetch, which runs for about 7 s (the shell's transfer on this link) once the page is quiet. 20 s covers the
// worst measured takeover with margin.
const PAGE_UPDATE_CAP_MS = 20000;

// The states the page saw of the update it waits for, each with its time in ms since the page's time origin. Every
// warm report carries them (reportWarm), so a takeover that runs long shows the state it stalled in. Null until the
// page waits for an update.
let updateStates = null;

function recordUpdateState(worker) {
  if (!(worker.state in updateStates)) updateStates[worker.state] = Math.round(performance.now());
}

function currentWorkerRegistration() {
  return navigator.serviceWorker.register(OFFLINE_SHELL_WORKER_PATH, {scope: '/?session='})
    .catch((err) => console.error('offline shell worker registration failed:', err))
    .then(() => navigator.serviceWorker.getRegistration())
    .then((registration) => (registration === undefined ? undefined : whenUpdateTakesOver(registration)));
}

function whenUpdateTakesOver(registration) {
  const next = registration.installing ?? registration.waiting ?? null;
  if (next === null) return Promise.resolve(registration);
  updateStates = {};
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
        () => reject(new Error(`the worker update did not take over within ${PAGE_UPDATE_CAP_MS} ms`)),
        PAGE_UPDATE_CAP_MS);
    const settle = () => {
      recordUpdateState(next);
      if (next.state === 'activated') {
        clearTimeout(timer);
        resolve(registration);
      } else if (next.state === 'redundant') {
        clearTimeout(timer);
        reject(new Error('the worker update was discarded before it took over'));
      }
    };
    next.addEventListener('statechange', settle);
    // The worker may have reached its state before the listener attached, and then no statechange reaches it:
    // the state is read once the listener is in place, so such a state resolves at once.
    settle();
  });
}

// ---------------------------------------------------------------------------
// On-demand files: the same-origin files a view or panel loads when it is first used
// ---------------------------------------------------------------------------
// The files the session page's loadPageScripts and loadPageStyles calls name (utils.js), and the sidebar interaction
// files (sidebar/namespace.js). Each is either kept or excluded with its reason. A kept file renders stored content
// offline, so the worker stores it once the page is current and idle: an offline open then shows that view without its
// first online use. An excluded file's only function talks to the server, so its control shows its offline state and
// needs no stored code. offline_shell_page.test.js fails when a loader names a same-origin file neither list names.
//
// Kept: status-panel.js draws the Status view with its offline mark; comment_post.js is loaded before it, and its send
// path (startUndoableSend) registers the page-leave flush at load; worker-summary.js paints the For-you row's summary
// from the stored bootstrap; file-links.js marks file links in stored messages, and a failed probe is not a missing
// file; draft-cards.js paints the stored draft rows; artifact-cards.js renders artifact-page links in stored messages
// as cards. Each stylesheet follows its script.
const ON_DEMAND_FILES = Object.freeze([
  '/static/js/chat/status-panel.js',
  '/static/js/comment_post.js',
  '/static/js/chat/worker-summary.js',
  '/static/css/worker-summary.css',
  '/static/js/chat/file-links.js',
  '/static/features/drafts/draft-cards.js',
  '/static/features/artifacts/artifact-cards.js',
  '/static/features/artifacts/artifacts.css',
]);

// Excluded: each same-origin file the loaders name that the worker does not store, with the reason.
const ON_DEMAND_EXCLUDED = Object.freeze({
  '/static/features/terminal/terminal_b64.js': 'terminal: a shell session on the server, no stored content',
  '/static/features/terminal/terminal_clipboard.js': 'terminal: a shell session on the server, no stored content',
  '/static/features/terminal/terminal_mount.js': 'terminal: a shell session on the server, no stored content',
  '/static/features/terminal/terminal_panel.js': 'terminal: a shell session on the server, no stored content',
  '/static/js/file-upload.js': 'file upload: sends the file to the server',
  '/static/css/file-upload.css': 'file upload: the stylesheet of the upload control',
  '/static/features/voice/voice-input.js': 'voice input: records and sends audio to the server',
  '/static/features/voice/voice-overlay.css': 'voice input: the stylesheet of the voice control',
  '/static/features/cron/cron.js': 'cron tasks: lists and edits the server tasks',
  '/static/features/usage/ext_usage.js': 'usage: fetches the usage figures from the server',
  '/static/features/usage/usage.css': 'usage: the stylesheet of the usage panel',
  '/static/js/sidebar/archived.js': 'archived list: fetches the archived sessions from the server',
  '/static/js/sidebar/session-actions.js': 'session actions: rename, archive, delete and unarchive on the server',
  '/static/js/sidebar/session-menus.js': 'session menus: the menus of the session actions',
  '/static/js/sidebar/row-menu.js': 'row menu widget: the menus of the session actions',
  '/static/js/sidebar/modals.js': 'sidebar modals: the action dialogs and the group editor post to the server',
  '/static/js/sidebar/session-move.js': 'move picker: fetches the groups and moves the session on the server',
  '/static/css/sidebar-interactions.css': 'sidebar actions: the stylesheet of the session actions',
  '/static/js/chat/triggers-tray.js': 'pending triggers: fetched from and cancelled on the server',
  '/static/css/triggers-tray.css': 'pending triggers: the stylesheet of the triggers tray',
  '/static/features/explain/explain.js': 'explain: asks the server for each explanation',
  '/static/features/artifacts/plan-panel.js': 'plans panel: fetches the plans from the server; the copy holds none',
});

// The versioned URLs the page requests for the kept files (utils.js pageAssetUrl), so the worker's stored keys are the
// page's URLs.
function onDemandFileUrls() {
  return ON_DEMAND_FILES.map((path) => pageAssetUrl(path));
}

// The worker stores the kept files when the page is current and the browser is idle. The reply is one event per
// request, {stored, revalidated} or {error}; the poor-network bench waits for it before a warm browser closes.
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

// The reload after a 4409 close navigates past the stored shell. The stored shell is the release the close found stale,
// so the navigation carries RELOAD_PAST_SHELL_PARAM, which the worker passes to the network (offline-shell-worker.js).
// The page the server now serves loads as it does on a network reload, and the new page's warm stores the new shell.
const RELOAD_PAST_SHELL_PARAM = 'reloading';

// The worker that answered this page's shell learns that the server runs a newer release first, so the navigation the
// reload makes goes to the network: a later navigation of this tab cannot take the same stale shell (4409 again). The
// update check runs first, so the release's worker script is fetched and its install starts before the reload.
function reloadPastStoredShell() {
  const url = new URL(location.href);
  url.searchParams.set(RELOAD_PAST_SHELL_PARAM, '1');
  if (!offlineShellSupported()) {
    location.replace(url.href);
    return;
  }
  navigator.serviceWorker.getRegistration()
      .then((registration) => registration?.update())
      .catch((err) => console.warn('offline shell: the update check failed:', err));
  const controller = navigator.serviceWorker.controller;
  if (!controller) {
    location.replace(url.href);
    return;
  }
  const channel = new MessageChannel();
  channel.port1.onmessage = (event) => {
    if (event.data.error !== undefined) console.warn('offline shell: the stale-shell mark failed:', event.data.error);
    location.replace(url.href);
  };
  controller.postMessage({type: 'stale-shell'}, [channel.port2]);
}

// The page takes the parameter out of the address bar once it has loaded: the marked navigation is the one that loaded
// the page, and a later reload should answer from the stored shell as any other does.
function removeReloadPastShellParam() {
  const url = new URL(location.href);
  if (!url.searchParams.has(RELOAD_PAST_SHELL_PARAM)) return;
  url.searchParams.delete(RELOAD_PAST_SHELL_PARAM);
  history.replaceState(history.state, '', url.href);
}

window.addEventListener('load', removeReloadPastShellParam);

// ---------------------------------------------------------------------------
// The user-waiting requests: the worker holds its installs while one is in flight
// ---------------------------------------------------------------------------
// The requests the user waits on are a first screen, a session's older events, the sidebar's full list, and a send.
// The request goes out unchanged; its start and its end are reported to the worker, which aborts or defers the shell
// and on-demand fetches it has in flight (offline-shell-worker.js, pageRequestStarted). The end is the response's
// arrival: the worker's quiet gap covers the body that follows it.
function isUserWaitingRequest(input, init) {
  const url = new URL(typeof input === 'string' ? input : input.url, location.href);
  if (url.origin !== location.origin) return false;
  const method = (init?.method ?? input.method ?? 'GET').toUpperCase();
  if (method === 'POST') return /^\/api\/chat\/[^/]+\/message$/.test(url.pathname);
  if (method !== 'GET') return false;
  return /^\/api\/sessions\/[^/]+\/(bootstrap|events)$/.test(url.pathname) ||
    (url.pathname === '/api/sessions/' && !url.searchParams.has('ids'));
}

// The worker that holds this page takes the report once its registration is active. Reports wait in order, so a
// report made before the first registration reaches the worker with the reports after it.
function reportUserRequest(type, id) {
  navigator.serviceWorker.ready.then((registration) => registration.active.postMessage({type, id}));
}

// The reported requests that have not ended. An unload ends them, so no worker install waits on a page that is gone.
const userRequestsOpen = new Set();
let userRequestCount = 0;

function reportUserWaitingRequests() {
  const pageFetch = window.fetch;
  window.fetch = function(input, init) {
    if (!isUserWaitingRequest(input, init)) return pageFetch.call(window, input, init);
    const id = `${performance.timeOrigin}:${++userRequestCount}`;
    userRequestsOpen.add(id);
    reportUserRequest('page-request-start', id);
    return pageFetch.call(window, input, init).finally(() => endUserRequest(id));
  };
  window.addEventListener('pagehide', () => {
    for (const id of userRequestsOpen) reportUserRequest('page-request-end', id);
    userRequestsOpen.clear();
  });
}

function endUserRequest(id) {
  if (!userRequestsOpen.delete(id)) return;
  reportUserRequest('page-request-end', id);
}

if (offlineShellSupported()) reportUserWaitingRequests();
