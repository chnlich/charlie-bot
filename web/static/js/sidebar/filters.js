(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Sidebar filter registry
// ---------------------------------------------------------------------------
const sidebarFilters = [];
const sidebarFiltersByName = {};

// The pill strip lists the filters in registration order. A filter with
// `before` takes its place in front of the named filter. A package registers
// its filter when its script loads.
function registerSidebarFilter(filter) {
  if (!filter || !filter.name || !filter.label || !filter.url) throw new Error('Invalid sidebar filter registration');
  if (sidebarFiltersByName[filter.name]) throw new Error('Duplicate sidebar filter: ' + filter.name);
  const normalized = {
    name: filter.name,
    label: filter.label,
    url: filter.url,
    restoreFromUrl: filter.restoreFromUrl !== false,
  };
  const at = filter.before ? sidebarFilters.indexOf(sidebarFiltersByName[filter.before]) : sidebarFilters.length;
  if (at < 0) throw new Error('Unknown sidebar filter: ' + filter.before);
  sidebarFilters.splice(at, 0, normalized);
  sidebarFiltersByName[normalized.name] = normalized;
}

registerSidebarFilter({name: 'all', label: 'Workspace', url: '/api/sessions/?compact=1&screen=1', restoreFromUrl: false});
registerSidebarFilter({name: 'starred', label: 'Later', url: '/api/sessions/starred'});
registerSidebarFilter({name: 'archived', label: 'Archive', url: '/api/sessions/archived'});

function getSidebarFilter(filter) {
  return sidebarFiltersByName[filter] || null;
}

function workspaceListingUrl(activeSessionId) {
  const url = getSidebarFilter('all').url;
  if (!activeSessionId) return url;
  const separator = url.includes('?') ? '&' : '?';
  return url + separator + 'active=' + encodeURIComponent(activeSessionId);
}

// The offline shell stores the first screen's listing under the URL this builds (offline-shell.js).
registerOfflineShellListingUrl(workspaceListingUrl);

function getRestorableSidebarFilters() {
  return sidebarFilters.filter(filter => filter.restoreFromUrl).map(filter => filter.name);
}

function renderSidebarFilterPills() {
  const container = document.getElementById('sidebar-filter-pills');
  if (!container) return;
  const buttons = sidebarFilters.map(filter => {
    const active = filter.name === currentFilter;
    const cls = Sidebar.filterPillClass(active);
    return `<button onclick="enterSidebarFilter('${filter.name}')" id="filter-${filter.name}" class="${cls}">${filter.label}</button>`;
  }).join('');
  container.innerHTML = buttons;
}

// ---------------------------------------------------------------------------
// Sidebar filter & star
// ---------------------------------------------------------------------------
function setSidebarFilterPill(filter) {
  currentFilter = filter;
  renderSidebarFilterPills();
  document.querySelectorAll('.filter-pill').forEach(btn => {
    btn.classList.remove('bg-blue-600/20', 'text-blue-300');
    btn.classList.add('text-slate-400');
  });
  const active = document.getElementById('filter-' + filter);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('text-slate-400');
  }
}

// The badges at the top of the Workspace list: a package registers a loader
// when its script loads. load(signal) resolves to the badge HTML, or '' for no
// badge, and resolves even when its own fetch fails, so the list still paints.
// It rejects with an AbortError when the signal aborts: a search start cut the
// Workspace's requests short, and the badges load again with the clear.
const workspaceBadgeLoaders = [];

function registerWorkspaceBadge(load) {
  if (typeof load !== 'function') throw new Error('Invalid Workspace badge registration');
  if (workspaceBadgeLoaders.includes(load)) throw new Error('Duplicate Workspace badge');
  workspaceBadgeLoaders.push(load);
}

// Every loader runs on each Workspace fetch; the badges paint in registration
// order.
function loadWorkspaceBadges(signal) {
  return Promise.all(workspaceBadgeLoaders.map(load => load(signal))).then(badges => badges.join(''));
}

