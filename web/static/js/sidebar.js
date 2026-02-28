// ---------------------------------------------------------------------------
// Sidebar spinner (running tasks indicator)
// ---------------------------------------------------------------------------
// Tracks server-reported unread state per session so we can restore the
// unread dot after the spinner hides.
const sessionUnread = {};

// ---------------------------------------------------------------------------
// SPA-style session switching
// ---------------------------------------------------------------------------
let switchGeneration = 0;
let switching = false;

async function switchSession(sessionId) {
  // Welcome screen — no SPA state to swap, fall back to full load
  if (!SESSION_ID) { location.href = '/?session=' + sessionId; return; }
  // Already on this session
  if (sessionId === SESSION_ID) return;

  switching = true;
  const gen = ++switchGeneration;

  // Save draft for current session
  if (DRAFT_KEY) {
    const v = document.getElementById('msg-input').value;
    if (v) localStorage.setItem(DRAFT_KEY, v);
    else localStorage.removeItem(DRAFT_KEY);
  }

  // Stop thinking indicator
  if (masterThinking) stopThinking();

  // Close WebSocket (suppress auto-reconnect)
  if (ws) { ws.onclose = null; ws.close(); ws = null; }
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }

  // Reset streaming state
  streamBuf = '';
  catchupDone = false;
  pendingUserMsg = false;
  hideStreaming();

  // Fetch session view data
  let data;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/view');
    if (!res.ok) throw new Error(res.status);
    data = await res.json();
  } catch (err) {
    console.error('switchSession fetch failed:', err);
    switching = false;
    location.href = '/?session=' + sessionId;
    return;
  }

  // Discard stale response from rapid clicks
  if (gen !== switchGeneration) return;  // newer switch owns the flag

  // Update globals
  SESSION_ID = sessionId;
  DRAFT_KEY = 'charliebot-draft-' + sessionId;
  THINKING_SINCE = data.session.thinking_since || null;
  eventCursor = data.event_count;
  usageTotalCost = data.usage ? (data.usage.total_cost_usd || 0) : 0;

  // Update URL
  history.pushState({session: sessionId}, '', '/?session=' + sessionId);

  // Render content
  renderSessionView(data);

  // Mark switched-to session as read (WS was closed so broadcast is lost)
  sessionUnread[sessionId] = false;
  const unreadDot = document.getElementById('unread-' + sessionId);
  if (unreadDot) unreadDot.classList.add('hidden');

  // Reconnect WebSocket
  reconnectDelay = 1000;
  connectWS();
  switching = false;

  // Restore draft for new session
  const draft = localStorage.getItem(DRAFT_KEY);
  const inp = document.getElementById('msg-input');
  if (inp) { inp.value = draft || ''; autoResize(inp); }

  // Resume thinking if session was mid-thought
  if (THINKING_SINCE) {
    thinkingStart = new Date(THINKING_SINCE).getTime();
    startThinking();
  }

  updateSpinner();
  updateSidebarHighlight(sessionId);

  // Reset lazy-load state
  _backlogLoaded = false;
  loadedThreads.clear();
}

