// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Tracks server-reported unread state per session so we can restore the
// unread dot after the spinner hides.
const sessionUnread = {};

function setSessionSpinner(sid, visible) {
  const spinner = document.getElementById('spinner-' + sid);
  if (spinner) spinner.classList.toggle('hidden', !visible);
  // While spinner is visible, suppress the unread dot; restore when hidden.
  const dot = document.getElementById('unread-' + sid);
  if (dot) dot.classList.toggle('hidden', visible || !sessionUnread[sid]);
}

function updateSpinner() {
  var anyRunning = Array.from(document.querySelectorAll('[id^="thread-status-"]'))
    .some(function(el) { return el.textContent === 'running'; });
  setSessionSpinner(SESSION_ID, masterThinking || anyRunning);
}

function updateWorkersTabBadge() {
  var btn = document.getElementById('btn-workers');
  if (!btn) return;
  var count = document.querySelectorAll('[id^="thread-dot-"]').length;
  var badge = btn.querySelector('span');
  if (count > 0) {
    if (!badge) {
      badge = document.createElement('span');
      badge.className = 'ml-1 text-xs bg-slate-600 px-1.5 py-0.5 rounded-full';
      btn.appendChild(badge);
    }
    badge.textContent = count;
  } else if (badge) {
    badge.remove();
  }
}

// ---------------------------------------------------------------------------
// Workers tab live updates
// ---------------------------------------------------------------------------
const STATUS_DOT_COLORS = {
  running: 'bg-blue-500', completed: 'bg-green-500',
  failed: 'bg-red-500', cancelled: 'bg-slate-500', idle: 'bg-slate-500',
};

function updateWorkerStatus(threadId, status) {
  const dot = document.getElementById('thread-dot-' + threadId);
  const text = document.getElementById('thread-status-' + threadId);
  if (!dot || !text) return;
  dot.className = 'w-2 h-2 rounded-full flex-shrink-0 ' + (STATUS_DOT_COLORS[status] || 'bg-slate-500');
  text.textContent = status;
}

function addWorkerCard(threadId, description) {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  // Remove placeholder if present
  document.getElementById('no-workers-placeholder')?.remove();
  // Don't add duplicate
  if (document.getElementById('thread-dot-' + threadId)) return;
  const card = document.createElement('div');
  card.className = 'bg-slate-800 rounded-xl border border-slate-700 overflow-hidden';
  card.innerHTML = `
    <div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750"
         onclick="toggleThreadDetail('${threadId}', '${SESSION_ID}')">
      <span id="thread-dot-${threadId}" class="w-2 h-2 rounded-full flex-shrink-0 bg-blue-500 animate-pulse"></span>
      <div class="flex-1 min-w-0">
        <p class="text-sm truncate">${escapeHtml(description)}</p>
        <p id="thread-status-${threadId}" class="text-xs text-slate-500">running</p>
      </div>
      <svg class="w-4 h-4 text-slate-500 transition-transform thread-chevron" id="chevron-${threadId}"
           fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
      </svg>
    </div>
    <div id="thread-detail-${threadId}" class="hidden border-t border-slate-700">
      <div id="thread-events-${threadId}" class="p-4 max-h-96 overflow-y-auto">
        <p class="text-xs text-slate-500">Loading events...</p>
      </div>
    </div>`;
  container.prepend(card);
  updateWorkersTabBadge();
}

// ---------------------------------------------------------------------------
// Thinking indicator
// ---------------------------------------------------------------------------
let thinkingInterval = null;
let thinkingStart = null;

function startThinking() {
  masterThinking = true;
  thinkingStart = Date.now();
  document.getElementById('thinking').classList.remove('hidden');
  updateThinkingTime();
  thinkingInterval = setInterval(updateThinkingTime, 1000);
  document.getElementById('send-btn').disabled = true;
  document.getElementById('send-btn').classList.add('opacity-50');
  setSessionSpinner(SESSION_ID, true);
  const sel = document.getElementById('backend-select');
  if (sel) sel.disabled = true;
}

function stopThinking() {
  masterThinking = false;
  document.getElementById('thinking').classList.add('hidden');
  if (thinkingInterval) { clearInterval(thinkingInterval); thinkingInterval = null; }
  thinkingStart = null;
  document.getElementById('send-btn').disabled = false;
  document.getElementById('send-btn').classList.remove('opacity-50');
  const sel = document.getElementById('backend-select');
  if (sel) sel.disabled = false;
  updateSpinner();
}

function updateThinkingTime() {
  if (!thinkingStart) return;
  const secs = Math.floor((Date.now() - thinkingStart) / 1000);
  document.getElementById('thinking-time').textContent = secs + 's';
}

