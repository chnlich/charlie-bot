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
  setSidebarFilterPill(filter);
  if (filter === 'archived') {
    // The archived view owns its fetch: keyset pagination with a group filter
    // strip (archived.js) instead of the one-shot array the other views use.
    loadArchivedView();
    return;
  }
  // Fetch sessions for this filter
  const registeredFilter = getSidebarFilter(filter);
  if (!registeredFilter) throw new Error('Unknown sidebar filter: ' + filter);
  if (filter === 'all') {
    // The Workspace badges and the session list load in parallel, so badges
    // and list paint together in the one render call.
    Promise.all([
      fetch(registeredFilter.url).then(res => {
        if (!res.ok) throw new Error(`Filter fetch failed: ${res.status}`);
        return res.json();
      }),
      loadWorkspaceBadges(),
    ])
      .then(([payload, workspaceBadges]) => renderSessionList(listingRows(payload), filter, {workspaceBadges}))
      .catch(err => console.error('Filter fetch failed:', err));
    return;
  }
  fetch(registeredFilter.url)
    .then(res => {
      if (!res.ok) throw new Error(`Filter fetch failed: ${res.status}`);
      return res.json();
    })
    .then(payload => renderSessionList(listingRows(payload), filter))
    .catch(err => console.error('Filter fetch failed:', err));
}

// A listing's rows: the Workspace fetch answers with the page's column form, which expandSessionColumns restores,
// and a filter that answers with a row array needs no expansion.
function listingRows(payload) {
  return Array.isArray(payload) ? payload : expandSessionColumns(payload);
}

// ---------------------------------------------------------------------------
// Workspace row refresh
// ---------------------------------------------------------------------------
// A task-tree change names the session whose row changed. The Workspace fetches those rows alone and merges them into
// the list it painted. Ids that arrive while a fetch is in flight go into the next fetch, and a hidden tab fetches once
// it is visible.
const ROW_REFRESH_BATCH = 50;
const pendingRowIds = new Set();
let rowRefreshInFlight = false;

function queueRowRefresh(sessionId) {
  pendingRowIds.add(sessionId);
  deferWhileHidden('row-refresh', drainRowRefresh);
}

function drainRowRefresh() {
  if (rowRefreshInFlight) return;
  if (!workspaceRowsShown()) {
    pendingRowIds.clear();
    return;
  }
  if (pendingRowIds.size === 0) return;
  const batch = Array.from(pendingRowIds).slice(0, ROW_REFRESH_BATCH);
  batch.forEach(id => pendingRowIds.delete(id));
  rowRefreshInFlight = true;
  fetch('/api/sessions/?compact=1&ids=' + batch.map(encodeURIComponent).join(','))
    .then(res => {
      if (!res.ok) throw new Error(`Row refresh failed: ${res.status}`);
      return res.json();
    })
    .then(payload => {
      // A view the reader left while the fetch was in flight takes no rows: its own load paints it.
      if (workspaceRowsShown()) {
        mergeRowsIntoRenderedList(expandSessionColumns(payload.sessions), payload.gone);
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

// The base64url alphabet: a UUID session id travels as its 16 bytes in this alphabet.
const UUID_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

// The bytes a base64url text carries.
function decodeBase64Url(text) {
  let buffer = 0;
  let bits = 0;
  const bytes = [];
  for (const character of text) {
    const index = UUID_ALPHABET.indexOf(character);
    if (index < 0) throw new Error('Encoded session id character ' + character);
    buffer = (buffer << 6) | index;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((buffer >> bits) & 0xff);
    }
  }
  return bytes;
}

// The session id 16 bytes spell in the canonical UUID form the store and the routes name it by.
function sessionIdOf(bytes) {
  if (bytes.length !== 16) throw new Error('Encoded session id of ' + bytes.length + ' bytes');
  const hex = bytes.map(byte => byte.toString(16).padStart(2, '0')).join('');
  return hex.slice(0, 8) + '-' + hex.slice(8, 12) + '-' + hex.slice(12, 16) + '-' +
    hex.slice(16, 20) + '-' + hex.slice(20);
}

// The session id one `uuid-b64` cell carries: 22 base64url characters that encode its 16 bytes, or a `~` id as
// its text.
function expandSessionId(cell) {
  if (cell[0] === '~') return cell.slice(1);
  if (cell.length !== 22) throw new Error('Encoded session id of ' + cell.length + ' characters');
  return sessionIdOf(decodeBase64Url(cell));
}

// The session ids one `uuid-b64` column carries. A string holds every id's 16 bytes in row order. An array holds one
// cell per row, which is how a home whose ids are not all canonical UUIDs travels.
function expandSessionIds(values, count) {
  if (Array.isArray(values)) return values.map(value => (value == null ? null : expandSessionId(value)));
  const bytes = decodeBase64Url(values);
  if (bytes.length !== count * 16) throw new Error('Encoded session id blob of ' + bytes.length + ' bytes');
  return Array.from({length: count}, (_, index) => sessionIdOf(bytes.slice(index * 16, (index + 1) * 16)));
}

// The page carries the sidebar rows as columns: one array per field, in row order, with null where a row leaves the
// field out. The page declares under `$encoded` the columns whose cells are not their field's value. A `uuid-b64`
// column holds the session ids' 16 bytes in base64url. A `delta` column holds the first value as it stands and every
// later value as its difference from the row before. Expanding the columns once here gives every sidebar view
// the row objects it reads.
function expandSessionColumns(columns) {
  const encoding = columns.$encoded || {};
  const fields = Object.keys(columns).filter(field => field !== '$encoded');
  const first = fields.length ? columns[fields[0]] : [];
  // The `id` column's string form carries 16 bytes per row, and base64url carries 3 bytes in 4 characters.
  const count = Array.isArray(first) ? first.length : Math.floor(first.length * 3 / 4 / 16);
  const rows = Array.from({length: count}, () => ({}));
  fields.forEach(field => {
    const values = columns[field];
    const kind = encoding[field];
    if (kind === 'uuid-b64') {
      expandSessionIds(values, count).forEach((id, index) => {
        if (id != null) rows[index][field] = id;
      });
      return;
    }
    let running = 0;
    for (let i = 0; i < count; i++) {
      let value = values[i];
      if (value == null) continue;
      if (kind === 'delta') {
        running += value;
        value = running;
      }
      rows[i][field] = value;
    }
  });
  return rows;
}

function restoreSidebarFromUrl() {
  renderSidebarFilterPills();
  const initialSessions = expandSessionColumns(INITIAL_SESSIONS);
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
      renderSessionList(initialSessions, 'all');
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
  expandSessionColumns,
  handleSidebarSearch,
  toggleSessionStar,
  queueRowRefresh,
  refreshWorkspaceBadges,
};
Sidebar.wire(GLOBALS);

})();