function renderSessionView(data) {
  const session = data.session;
  const messages = data.messages;

  // Update header
  const headerName = document.getElementById('header-session-name');
  if (headerName) {
    headerName.textContent = session.name;
    headerName.setAttribute('onclick', "startRename(event, '" + session.id + "', '" + escapeHtml(session.name).replace(/'/g, "\\'") + "')");
  }

  // Update events viewer link
  const evLink = document.querySelector('a[href*="/events"]');
  if (evLink) evLink.href = '/sessions/' + session.id + '/events';

  // Update usage
  renderUsageFromData(data.usage);

  // Update backend selector
  const sel = document.getElementById('backend-select');
  if (sel) { sel.value = data.active_backend; sel.dataset.current = data.active_backend; }

  // Build message HTML
  const container = document.getElementById('messages');
  if (!container) return;
  const streamEl = document.getElementById('streaming-msg');
  const streamHtml = streamEl ? streamEl.outerHTML : '';

  const parts = messages.map(msg => {
    if (msg.role === 'user') {
      const voiceSpan = msg.is_voice ? '<span class="text-xs text-blue-200 block mb-1">&#127908; Voice</span>' : '';
      return '<div class="flex justify-end"><div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">'
        + voiceSpan + '<div class="whitespace-pre-wrap">' + escapeHtml(msg.content) + '</div></div></div>';
    }
    if (msg.role === 'assistant') {
      return '<div class="flex justify-start"><div class="max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm prose-msg" data-md>'
        + escapeHtml(msg.content) + '</div></div>';
    }
    if (msg.role === 'system') {
      return '<div class="flex justify-center"><div class="bg-slate-700/50 text-slate-400 text-xs px-3 py-1.5 rounded-full max-w-[85%] overflow-hidden truncate">'
        + escapeHtml(msg.content) + '</div></div>';
    }
    if (msg.role === 'worker_summary') {
      const escaped = escapeHtml(msg.full_content || '').replace(/"/g, '&quot;');
      return '<div class="flex justify-start"><div class="max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300 prose-msg cursor-pointer"'
        + ' data-md data-full="' + escaped + '"'
        + ' onclick="showTextModal(\'Worker Result\', this.dataset.full)">'
        + escapeHtml(msg.content) + '</div></div>';
    }
    if (msg.role === 'plan') {
      return '<div class="flex justify-start"><div class="max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm prose-msg">'
        + '<div class="flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2">'
        + '<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4"/></svg>'
        + 'Plan</div>'
        + '<div data-md>' + escapeHtml(msg.content) + '</div></div></div>';
    }
    if (msg.role === 'separator') {
      const timeStr = msg.thinking_seconds != null ? ' &middot; ' + msg.thinking_seconds + 's' : '';
      return '<div class="flex items-center gap-3 py-2 px-4 separator-line group/sep">'
        + '<div class="flex-1 border-t border-slate-600/40"></div>'
        + '<span class="text-xs text-slate-500 whitespace-nowrap">response complete' + timeStr + '</span>'
        + '<button onclick="rewindSession(\'' + session.id + '\', ' + msg.event_index + ')"'
        + ' class="opacity-0 group-hover/sep:opacity-100 p-0.5 text-slate-500 hover:text-blue-400 transition-opacity" title="Rewind to here">'
        + '<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12.066 11.2a1 1 0 000 1.6l5.334 4A1 1 0 0019 16V8a1 1 0 00-1.6-.8l-5.333 4zM4.066 11.2a1 1 0 000 1.6l5.334 4A1 1 0 0011 16V8a1 1 0 00-1.6-.8l-5.334 4z"/></svg>'
        + '</button>'
        + '<div class="flex-1 border-t border-slate-600/40"></div></div>';
    }
    return '';
  });

  container.innerHTML = parts.join('') + streamHtml;

  // Parse markdown
  container.querySelectorAll('[data-md]').forEach(el => { el.innerHTML = marked.parse(el.textContent); });

  // Scroll to bottom
  container.scrollTop = container.scrollHeight;

  // Render workers tab
  renderWorkersTab(data.threads, session.id);

  // Update workers badge
  const btn = document.getElementById('btn-workers');
  if (btn) {
    const badge = btn.querySelector('span');
    if (data.threads.length > 0) {
      if (badge) { badge.textContent = data.threads.length; }
      else {
        const s = document.createElement('span');
        s.className = 'ml-1 text-xs bg-slate-600 px-1.5 py-0.5 rounded-full';
        s.textContent = data.threads.length;
        btn.appendChild(s);
      }
    } else if (badge) {
      badge.remove();
    }
  }

  switchTab('chat');
}

function renderUsageFromData(usage) {
  const indicator = document.getElementById('usage-indicator');
  if (!usage) { if (indicator) indicator.classList.add('hidden'); return; }
  if (indicator) indicator.classList.remove('hidden');

  const contextTokens = usage.context_tokens || 0;
  const contextLimit = usage.context_limit || 200000;
  const pct = contextLimit > 0 ? (contextTokens / contextLimit * 100) : 0;

  const bar = document.getElementById('usage-bar');
  if (bar) {
    bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
    bar.className = 'h-full rounded-full transition-all duration-300 '
      + (pct > 80 ? 'bg-red-500' : pct > 50 ? 'bg-yellow-500' : 'bg-blue-500');
  }
  const text = document.getElementById('usage-text');
  if (text) text.textContent = formatTokens(contextTokens) + ' / ' + formatTokens(contextLimit);
  const cost = document.getElementById('usage-cost');
  if (cost) cost.textContent = '$' + (usage.total_cost_usd || 0).toFixed(2);
}

function renderWorkersTab(threads, sessionId) {
  const container = document.getElementById('tab-workers');
  if (!container) return;

  if (!threads || !threads.length) {
    container.innerHTML = '<div id="no-workers-placeholder" class="flex items-center justify-center h-full text-slate-500 text-sm">No worker threads</div>';
    return;
  }

  const cards = threads.map(t => {
    const statusColors = {running: 'bg-blue-500', completed: 'bg-green-500', failed: 'bg-red-500', cancelled: 'bg-slate-500', idle: 'bg-slate-500'};
    const dotColor = statusColors[t.status] || 'bg-slate-500';
    const pulse = t.status === 'running' ? ' animate-pulse' : '';
    const created = new Date(t.created_at);
    const mm = String(created.getMonth() + 1).padStart(2, '0');
    const dd = String(created.getDate()).padStart(2, '0');
    const hh = String(created.getHours()).padStart(2, '0');
    const mi = String(created.getMinutes()).padStart(2, '0');
    const timeStr = mm + '/' + dd + ' ' + hh + ':' + mi;
    let duration = '';
    if (t.completed_at) {
      const secs = Math.floor((new Date(t.completed_at) - created) / 1000);
      duration = ' &middot; ' + Math.floor(secs / 60) + 'm' + (secs % 60) + 's';
    }
    const cancelBtn = t.status === 'running'
      ? '<button id="cancel-btn-' + t.id + '" onclick="event.stopPropagation(); cancelThread(\'' + t.id + '\', \'' + sessionId + '\')" class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel"><svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/></svg></button>'
      : '';
    return '<div class="bg-slate-800 rounded-xl border border-slate-700 overflow-hidden">'
      + '<div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750" onclick="toggleThreadDetail(\'' + t.id + '\', \'' + sessionId + '\')">'
      + '<span id="thread-dot-' + t.id + '" class="w-2 h-2 rounded-full flex-shrink-0 ' + dotColor + pulse + '"></span>'
      + '<div class="flex-1 min-w-0">'
      + '<p class="text-sm truncate cursor-pointer hover:text-blue-400 transition-colors" title="Click to view full description" onclick="event.stopPropagation(); showTextModal(\'Worker Description\', this.dataset.full)" data-full="' + escapeHtml(t.description || '').replace(/"/g, '&quot;') + '">' + escapeHtml(t.description || '') + '</p>'
      + '<p id="thread-status-' + t.id + '" class="text-xs text-slate-500">' + (t.status || 'idle') + ' &middot; ' + timeStr + duration + '</p>'
      + '</div>'
      + cancelBtn
      + '<svg class="w-4 h-4 text-slate-500 transition-transform thread-chevron" id="chevron-' + t.id + '" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/></svg>'
      + '</div>'
      + '<div id="thread-detail-' + t.id + '" class="hidden border-t border-slate-700">'
      + '<div id="thread-events-' + t.id + '" class="p-4 max-h-96 overflow-y-auto"><p class="text-xs text-slate-500">Loading events...</p></div>'
      + '</div></div>';
  });

  container.innerHTML = cards.join('');
}

function updateSidebarHighlight(newSessionId) {
  document.querySelectorAll('[id^="session-"]').forEach(el => {
    if (!el.id.startsWith('session-')) return;
    el.classList.remove('bg-blue-600/20', 'text-blue-300');
    el.classList.add('hover:bg-slate-700/50', 'text-slate-300');
    el.querySelectorAll('.group-hover\\:opacity-100').forEach(btn => {
      if (btn.classList.contains('star-btn') && btn.classList.contains('text-yellow-400')) return;
      btn.classList.remove('!opacity-100');
    });
  });
  const active = document.getElementById('session-' + newSessionId);
  if (active) {
    active.classList.add('bg-blue-600/20', 'text-blue-300');
    active.classList.remove('hover:bg-slate-700/50', 'text-slate-300');
    active.querySelectorAll('.group-hover\\:opacity-100').forEach(btn => {
      btn.classList.add('!opacity-100');
    });
  }
}

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
  // preserve timestamp portion if present
  const cur = text.textContent;
  const dotIdx = cur.indexOf(' · ');
  const suffix = dotIdx !== -1 ? cur.substring(dotIdx) : '';
  text.textContent = status + suffix;
  const cancelBtn = document.getElementById('cancel-btn-' + threadId);
  if (cancelBtn) cancelBtn.style.display = status === 'running' ? '' : 'none';
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
  const nowStr = (() => {
    const d = new Date();
    const mm = String(d.getMonth() + 1).padStart(2, '0');
    const dd = String(d.getDate()).padStart(2, '0');
    const hh = String(d.getHours()).padStart(2, '0');
    const mi = String(d.getMinutes()).padStart(2, '0');
    return `${mm}/${dd} ${hh}:${mi}`;
  })();
  card.innerHTML = `
    <div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750"
         onclick="toggleThreadDetail('${threadId}', '${SESSION_ID}')">
      <span id="thread-dot-${threadId}" class="w-2 h-2 rounded-full flex-shrink-0 bg-blue-500 animate-pulse"></span>
      <div class="flex-1 min-w-0">
        <p class="text-sm truncate cursor-pointer hover:text-blue-400 transition-colors" title="Click to view full description" onclick="event.stopPropagation(); showTextModal('Worker Description', this.dataset.full)" data-full="${escapeHtml(description)}">${escapeHtml(description)}</p>
        <p id="thread-status-${threadId}" class="text-xs text-slate-500">running · ${nowStr}</p>
      </div>
      <button id="cancel-btn-${threadId}" onclick="event.stopPropagation(); cancelThread('${threadId}', '${SESSION_ID}')"
              class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors" title="Cancel">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/>
        </svg>
      </button>
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
  thinkingStart = thinkingStart || Date.now();
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

async function waitSession(id) {
  try {
    await fetch(`/api/sessions/${id}/wait`, { method: 'POST' });
    if (SESSION_ID === id) {
      location.href = '/?session=';
    } else {
      document.getElementById('session-' + id)?.remove();
    }
  } catch (err) {
    console.error('Wait failed:', err);
  }
}

async function unwaitSession(id) {
  try {
    await fetch(`/api/sessions/${id}/unwait`, { method: 'POST' });
    document.getElementById('session-' + id)?.remove();
  } catch (err) {
    console.error('Unwait failed:', err);
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
  const urls = {
    all: '/api/sessions/',
    starred: '/api/sessions/starred',
    archived: '/api/sessions/archived',
    waiting: '/api/sessions/waiting',
    scheduled: '/api/sessions/scheduled',
  };
  const addBtn = document.getElementById('cron-add-btn');
  if (addBtn) addBtn.classList.toggle('hidden', filter !== 'scheduled');
  fetch(urls[filter])
    .then(res => res.json())
    .then(sessions => renderSessionList(sessions, filter))
    .catch(err => console.error('Filter fetch failed:', err));
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
      document.getElementById('session-' + id)?.remove();
    }
  } catch (err) {
    console.error('Star toggle failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Grouped scheduled task rendering
// ---------------------------------------------------------------------------
function renderScheduledSessionItem(s) {
  const isActive = SESSION_ID === s.id;
  const activeClass = isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
  const starFill = s.starred ? 'currentColor' : 'none';
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const starSvg = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;
  const gearBtn = s.scheduled_task ? `
    <button onclick="event.preventDefault(); event.stopPropagation(); openCronEditor('${escapeHtml(s.scheduled_task)}')"
            class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Edit task config">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><circle cx="12" cy="12" r="3"/></svg>
    </button>` : '';
  const actions = `
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
    </button>
    ${gearBtn}`;
  return `<a href="/?session=${s.id}&filter=scheduled"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}"
     ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
     onclick="event.preventDefault(); switchSession('${s.id}')"
     id="session-${s.id}">
    <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${s.has_running_tasks ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
    <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && !s.has_running_tasks ? '' : 'hidden'}"></span>
    <svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task || '')}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : ''}
      ${s.last_run_status ? `<span class="block text-xs ${s.last_run_status === 'success' ? 'text-green-400' : s.last_run_status === 'running' ? 'text-yellow-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400')}">Last: ${escapeHtml(s.last_run_status)}${s.last_run_status === 'failed' && s.schedule_allow_failure ? ' (review needed)' : ''}</span>` : ''}
      <span class="block text-xs text-slate-600 session-usage hidden" id="sidebar-usage-${s.id}"></span>
    </span>
    ${actions}
  </a>`;
}

function renderGroupedScheduledList(sessions) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    nav.innerHTML = '<p class="text-slate-500 text-sm px-3 py-2">No scheduled sessions</p>';
    return;
  }
  // Group by project
  const groups = {};
  sessions.forEach(s => {
    const key = s.schedule_project || '';
    if (!groups[key]) groups[key] = [];
    groups[key].push(s);
  });
  // Sort: named groups alphabetically, '' (no project) last
  const sortedKeys = Object.keys(groups).sort((a, b) => {
    if (a === '') return 1;
    if (b === '') return -1;
    return a.localeCompare(b);
  });
  // Load collapsed state from localStorage (collapsed by default)
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('cron-group-collapsed') || '{}'); } catch (e) {}

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No project)';
    const groupSessions = groups[key];
    const enabledCount = groupSessions.filter(s => s.schedule_enabled !== false).length;
    const totalCount = groupSessions.length;
    const isCollapsed = collapsedState[key] !== false; // collapsed by default
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtml(key);

    html += `<div class="cron-group" data-group-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           onclick="toggleCronGroup('${safeKey}')">
        <svg class="w-3 h-3 text-slate-500 transition-transform cron-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        <span class="text-xs text-slate-500 ml-auto">${enabledCount}/${totalCount} enabled</span>
      </div>
      <div class="cron-group-items ${isCollapsed ? 'hidden' : ''}" data-group-items="${safeKey}">
        ${groupSessions.map(s => renderScheduledSessionItem(s)).join('')}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
}