// The Workspace list's requests (the listing, its badges and the row refresh) share one abort signal. A search start
// aborts the ones in flight, and a request that starts after that takes the fresh controller.
let listController = new AbortController();

function listRequestSignal() {
  return listController.signal;
}

function abortListRequests() {
  listController.abort();
  listController = new AbortController();
}

// The Workspace memory a clear restores (the last Workspace paint) is fresh while no invalidation came after the full
// listing it was painted from. A group change, a reconnect, or a row restore that moved rows in or out of the
// Workspace invalidates it.
let workspaceInvalidations = 0;
let workspaceMemoryVersion = 0;

function invalidateWorkspaceMemory() {
  workspaceInvalidations += 1;
}

// A listing paints only while the search generation it was sent under is current and the search box is empty. A
// search that started after the request owns the list.
function listingMayPaint(generation) {
  return generation === searchGeneration && !searchBoxHasText();
}

function listingFailed(err) {
  // A request the search start aborted ends here without a message.
  if (err.name !== 'AbortError') console.error('Filter fetch failed:', err);
}

function switchSidebarFilter(filter) {
  // The rows the page polls move with the view, so the next status poll asks for the whole set.
  resetSessionStatusPoll();
  setSidebarFilterPill(filter);
  if (filter === 'archived') {
    // The archived view owns its fetch: keyset pagination with a group filter
    // strip (archived.js) instead of the one-shot array the other views use.
    loadArchivedView();
    return;
  }
  // Fetch sessions for this filter
  const generation = searchGeneration;
  const signal = listRequestSignal();
  if (filter === 'all') {
    // The Workspace badges and the session list load in parallel, so badges
    // and list paint together in the one render call.
    const memoryVersion = workspaceInvalidations;
    Promise.all([fetchFilterListing(filter, {networkOnly: false, signal}), loadWorkspaceBadges(signal)])
      .then(([listing, workspaceBadges]) => {
        if (!listingMayPaint(generation)) return;
        workspaceMemoryVersion = memoryVersion;
        workspaceBadgesStale = false;
        paintListing(listing, filter, {workspaceBadges});
      })
      .catch(listingFailed);
    return;
  }
  fetchFilterListing(filter, {networkOnly: false, signal})
    .then(listing => {
      if (!listingMayPaint(generation)) return;
      paintListing(listing, filter);
    })
    .catch(listingFailed);
}

// One view's listing from its list route: `rows` and `stored`, which says a stored copy answered it. Rows the page
// recorded an unread fact for after this request began keep that fact: fix E's gate (status.js replyIsCurrent)
// refuses the older answer. `networkOnly` asks the worker for the network answer (last-state.js).
function fetchFilterListing(filter, {networkOnly, signal}) {
  const registeredFilter = getSidebarFilter(filter);
  if (!registeredFilter) throw new Error('Unknown sidebar filter: ' + filter);
  const requestSeq = factSeqAtRequest();
  const headers = networkOnly ? networkOnlyHeaders() : {};
  // The open session's own row stays painted after the switch, whatever its rank in the list. The shell's stored
  // listing is keyed by this same URL, so the page's session comes from the URL when the page has not opened it yet.
  const activeSessionId = typeof SESSION_ID !== 'undefined' && SESSION_ID
    ? SESSION_ID
    : new URL(location.href).searchParams.get('session');
  const url = filter === 'all' ? workspaceListingUrl(activeSessionId) : registeredFilter.url;
  return fetch(url, {headers, signal}).then(res => {
    if (!res.ok) throw new Error(`Filter fetch failed: ${res.status}`);
    const stored = isStoredResponse(res);
    return res.json().then(payload => ({
      rows: listingRows(payload, requestSeq).map(row => keepCurrentUnread(row, requestSeq)),
      listing: listingScreen(payload),
      stored,
    }));
  });
}

function keepCurrentUnread(row, requestSeq) {
  if (replyIsCurrent('unread', row.id, requestSeq)) return row;
  return Object.assign({}, row, {has_unread: Sidebar.ownUnread(row.id)});
}

