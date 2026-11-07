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

registerSidebarFilter({name: 'all', label: 'Workspace', url: '/api/sessions/?compact=1', restoreFromUrl: false});
registerSidebarFilter({name: 'starred', label: 'Later', url: '/api/sessions/starred'});
registerSidebarFilter({name: 'archived', label: 'Archive', url: '/api/sessions/archived'});

function getSidebarFilter(filter) {
  return sidebarFiltersByName[filter] || null;
}

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
// when its script loads. load() resolves to the badge HTML, or '' for no
// badge, and resolves even when its own fetch fails, so the list still paints.
const workspaceBadgeLoaders = [];

function registerWorkspaceBadge(load) {
  if (typeof load !== 'function') throw new Error('Invalid Workspace badge registration');
  if (workspaceBadgeLoaders.includes(load)) throw new Error('Duplicate Workspace badge');
  workspaceBadgeLoaders.push(load);
}

// Every loader runs on each Workspace fetch; the badges paint in registration
// order.
function loadWorkspaceBadges() {
  return Promise.all(workspaceBadgeLoaders.map(load => load())).then(badges => badges.join(''));
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
  if (filter === 'all') {
    // The Workspace badges and the session list load in parallel, so badges
    // and list paint together in the one render call.
    Promise.all([fetchFilterListing(filter, {networkOnly: false}), loadWorkspaceBadges()])
      .then(([listing, workspaceBadges]) => paintListing(listing, filter, {workspaceBadges}))
      .catch(err => console.error('Filter fetch failed:', err));
    return;
  }
  fetchFilterListing(filter, {networkOnly: false})
    .then(listing => paintListing(listing, filter))
    .catch(err => console.error('Filter fetch failed:', err));
}

// One view's listing from its list route: `rows` and `stored`, which says a stored copy answered it. Rows the page
// recorded an unread fact for after this request began keep that fact: fix E's gate (status.js replyIsCurrent)
// refuses the older answer. `networkOnly` asks the worker for the network answer (last-state.js).
function fetchFilterListing(filter, {networkOnly}) {
  const registeredFilter = getSidebarFilter(filter);
  if (!registeredFilter) throw new Error('Unknown sidebar filter: ' + filter);
  const requestSeq = factSeqAtRequest();
  const headers = networkOnly ? networkOnlyHeaders() : {};
  return fetch(registeredFilter.url, {headers}).then(res => {
    if (!res.ok) throw new Error(`Filter fetch failed: ${res.status}`);
    const stored = isStoredResponse(res);
    return res.json().then(payload => ({
      rows: listingRows(payload).map(row => keepCurrentUnread(row, requestSeq)),
      stored,
    }));
  });
}

function keepCurrentUnread(row, requestSeq) {
  if (replyIsCurrent('unread', row.id, requestSeq)) return row;
  return Object.assign({}, row, {has_unread: sessionUnread[row.id]});
}

function paintListing({rows, stored}, filter, options = {}) {
  if (stored) document.dispatchEvent(new CustomEvent('charliebot:stored-list'));
  renderSessionList(rows, filter, options);
}

// The sidebar list refresh that proves a copy current (last-state.js): the current view's list from the network,
// painted while that view is still on screen. It rejects unless a network answer arrived. The archived view pages its
// own rows and its list route is no copy source, so it has nothing to refresh here.
function refreshCurrentList() {
  const filter = currentFilter;
  if (filter === 'archived') return Promise.resolve();
  const listing = fetchFilterListing(filter, {networkOnly: true});
  const pending = filter === 'all'
    ? Promise.all([listing, loadWorkspaceBadges()])
    : listing.then(result => [result]);
  return pending.then(([result, workspaceBadges]) => {
    if (result.stored) throw new Error('the list refresh was answered from a copy');
    if (currentFilter === filter) paintListing(result, filter, workspaceBadges ? {workspaceBadges} : {});
  });
}