function toggleCronGroup(key) {
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('cron-group-collapsed') || '{}'); } catch (e) {}
  const wasCollapsed = collapsedState[key] !== false;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem('cron-group-collapsed', JSON.stringify(collapsedState));

  const items = document.querySelector(`[data-group-items="${key}"]`);
  if (items) items.classList.toggle('hidden');
  const group = document.querySelector(`[data-group-key="${key}"]`);
  if (group) {
    const chevron = group.querySelector('.cron-group-chevron');
    if (chevron) chevron.classList.toggle('rotate-90');
  }
}

function renderSessionList(sessions, filter) {
  if (filter === 'scheduled') {
    renderGroupedScheduledList(sessions);
    return;
  }
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    const labels = {
      all: 'No sessions yet',
      starred: 'No starred sessions',
      archived: 'No archived sessions',
      waiting: 'No waiting sessions',
      scheduled: 'No scheduled sessions',
      search: 'No matching sessions',
    };
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
    } else if (filter === 'waiting') {
      actions = `
        <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
                class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
          <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); unwaitSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-green-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Reactivate">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15"/></svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
        </button>`;
    } else {
      const gearBtn = (filter === 'scheduled' && s.scheduled_task) ? `
        <button onclick="event.preventDefault(); event.stopPropagation(); openCronEditor('${escapeHtml(s.scheduled_task)}')"
                class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Edit task config">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><circle cx="12" cy="12" r="3"/></svg>
        </button>` : '';
      actions = `
        <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
                class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
          <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-blue-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Rename">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); waitSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-amber-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Mark waiting">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z"/></svg>
        </button>
        <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
                class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
        </button>
        ${gearBtn}`;
    }
    return `<a href="/?session=${s.id}&filter=${filter}"
       class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}"
       ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
       onclick="event.preventDefault(); switchSession('${s.id}')"
       id="session-${s.id}">
      <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${s.has_running_tasks ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
      <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && !s.has_running_tasks ? '' : 'hidden'}"></span>
      ${s.scheduled_task ? `<svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task)}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>` : ''}
      <span class="flex-1 min-w-0">
        <span class="truncate block session-name">${escapeHtml(s.name)}</span>
        ${filter === 'scheduled' && s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : `<span class="block text-xs text-slate-500 session-time" data-time="${timeIso}">${timeStr}</span>`}
        <span class="block text-xs text-slate-600 session-usage hidden" id="sidebar-usage-${s.id}"></span>
      </span>
      ${actions}
    </a>`;
  }).join('');
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
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

