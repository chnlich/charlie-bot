// ---------------------------------------------------------------------------
// Offline shell: the page's side of the service worker (offline-shell-worker.js)
// ---------------------------------------------------------------------------
// The page registers the worker, asks it to store the shell once the page has loaded, drops the stored
// shell before a 4409 reload, and drops the stored copies on a 401. A browser without service workers
// keeps the page it always had.

const OFFLINE_SHELL_WORKER_PATH = '/offline-shell-worker.js';

function offlineShellSupported() {
  return 'serviceWorker' in navigator;
}

// Whether a worker controls this page: page_load reports it as `offline_shell`.
function offlineShellPresent() {
  return offlineShellSupported() && navigator.serviceWorker.controller !== null;
}

function registerOfflineShell() {
  if (!offlineShellSupported()) return;
  navigator.serviceWorker.register(OFFLINE_SHELL_WORKER_PATH, {scope: '/?session='})
    .catch((err) => console.error('offline shell worker registration failed:', err));
  window.addEventListener('load', () => {
    navigator.serviceWorker.ready.then((registration) => registration.active.postMessage({type: 'warm'}));
  });
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

// The reload after a 4409 close. With a worker holding the page, the stored shell of the old release goes first,
// so the reload reads the current release's page from the network; without one, the page reloads at once.
function reloadAfterDroppingShell() {
  if (!offlineShellSupported()) {
    location.reload();
    return;
  }
  withOfflineShellWorker((worker) => new Promise((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = () => resolve();
    worker.postMessage({type: 'drop-shell'}, [channel.port2]);
  })).then(() => location.reload());
}
