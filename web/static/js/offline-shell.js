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

// The warm's outcome, one event per page load: the worker's reply once it has stored this release's
// shell and files ({stored: true} or {stored: false, error}), or {supported: false} when this browser
// has no service worker to ask. The poor-network bench waits for it before it closes a warm browser,
// so a shell the warm was storing survives the close.
function reportWarm(detail) {
  document.dispatchEvent(new CustomEvent('charliebot:offline-shell-warm', {detail}));
}

function registerOfflineShell() {
  if (!offlineShellSupported()) {
    reportWarm({supported: false});
    return;
  }
  navigator.serviceWorker.register(OFFLINE_SHELL_WORKER_PATH, {scope: '/?session='})
    .catch((err) => console.error('offline shell worker registration failed:', err));
  window.addEventListener('load', () => {
    navigator.serviceWorker.ready.then((registration) => {
      if (!registration.active) {
        reportWarm({supported: false});
        return;
      }
      const channel = new MessageChannel();
      channel.port1.onmessage = (event) => reportWarm(event.data ?? {});
      registration.active.postMessage({type: 'warm'}, [channel.port2]);
    }, (err) => reportWarm({error: String(err)}));
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