function paintListing({rows, listing, stored}, filter, options = {}) {
  if (stored) document.dispatchEvent(new CustomEvent('charliebot:stored-list'));
  renderSessionList(rows, filter, listing ? {...options, listing} : options);
}

// The sidebar list refresh that proves a copy current (last-state.js): the current view's list from the network,
// painted while that view is still on screen. It rejects unless a network answer arrived. The archived view pages its
// own rows and its list route is no copy source, so it has nothing to refresh here.
function refreshCurrentList() {
  const filter = currentFilter;
  if (filter === 'archived') return Promise.resolve();
  const generation = searchGeneration;
  const signal = listRequestSignal();
  const memoryVersion = workspaceInvalidations;
  const listing = fetchFilterListing(filter, {networkOnly: true, signal});
  const pending = filter === 'all'
    ? Promise.all([listing, loadWorkspaceBadges(signal)])
    : listing.then(result => [result]);
  return pending.then(([result, workspaceBadges]) => {
    if (result.stored) throw new Error('the list refresh was answered from a copy');
    // A view the reader left, or a search that started during the refresh, leaves the list to its own owner; the
    // refresh retries and proves the copy current once the list is on screen again.
    if (currentFilter !== filter || !listingMayPaint(generation)) {
      throw new Error('the list changed during its refresh');
    }
    if (filter === 'all') {
      workspaceMemoryVersion = memoryVersion;
      workspaceBadgesStale = false;
    }
    paintListing(result, filter, workspaceBadges ? {workspaceBadges} : {});
  });
}

// A listing's rows: the Workspace fetch answers with the page's column form, which expandSessionColumns restores,
// and a filter that answers with a row array needs no expansion. The rows' facts land through the reply gate
// at the fetch's requestSeq, before the list paints from the facts.
function listingRows(payload, requestSeq) {
  const rows = Array.isArray(payload) ? payload : Sidebar.expandSessionColumns(payload);
  applyListingFacts(rows, requestSeq);
  return rows;
}

// ---------------------------------------------------------------------------
// Sidebar row refresh
// ---------------------------------------------------------------------------
// A task-tree change names the session whose row changed. Every list the server can scope by filter fetches that row
// alone and merges it into the list it painted: the Workspace, the starred view and each registered view. Ids that
// arrive while a fetch is in flight go into the next fetch, and a hidden tab fetches once it is visible.
const ROW_REFRESH_BATCH = 50;
const pendingRowIds = new Set();
let rowRefreshInFlight = false;
// Set by a restore after a clear: the row refresh it starts re-reads the whole Workspace when a row entered or left it.
let restoreRowCheck = false;

function queueRowRefresh(sessionId) {
  pendingRowIds.add(sessionId);
  deferWhileHidden('row-refresh', drainRowRefresh);
}

// The row refresh URL of one view: the view's own list route with the changed rows named by `ids`. The view whose
// list is painted answers those rows in the compact column form the full list uses.
function rowRefreshUrl(filter, ids) {
  const registered = getSidebarFilter(filter);
  if (!registered) throw new Error('Unknown sidebar filter: ' + filter);
  const separator = registered.url.includes('?') ? '&' : '?';
  return registered.url + separator + 'ids=' + ids.map(encodeURIComponent).join(',');
}

