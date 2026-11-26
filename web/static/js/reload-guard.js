// The automatic code reloads of a tab (feature scripts here, the socket's swaps and 4409 reloads in websocket.js) share
// one guard: a navigation starts at most once per STALE_PAGE_RELOAD_GUARD_MS per tab, so no stored state turns a
// mismatch into a reload loop. The stamp is written when a reload navigates.
const STALE_PAGE_RELOAD_KEY = 'charliebot-stale-page-reload-at';
const STALE_PAGE_RELOAD_GUARD_MS = 60000;

function staleReloadWaitMs() {
  const lastReload = Number(sessionStorage.getItem(STALE_PAGE_RELOAD_KEY));
  if (!lastReload) return 0;
  return Math.max(0, lastReload + STALE_PAGE_RELOAD_GUARD_MS - Date.now());
}

function markStaleReload() {
  sessionStorage.setItem(STALE_PAGE_RELOAD_KEY, String(Date.now()));
}