// ---------------------------------------------------------------------------
// Cron task editor modal
// ---------------------------------------------------------------------------
let cronEditMode = null; // 'edit' or 'add'
let cronOriginalName = null;

async function openCronEditor(taskName) {
  let task;
  try {
    const res = await fetch('/api/cron/tasks');
    if (!res.ok) throw new Error(await res.text());
    const tasks = await res.json();
    task = tasks.find(t => t.name === taskName);
  } catch (err) {
    console.error('Failed to load cron tasks:', err);
    alert('Failed to load task: ' + err);
    return;
  }
  if (!task) {
    alert('Task "' + taskName + '" not found');
    return;
  }
  cronEditMode = 'edit';
  cronOriginalName = taskName;
  document.getElementById('cron-modal-title').textContent = 'Edit Scheduled Task';
  document.getElementById('cron-name').value = task.name;
  document.getElementById('cron-name').readOnly = true;
  document.getElementById('cron-expr').value = task.cron || '';
  document.getElementById('cron-prompt').value = task.prompt || '';
  document.getElementById('cron-repo').value = task.repo || '';
  document.getElementById('cron-project').value = task.project || '';
  document.getElementById('cron-timezone').value = task.timezone || 'America/New_York';
  document.getElementById('cron-enabled').checked = task.enabled !== false;
  document.getElementById('cron-delete-btn').classList.remove('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function openCronAdder() {
  cronEditMode = 'add';
  cronOriginalName = null;
  document.getElementById('cron-modal-title').textContent = 'New Scheduled Task';
  document.getElementById('cron-name').value = '';
  document.getElementById('cron-name').readOnly = false;
  document.getElementById('cron-expr').value = '';
  document.getElementById('cron-prompt').value = '';
  document.getElementById('cron-repo').value = '';
  document.getElementById('cron-project').value = '';
  document.getElementById('cron-timezone').value = 'America/New_York';
  document.getElementById('cron-enabled').checked = true;
  document.getElementById('cron-delete-btn').classList.add('hidden');
  document.getElementById('cron-modal').classList.remove('hidden');
}

function closeCronModal() {
  document.getElementById('cron-modal').classList.add('hidden');
}

async function saveCronTask() {
  const name = document.getElementById('cron-name').value.trim();
  const cron = document.getElementById('cron-expr').value.trim();
  const prompt = document.getElementById('cron-prompt').value.trim();
  const repo = document.getElementById('cron-repo').value.trim() || null;
  const project = document.getElementById('cron-project').value.trim() || null;
  const timezone = document.getElementById('cron-timezone').value.trim();
  const enabled = document.getElementById('cron-enabled').checked;

  let res;
  try {
    if (cronEditMode === 'edit') {
      res = await fetch(`/api/cron/tasks/${encodeURIComponent(cronOriginalName)}`, {
        method: 'PUT',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({cron, prompt, repo, project, timezone, enabled}),
      });
    } else {
      res = await fetch('/api/cron/tasks', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({name, cron, prompt, repo, project, timezone, enabled}),
      });
    }
  } catch (err) {
    alert('Failed: ' + err);
    return;
  }
  if (!res.ok) {
    alert('Failed: ' + await res.text());
    return;
  }
  closeCronModal();
  switchSidebarFilter('scheduled');
}