function drainRowRefresh() {
  if (rowRefreshInFlight) return;
  if (!sidebarRowsRefreshable()) {
    // An open search keeps the ids for the restore after its clear. Any other view drops them.
    if (!searchBoxHasText()) pendingRowIds.clear();
    return;
  }
  if (pendingRowIds.size === 0) {
    restoreRowCheck = false;
    return;
  }
  const filter = currentFilter;
  const batch = Array.from(pendingRowIds).slice(0, ROW_REFRESH_BATCH);
  batch.forEach(id => pendingRowIds.delete(id));
  rowRefreshInFlight = true;
  const requestSeq = factSeqAtRequest();
  fetch(rowRefreshUrl(filter, batch), {signal: listRequestSignal()})
    .then(res => {
      if (!res.ok) throw new Error(`Row refresh failed: ${res.status}`);
      return res.json();
    })
    .then(payload => {
      // A view the reader left while the fetch was in flight takes no rows: its own load paints it.
      if (currentFilter !== filter) return;
      // A search that started while the fetch was in flight keeps the ids for the restore after its clear.
      if (!sidebarRowsRefreshable()) {
        batch.forEach(id => pendingRowIds.add(id));
        return;
      }
      const rows = listingRows(payload.sessions, requestSeq);
      if (restoreRowCheck && rowsChangeMembership(rows, payload.gone)) {
        // The group counts follow the membership, which only a full listing carries.
        invalidateWorkspaceMemory();
        pendingRowIds.clear();
        switchSidebarFilter(filter);
        return;
      }
      mergeRowsIntoRenderedList(rows, payload.gone);
    })
    .catch(err => {
      // A search start aborted the fetch: its ids wait for the restore after the clear.
      if (err.name === 'AbortError') {
        batch.forEach(id => pendingRowIds.add(id));
        return;
      }
      console.error('Row refresh failed:', err);
    })
    .finally(() => {
      rowRefreshInFlight = false;
      // Ids that arrived during the fetch form the next one. A tab that went hidden meanwhile holds it until it is
      // visible, like any other work that hides.
      deferWhileHidden('row-refresh', drainRowRefresh);
    });
}

// The Workspace's badges (the broken cron tasks) load and repaint in place. The Workspace view owns them, so any other
// view takes no request. A search takes none either: the badges are stale until the clear loads them again.
let workspaceBadgesStale = false;

function refreshWorkspaceBadges() {
  if (currentFilter !== 'all') return;
  if (searchBoxHasText()) {
    workspaceBadgesStale = true;
    return;
  }
  loadWorkspaceBadges(listRequestSignal())
    .then(workspaceBadges => {
      workspaceBadgesStale = false;
      paintWorkspaceBadges(workspaceBadges);
    })
    .catch(err => {
      if (err.name === 'AbortError') {
        workspaceBadgesStale = true;
        return;
      }
      console.error('Workspace badges failed:', err);
    });
}

// Pill-click entry point: only entering a different filter collapses group
// expansions; re-clicking the active pill and every in-place refresh keep them.
function enterSidebarFilter(filter) {
  if (filter !== currentFilter) resetGroupLimitState();
  switchSidebarFilter(filter);
}

function renderSidebarLoadErrors(errors) {
  const nav = document.getElementById('session-list');
  nav.innerHTML = errors.map(err =>
    `<div class="mx-3 my-2 px-3 py-2 rounded-lg bg-red-900/40 border border-red-700/50 text-red-300 text-xs">${escapeHtml(err)}</div>`
  ).join('');
}

// The rows of the sidebar the page carries, as `$screen` declares them: the first screen's rows, each group's whole
// row count, and whether the page left rows out. The page leaves a group's rows out once it paints more roots than
// the preview limit, so the reader's "Show all" fetches them from the listing route.
function pageScreen() {
  return listingScreen(INITIAL_SESSIONS);
}

function listingScreen(payload) {
  const screen = payload && payload.$screen;
  return screen && screen.partial ? screen : null;
}

function restoreSidebarFromUrl() {
  renderSidebarFilterPills();
  if (INITIAL_SESSIONS === null) {
    // The shell carries no rows: the list is the listing route's, from a stored copy or the network.
    const urlShellFilter = new URLSearchParams(location.search).get('filter');
    switchSidebarFilter(getRestorableSidebarFilters().includes(urlShellFilter) ? urlShellFilter : 'all');
    return;
  }
  // The server rendered the page's rows before this page applied any fact.
  const initialSessions = listingRows(INITIAL_SESSIONS, 0);
  const params = new URLSearchParams(location.search);
  const urlFilter = params.get('filter');
  const urlQuery = params.get('q');
  if (urlQuery) {
    const searchInput = document.getElementById('sidebar-search');
    if (searchInput) { searchInput.value = urlQuery; handleSidebarSearch(urlQuery); }
  } else if (getRestorableSidebarFilters().includes(urlFilter)) {
    switchSidebarFilter(urlFilter);
  } else {
    setSidebarFilterPill('all');
    if (INITIAL_LOAD_ERRORS.length) {
      renderSidebarLoadErrors(INITIAL_LOAD_ERRORS);
    } else {
      renderSessionList(initialSessions, 'all', {listing: pageScreen()});
      // The Workspace badges load on their own so the first paint never waits
      // for them; the repaint discards itself when the view has moved on. A
      // hidden tab loads them once it is visible.
      deferWhileHidden('workspace-badges', refreshWorkspaceBadges);
    }
  }
}

