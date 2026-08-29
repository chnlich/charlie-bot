(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Sidebar filter registry
// ---------------------------------------------------------------------------
const sidebarFilters = [];
const sidebarFiltersByName = {};

function registerSidebarFilter(filter) {
  if (!filter || !filter.name || !filter.label || !filter.url) throw new Error('Invalid sidebar filter registration');
  if (sidebarFiltersByName[filter.name]) throw new Error('Duplicate sidebar filter: ' + filter.name);
  const normalized = {
    name: filter.name,
    label: filter.label,
    url: filter.url,
    restoreFromUrl: filter.restoreFromUrl !== false,
  };
  sidebarFilters.push(normalized);
  sidebarFiltersByName[normalized.name] = normalized;
}

registerSidebarFilter({name: 'all', label: 'All', url: '/api/sessions/', restoreFromUrl: false});
registerSidebarFilter({name: 'starred', label: 'Starred', url: '/api/sessions/starred'});
registerSidebarFilter({name: 'archived', label: 'Archived', url: '/api/sessions/archived'});
registerSidebarFilter({name: 'scheduled', label: 'Scheduled', url: '/api/sessions/scheduled'});

function getSidebarFilter(filter) {
  return sidebarFiltersByName[filter] || null;
}

function getRestorableSidebarFilters() {
  return sidebarFilters.filter(filter => filter.restoreFromUrl).map(filter => filter.name);
}

function renderSidebarFilterPills() {
  const container = document.getElementById('sidebar-filter-pills');
  if (!container) return;
  const addBtn = document.getElementById('cron-add-btn');
  const buttons = sidebarFilters.map(filter => {
    const active = filter.name === currentFilter;
    const cls = active
      ? 'filter-pill px-2.5 py-1 text-xs rounded-full font-medium transition-colors bg-blue-600/20 text-blue-300'
      : 'filter-pill px-2.5 py-1 text-xs rounded-full font-medium transition-colors text-slate-400 hover:text-slate-200';
    return `<button onclick="enterSidebarFilter('${filter.name}')" id="filter-${filter.name}" class="${cls}">${filter.label}</button>`;
  }).join('');
  container.innerHTML = buttons;
  if (addBtn) container.appendChild(addBtn);
}

function getCurrentSidebarViewRequest() {
  const searchInput = document.getElementById('sidebar-search');
  const query = searchInput ? searchInput.value.trim() : '';
  if (query) {
    return {
      filter: 'search',
      url: '/api/sessions/search?q=' + encodeURIComponent(query),
    };
  }
  const filter = getSidebarFilter(currentFilter);
  if (!filter) throw new Error('Unknown sidebar filter: ' + currentFilter);
  return {filter: currentFilter, url: filter.url};
}

async function fetchSidebarSessionsForCurrentView() {
  const view = getCurrentSidebarViewRequest();
  const res = await fetch(view.url);
  if (!res.ok) throw new Error(`Fetch sessions failed: ${res.status}`);
  return {
    filter: view.filter,
    sessions: await res.json(),
  };
}

async function refreshSidebarAfterSessionRemoval(sessionId) {
  const {filter, sessions} = await fetchSidebarSessionsForCurrentView();
  const visibleSessions = sessions.filter(s => s.id !== sessionId);
  delete sessionUnread[sessionId];
  renderSessionList(visibleSessions, filter);

  if (SESSION_ID !== sessionId) {
    updateSidebarHighlight(SESSION_ID);
    return;
  }

  const nextSession = visibleSessions[0] || null;
  if (nextSession) {
    await switchSession(nextSession.id);
  } else {
    renderNoActiveSessionView();
  }
}

async function archiveSession(id) {
  try {
    const res = await fetch(`/api/sessions/${id}`, { method: 'DELETE' });
    if (!res.ok) throw new Error(`Archive failed: ${res.status}`);
    await refreshSidebarAfterSessionRemoval(id);
  } catch (err) {
    console.error('Archive failed:', err);
  }
}

async function deleteSessionPermanently(sessionId) {
  const res = await fetch(`/api/sessions/${sessionId}/permanent`, { method: 'DELETE' });
  if (!res.ok) throw new Error(`Permanent delete failed: ${res.status}`);
  await refreshSidebarAfterSessionRemoval(sessionId);
}

async function unarchiveSession(id) {
  try {
    await fetch(`/api/sessions/${id}/unarchive`, { method: 'POST' });
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Unarchive failed:', err);
  }
}

async function stopActiveTui() {
  if (!SESSION_ID) return;
  if (!confirm('Stop the claude process for this session? You can reopen to resume.')) return;
  const sessionId = SESSION_ID;
  try {
    const res = await fetch(`/api/sessions/${sessionId}/tui/stop`, { method: 'POST' });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(body.detail || `HTTP ${res.status}`);
    }
    const data = await res.json();
    if (data.stopped !== true) throw new Error('Stop endpoint did not return stopped=true');
    globalThis.TuiStatusMap[sessionId] = {running: false, busy: false};
    refreshTuiDots();
    if (globalThis.TuiSession && globalThis.TuiSession.showStoppedBanner) {
      globalThis.TuiSession.showStoppedBanner();
    }
  } catch (err) {
    showToast('Stop Claude failed: ' + err.message, true);
    console.error('Stop Claude failed:', err);
  }
}

let deleteConfirmKeyHandler = null;