async function deleteCronTask() {
  const name = cronOriginalName;
  if (!confirm(`Delete task "${name}"?`)) return;
  let res;
  try {
    res = await fetch(`/api/cron/tasks/${encodeURIComponent(name)}`, {method: 'DELETE'});
  } catch (err) {
    alert('Failed: ' + err);
    return;
  }
  if (!res.ok) {
    alert('Failed: ' + await res.text());
    return;
  }
  closeCronModal();
  switchSidebarFilter('scheduled');
}

// ---------------------------------------------------------------------------
// Lazy-load sidebar usage badges
// ---------------------------------------------------------------------------
function fetchSidebarUsage() {
  fetch('/api/sessions/usage')
    .then(res => res.json())
    .then(data => {
      for (const [sid, u] of Object.entries(data)) {
        const el = document.getElementById('sidebar-usage-' + sid);
        if (el) {
          const tokens = Math.round((u.context_tokens || 0) / 1000) + 'k';
          const cost = '$' + (u.total_cost_usd || 0).toFixed(2);
          el.textContent = tokens + ' tokens \u00b7 ' + cost;
          el.classList.remove('hidden');
        }
      }
    })
    .catch(err => console.debug('Sidebar usage fetch failed:', err));
}

// ---------------------------------------------------------------------------
// Session rewind
// ---------------------------------------------------------------------------
async function rewindSession(sessionId, eventIndex) {
  if (!confirm('Rewind session to this point? A new session will be created.')) return;
  try {
    const res = await fetch("/api/sessions/" + sessionId + "/rewind", {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({event_index: eventIndex}),
    });
    if (!res.ok) throw new Error(await res.text());
    const data = await res.json();
    location.href = '/?session=' + data.id;
  } catch (err) {
    console.error('Rewind failed:', err);
    alert('Rewind failed: ' + err.message);
  }
}