// ---------------------------------------------------------------------------
// Session search
// ---------------------------------------------------------------------------
let searchDebounceTimer = null;
// The query generation the list is waiting for. Each input text takes the next
// one. A response carries the generation it was sent under, so a slow earlier
// query cannot repaint the rows of a newer one. A broadcast refresh does not
// take one: its answer is for the same text.
let searchGeneration = 0;
// The text the next search request answers: the newest query typed.
let searchQueryText = '';
// The query whose rows the list holds, '' while it holds another view or a
// note. A same-text call is a refresh. The WebSocket handlers refetch the open
// query when a session or task tree changes, and that refresh keeps the rows
// the reader is on.
let shownSearchQuery = '';
// Whether the list holds a search note instead of rows.
let searchPending = false;
// The response body the list last painted for a search. A response with the
// same body repaints nothing.
let shownSearchBody = null;
// The generation of the search request on the wire for the current query, or
// null. A broadcast refresh of that query waits for the request instead of
// sending another.
let searchInFlightGeneration = null;
// Whether a broadcast refresh arrived while the current query's request was in
// flight. The request's end sends one more request for the newest text.
let searchRefreshPending = false;

// The search field's clear control (index.html, wired below): the reader's way back to the pills and
// the current filter's rows. The empty field hides the control itself, so this only empties the box
// and hands the query to the one search path.
function clearSidebarSearch() {
  const input = document.getElementById('sidebar-search');
  input.value = '';
  input.focus();
  handleSidebarSearch('');
}

// The clear's list. On the Workspace with a fresh memory, the rows paint from memory at once, and a row refresh then
// fetches the ids the search recorded. Any other view, or a stale memory, takes the full listing, which covers those
// ids too.
function leaveSearch() {
  if (currentFilter === 'all' && workspaceMemoryVersion === workspaceInvalidations && restoreWorkspaceFromMemory()) {
    restoreRowCheck = true;
    if (workspaceBadgesStale) refreshWorkspaceBadges();
    deferWhileHidden('row-refresh', drainRowRefresh);
    return;
  }
  pendingRowIds.clear();
  switchSidebarFilter(currentFilter);
}

// `fromBroadcast` marks a refresh a broadcast asked for. While a request is in
// flight, that refresh only marks the rows stale and keeps the generation: the
// request's end sends the one request that follows, so a burst of broadcasts
// costs at most one more request.
function handleSidebarSearch(query, fromBroadcast = false) {
  const text = query.trim();
  // A search start aborts the Workspace's requests in flight, before the debounce and the search request.
  if (text) abortListRequests();
  if (fromBroadcast && searchInFlightGeneration === searchGeneration) {
    searchRefreshPending = true;
    return;
  }
  clearTimeout(searchDebounceTimer);
  const pills = document.querySelector('.filter-pill')?.parentElement;
  ++searchGeneration;
  // A new query supersedes a refresh that was due for the old one.
  searchRefreshPending = false;
  searchQueryText = text;
  if (text) {
    // Hide filter pills while searching
    if (pills) pills.style.display = 'none';
    if (text !== shownSearchQuery) {
      // The rows answer other text, so they go before this request starts. A
      // content search over a production session home takes long enough to
      // see, and rows that contradict the box read as a search doing nothing.
      renderSearchNote('Searching…');
      shownSearchQuery = '';
      shownSearchBody = null;
      searchPending = true;
    }
    searchDebounceTimer = setTimeout(requestSearchRows, 300);
  } else {
    shownSearchQuery = '';
    shownSearchBody = null;
    searchPending = false;
    // Restore filter pills and current filter
    if (pills) pills.style.display = '';
    leaveSearch();
  }
}