function closeDeleteConfirmModal() {
  document.getElementById('delete-confirm-overlay')?.remove();
  if (deleteConfirmKeyHandler) {
    document.removeEventListener('keydown', deleteConfirmKeyHandler);
    deleteConfirmKeyHandler = null;
  }
}

function confirmDeletePermanently(sessionId) {
  closeDeleteConfirmModal();

  const overlay = document.createElement('div');
  overlay.id = 'delete-confirm-overlay';
  overlay.className = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';
  overlay.innerHTML = `
    <div class="bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72 text-center"
         onclick="event.stopPropagation()">
      <svg class="w-8 h-8 text-red-400 mx-auto mb-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2"
              d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/>
      </svg>
      <p class="text-sm font-semibold text-slate-200 mb-1">Delete permanently?</p>
      <p class="text-xs text-slate-400 mb-4">This will permanently delete this session and all its data. This cannot be undone.</p>
      <div class="flex gap-2 justify-center">
        <button id="confirm-cancel-btn" class="px-3 py-1.5 text-xs rounded-lg bg-slate-700 hover:bg-slate-600 text-slate-300 transition-colors">Cancel</button>
        <button id="confirm-delete-btn" class="px-3 py-1.5 text-xs rounded-lg bg-red-600 hover:bg-red-500 text-white transition-colors">Delete</button>
      </div>
    </div>`;

  overlay.querySelector('#confirm-cancel-btn').addEventListener('click', closeDeleteConfirmModal);
  overlay.querySelector('#confirm-delete-btn').addEventListener('click', async () => {
    closeDeleteConfirmModal();
    try {
      await deleteSessionPermanently(sessionId);
    } catch (err) {
      console.error('Permanent delete failed:', err);
    }
  });
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay) closeDeleteConfirmModal();
  });
  deleteConfirmKeyHandler = (e) => {
    if (e.key === 'Escape') closeDeleteConfirmModal();
  };
  document.addEventListener('keydown', deleteConfirmKeyHandler);

  document.body.appendChild(overlay);
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
  const addBtn = document.getElementById('cron-add-btn');
  if (addBtn) addBtn.classList.toggle('hidden', filter !== 'scheduled');
}

function switchSidebarFilter(filter) {
  setSidebarFilterPill(filter);
  // Fetch sessions for this filter
  const registeredFilter = getSidebarFilter(filter);
  if (!registeredFilter) throw new Error('Unknown sidebar filter: ' + filter);
  fetch(registeredFilter.url)
    .then(res => {
      if (!res.ok) throw new Error(`Filter fetch failed: ${res.status}`);
      return res.json();
    })
    .then(sessions => renderSessionList(sessions, filter))
    .catch(err => console.error('Filter fetch failed:', err));
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

function restoreSidebarFromUrl() {
  renderSidebarFilterPills();
  INITIAL_SESSIONS.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
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
      renderSessionList(INITIAL_SESSIONS, 'all');
    }
  }
}

// ---------------------------------------------------------------------------
// Session search
// ---------------------------------------------------------------------------
let searchDebounceTimer = null;

function handleSidebarSearch(query) {
  clearTimeout(searchDebounceTimer);
  const pills = document.querySelector('.filter-pill')?.parentElement;
  const addBtn = document.getElementById('cron-add-btn');
  if (query.trim()) {
    // Hide filter pills while searching
    if (pills) pills.style.display = 'none';
    searchDebounceTimer = setTimeout(() => {
      fetch('/api/sessions/search?q=' + encodeURIComponent(query.trim()))
        .then(res => res.json())
        .then(sessions => renderSessionList(sessions, 'search'))
        .catch(err => console.error('Search failed:', err));
    }, 300);
  } else {
    // Restore filter pills and current filter
    if (pills) pills.style.display = '';
    switchSidebarFilter(currentFilter);
  }
}

async function toggleSessionStar(id, currentlyStarred) {
  const endpoint = currentlyStarred ? 'unstar' : 'star';
  // Optimistic UI update
  const btn = document.getElementById('star-' + id);
  if (btn) {
    const svg = btn.querySelector('svg');
    if (currentlyStarred) {
      svg.setAttribute('fill', 'none');
      btn.classList.remove('text-yellow-400', '!opacity-100');
      btn.classList.add('hover:text-yellow-400');
      btn.setAttribute('onclick', `event.preventDefault(); event.stopPropagation(); toggleSessionStar('${id}', false)`);
    } else {
      svg.setAttribute('fill', 'currentColor');
      btn.classList.add('text-yellow-400', '!opacity-100');
      btn.classList.remove('hover:text-yellow-400');
      btn.setAttribute('onclick', `event.preventDefault(); event.stopPropagation(); toggleSessionStar('${id}', true)`);
    }
  }
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


Object.assign(Sidebar, {
  registerSidebarFilter,
  getSidebarFilter,
  getRestorableSidebarFilters,
  renderSidebarFilterPills,
  archiveSession,
  deleteSessionPermanently,
  unarchiveSession,
  stopActiveTui,
  confirmDeletePermanently,
  setSidebarFilterPill,
  switchSidebarFilter,
  enterSidebarFilter,
  restoreSidebarFromUrl,
  handleSidebarSearch,
  toggleSessionStar,
});
Sidebar.expose([
  'archiveSession',
  'deleteSessionPermanently',
  'unarchiveSession',
  'stopActiveTui',
  'confirmDeletePermanently',
  'setSidebarFilterPill',
  'switchSidebarFilter',
  'enterSidebarFilter',
  'restoreSidebarFromUrl',
  'handleSidebarSearch',
  'toggleSessionStar',
]);

})();