// A listing's rows: the Workspace fetch answers with the page's column form, which expandSessionColumns restores,
// and a filter that answers with a row array needs no expansion.
function listingRows(payload) {
  return Array.isArray(payload) ? payload : Sidebar.expandSessionColumns(payload);
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
    pendingRowIds.clear();
    return;
  }
  if (pendingRowIds.size === 0) return;
  const filter = currentFilter;
  const batch = Array.from(pendingRowIds).slice(0, ROW_REFRESH_BATCH);
  batch.forEach(id => pendingRowIds.delete(id));
  rowRefreshInFlight = true;
  fetch(rowRefreshUrl(filter, batch))
    .then(res => {
      if (!res.ok) throw new Error(`Row refresh failed: ${res.status}`);
      return res.json();
    })
    .then(payload => {
      // A view the reader left while the fetch was in flight takes no rows: its own load paints it.
      if (sidebarRowsRefreshable() && currentFilter === filter) {
        mergeRowsIntoRenderedList(Sidebar.expandSessionColumns(payload.sessions), payload.gone);
      }
    })
    .catch(err => console.error('Row refresh failed:', err))
    .finally(() => {
      rowRefreshInFlight = false;
      // Ids that arrived during the fetch form the next one. A tab that went hidden meanwhile holds it until it is
      // visible, like any other work that hides.
      deferWhileHidden('row-refresh', drainRowRefresh);
    });
}

// The Workspace's badges (the broken cron tasks) load and repaint in place. The Workspace view owns them, so any other
// view takes no request.
function refreshWorkspaceBadges() {
  if (currentFilter !== 'all') return;
  loadWorkspaceBadges().then(paintWorkspaceBadges);
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
  const screen = INITIAL_SESSIONS && INITIAL_SESSIONS.$screen;
  return screen && screen.partial ? screen : null;
}

function restoreSidebarFromUrl() {
  renderSidebarFilterPills();
  const initialSessions = Sidebar.expandSessionColumns(INITIAL_SESSIONS);
  Sidebar.resyncSessionUnread(initialSessions);
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
// one. A response carries the generation it was fetched under, so a slow
// earlier query cannot repaint the rows of a newer one.
let searchGeneration = 0;
// The query whose rows the list holds, '' while it holds another view or a
// note. A same-text call is a refresh. The WebSocket handlers refetch the open
// query when a session or task tree changes, and that refresh keeps the rows
// the reader is on.
let shownSearchQuery = '';
// Whether the list holds a search note instead of rows.
let searchPending = false;

function handleSidebarSearch(query) {
  clearTimeout(searchDebounceTimer);
  const pills = document.querySelector('.filter-pill')?.parentElement;
  const generation = ++searchGeneration;
  const text = query.trim();
  if (text) {
    // Hide filter pills while searching
    if (pills) pills.style.display = 'none';
    if (text !== shownSearchQuery) {
      // The rows answer other text, so they go before this request starts. A
      // content search over a production session home takes long enough to
      // see, and rows that contradict the box read as a search doing nothing.
      renderSearchNote('Searching…');
      shownSearchQuery = '';
      searchPending = true;
    }
    searchDebounceTimer = setTimeout(() => {
      fetch('/api/sessions/search?q=' + encodeURIComponent(text))
        .then(res => res.json())
        .then(sessions => {
          if (generation !== searchGeneration) return;
          shownSearchQuery = text;
          searchPending = false;
          renderSessionList(sessions, 'search');
        })
        .catch(err => {
          if (generation !== searchGeneration) return;
          console.error('Search failed:', err);
          // A failed refresh keeps the rows already on screen; only a search
          // that has no rows yet replaces its note with the failure.
          if (searchPending) renderSearchNote('Search failed; try again.');
        });
    }, 300);
  } else {
    shownSearchQuery = '';
    searchPending = false;
    // Restore filter pills and current filter
    if (pills) pills.style.display = '';
    switchSidebarFilter(currentFilter);
  }
}

async function toggleSessionStar(id, currentlyStarred) {
  const endpoint = currentlyStarred ? 'unstar' : 'star';
  // Optimistic UI update: repaint the button into the new state; groups.js's
  // renderStarButton paints the same state from the same two literals.
  const btn = document.getElementById('star-' + id);
  if (btn) {
    const starred = !currentlyStarred;
    btn.querySelector('svg').setAttribute('fill', starred ? 'currentColor' : 'none');
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
  toggleSessionStar,
  queueRowRefresh,
  refreshWorkspaceBadges,
  refreshCurrentList,
};
Sidebar.wire(GLOBALS);

})();