// The debounced search: sends the request for the newest text, or marks one due while that query's request is in
// flight.
function requestSearchRows() {
  if (searchInFlightGeneration === searchGeneration) {
    searchRefreshPending = true;
    return;
  }
  sendSearchRequest();
}

// Sends the search for the newest text under the current generation. Its end, success or rejection, releases the
// in-flight state and sends the request a broadcast left due. A response for a superseded generation paints nothing.
function sendSearchRequest() {
  const text = searchQueryText;
  const generation = searchGeneration;
  const requestSeq = factSeqAtRequest();
  searchInFlightGeneration = generation;
  globalThis.sidebarSearchCounters.fetch += 1;
  fetch('/api/sessions/search?q=' + encodeURIComponent(text))
    .then(res => res.json())
    .then(payload => {
      if (generation !== searchGeneration) {
        globalThis.sidebarSearchCounters.discard += 1;
        return;
      }
      shownSearchQuery = text;
      searchPending = false;
      const rows = listingRows(payload, requestSeq);
      // An answer identical to the one painted leaves the list as it is.
      const body = JSON.stringify(payload);
      if (body === shownSearchBody) return;
      shownSearchBody = body;
      renderSessionList(rows, 'search');
    })
    .catch(err => {
      if (generation !== searchGeneration) {
        globalThis.sidebarSearchCounters.discard += 1;
        return;
      }
      console.error('Search failed:', err);
      // A failed refresh keeps the rows already on screen; only a search
      // that has no rows yet replaces its note with the failure.
      if (searchPending) {
        shownSearchBody = null;
        renderSearchNote('Search failed; try again.');
      }
    })
    .finally(() => {
      if (searchInFlightGeneration === generation) searchInFlightGeneration = null;
      if (searchRefreshPending) {
        searchRefreshPending = false;
        requestSearchRows();
      }
    });
}

async function toggleSessionStar(id, currentlyStarred) {
  const endpoint = currentlyStarred ? 'unstar' : 'star';
  // Optimistic UI update: repaint the button into the new state; groups.js's
  // renderStarButton paints the same state from the same icon keys and class literals.
  const btn = document.getElementById('star-' + id);
  if (btn) {
    const starred = !currentlyStarred;
    btn.querySelector('svg').outerHTML = cbIcon(starred ? 'starred' : 'star', 14);
    btn.classList.toggle('text-yellow-400', starred);
    btn.classList.toggle('!opacity-100', starred);
    btn.classList.toggle('hover:text-yellow-400', !starred);
    btn.setAttribute('onclick', Sidebar.starButtonOnclick(id, starred));
  }
  // The archived tree's next repaint re-renders from its merged row list, so
  // the flip rides the stored row too, not just the on-screen button.
  if (currentFilter === 'archived') archivedStarToggled(id, !currentlyStarred);
  try {
    await fetch(`/api/sessions/${id}/${endpoint}`, { method: 'POST' });
    // If viewing starred filter and we just unstarred, remove from list
    if (currentFilter === 'starred' && currentlyStarred) {
      switchSidebarFilter(currentFilter);
    }
  } catch (err) {
    console.error('Star toggle failed:', err);
  }
}

// The session rows' star button (groups.js).
registerSidebarControl('toggleSessionStar', toggleSessionStar);

const GLOBALS = {
  registerSidebarFilter,
  registerWorkspaceBadge,
  setSidebarFilterPill,
  switchSidebarFilter,
  enterSidebarFilter,
  restoreSidebarFromUrl,
  handleSidebarSearch,
  clearSidebarSearch,
  toggleSessionStar,
  queueRowRefresh,
  refreshWorkspaceBadges,
  invalidateWorkspaceMemory,
  refreshCurrentList,
  workspaceListingUrl,
};
Sidebar.wire(GLOBALS);

})();