async function cancelMaster() {
  try {
    await fetch(`/api/chat/${SESSION_ID}/cancel`, { method: 'POST' });
  } catch (err) {
    console.error('Cancel master failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------
async function createSession() {
  try {
    const res = await fetch('/api/sessions/', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({}),
    });
    const data = await res.json();
    location.href = '/?session=' + data.id;
  } catch (err) {
    console.error('Create session failed:', err);
  }
}

function markSessionRead(id) {
  // Optimistically hide the unread dot
  const dot = document.getElementById('unread-' + id);
  if (dot) dot.classList.add('hidden');
  // Fire-and-forget API call
  fetch(`/api/sessions/${id}/read`, { method: 'POST' }).catch(() => {});
}

async function archiveSession(id) {
  try {
    await fetch(`/api/sessions/${id}`, { method: 'DELETE' });
    if (SESSION_ID === id) {
      location.href = '/?session=';
    } else {
      document.getElementById('session-' + id)?.remove();
    }
  } catch (err) {
    console.error('Archive failed:', err);
  }
}

async function unarchiveSession(id) {
  try {
    await fetch(`/api/sessions/${id}/unarchive`, { method: 'POST' });
    document.getElementById('session-' + id)?.remove();
  } catch (err) {
    console.error('Unarchive failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Sidebar filter & star
// ---------------------------------------------------------------------------
let currentFilter = 'all';

function switchSidebarFilter(filter) {
  currentFilter = filter;
  // Update pill styles
  document.querySelectorAll('.filter-pill').forEach(btn => {
    btn.classList.remove('bg-blue-600/20', 'text-blue-300');
    btn.classList.add('text-slate-400');
  });
  const active = document.getElementById('filter-' + filter);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('text-slate-400');
  }
  // Fetch sessions for this filter
  const urls = { all: '/api/sessions/', starred: '/api/sessions/starred', archived: '/api/sessions/archived' };
  fetch(urls[filter])
    .then(res => res.json())
    .then(sessions => renderSessionList(sessions, filter))
    .catch(err => console.error('Filter fetch failed:', err));
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
      document.getElementById('session-' + id)?.remove();
    }
  } catch (err) {
    console.error('Star toggle failed:', err);
  }
}

function renderSessionList(sessions, filter) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    const labels = { all: 'No sessions yet', starred: 'No starred sessions', archived: 'No archived sessions' };
    nav.innerHTML = `<p class="text-slate-500 text-sm px-3 py-2">${labels[filter]}</p>`;
    return;
  }
  const starSvg = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;
  nav.innerHTML = sessions.map(s => {
    const isActive = SESSION_ID === s.id;
    const activeClass = isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
    const starFill = s.starred ? 'currentColor' : 'none';
    const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
    const activeBtnClass = isActive ? '!opacity-100' : '';
    const timeStr = s.updated_at ? relativeTime(s.updated_at) : '';
    const timeIso = s.updated_at || '';
    // Action buttons differ by filter
    let actions = '';
    if (filter === 'archived') {
      actions = `
        <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
                class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
          <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); unarchiveSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-green-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Unarchive">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5-5m0 0l5 5m-5-5v12"/></svg>
        </button>`;
    } else {
      actions = `
        <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
                class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
          <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-blue-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Rename">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
        </button>`;
    }
    return `<a href="/?session=${s.id}&filter=${filter}"
       class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}"
       ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
       onclick="markSessionRead('${s.id}')"
       id="session-${s.id}">
      <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${s.has_running_tasks ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
      <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && !s.has_running_tasks ? '' : 'hidden'}"></span>
      ${s.scheduled_task ? `<svg class="w-3 h-3 text-blue-400 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task)}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>` : ''}
      <span class="flex-1 min-w-0">
        <span class="truncate block session-name">${escapeHtml(s.name)}</span>
        <span class="block text-xs text-slate-500 session-time" data-time="${timeIso}">${timeStr}</span>
        <span class="block text-xs text-slate-600 session-usage hidden" id="sidebar-usage-${s.id}"></span>
      </span>
      ${actions}
    </a>`;
  }).join('');
  updateRelativeTimes();
}

// Inline rename
let renameSessionId = null;

function startRename(e, id, currentName) {
  e.preventDefault();
  e.stopPropagation();
  renameSessionId = id;
  const link = document.getElementById('session-' + id);
  const rect = link.getBoundingClientRect();
  const input = document.getElementById('rename-input');
  input.style.top = rect.top + 'px';
  input.style.left = rect.left + 'px';
  input.style.width = rect.width + 'px';
  input.value = currentName;
  input.classList.remove('hidden');
  input.focus();
  input.select();
}

function handleRenameKey(e) {
  if (e.key === 'Enter') { e.preventDefault(); commitRename(); }
  if (e.key === 'Escape') { cancelRename(); }
}

async function commitRename() {
  const input = document.getElementById('rename-input');
  if (input.classList.contains('hidden')) return;
  const newName = input.value.trim();
  input.classList.add('hidden');
  if (!newName || !renameSessionId) return;

  try {
    await fetch(`/api/sessions/${renameSessionId}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: newName }),
    });
    // Update DOM — sidebar and header
    const link = document.getElementById('session-' + renameSessionId);
    if (link) link.querySelector('.session-name').textContent = newName;
    const header = document.getElementById('header-session-name');
    if (header && renameSessionId === SESSION_ID) header.textContent = newName;
  } catch (err) {
    console.error('Rename failed:', err);
  }
  renameSessionId = null;
}

function cancelRename() {
  document.getElementById('rename-input').classList.add('hidden');
  renameSessionId = null;
}

// ---------------------------------------------------------------------------
// Sidebar resize
// ---------------------------------------------------------------------------
function initSidebarResize() {
  const sidebar = document.getElementById('sidebar');
  const handle = document.getElementById('resize-handle');
  const saved = localStorage.getItem('sidebar-width');
  if (saved) sidebar.style.width = saved + 'px';

  let startX, startW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    startW = sidebar.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');

    function onMove(e) {
      const w = Math.min(Math.max(startW + e.clientX - startX, 200), 600);
      sidebar.style.width = w + 'px';
    }
    function onUp() {
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      localStorage.setItem('sidebar-width', sidebar.offsetWidth);
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}
