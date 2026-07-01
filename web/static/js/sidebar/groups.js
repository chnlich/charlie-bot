(function() {
  const Sidebar = globalThis.Sidebar;

const GROUP_SESSION_PREVIEW_LIMIT = 5;
const SESSION_GROUP_LIMIT_STORAGE_KEY = 'session-group-list-expanded';
const CRON_GROUP_LIMIT_STORAGE_KEY = 'cron-group-list-expanded';
const groupLimitState = {
  [SESSION_GROUP_LIMIT_STORAGE_KEY]: {},
  [CRON_GROUP_LIMIT_STORAGE_KEY]: {},
};

function escapeHtmlAttr(str) {
  return escapeHtml(str).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function loadGroupLimitState(storageKey) {
  if (!Object.keys(groupLimitState[storageKey]).length) {
    const saved = localStorage.getItem(storageKey);
    if (saved) groupLimitState[storageKey] = JSON.parse(saved);
  }
  return groupLimitState[storageKey];
}

function isGroupLimitExpanded(storageKey, key) {
  return loadGroupLimitState(storageKey)[key] === true;
}

function setGroupLimitExpanded(storageKey, key, expanded) {
  const state = loadGroupLimitState(storageKey);
  state[key] = expanded;
  localStorage.setItem(storageKey, JSON.stringify(state));
}

function resetGroupLimitState() {
  groupLimitState[SESSION_GROUP_LIMIT_STORAGE_KEY] = {};
  groupLimitState[CRON_GROUP_LIMIT_STORAGE_KEY] = {};
}

function shouldLimitHideSession(session, index, expanded) {
  if (expanded) return false;
  if (index < GROUP_SESSION_PREVIEW_LIMIT) return false;
  return session.id !== SESSION_ID;
}

function isOverGroupLimitExtra(session, index) {
  return index >= GROUP_SESSION_PREVIEW_LIMIT && session.id !== SESSION_ID;
}

function groupLimitItemOptions(kind, key, session, index, expanded) {
  if (!isOverGroupLimitExtra(session, index)) return {};
  const safeKey = escapeHtmlAttr(key);
  const hiddenClass = shouldLimitHideSession(session, index, expanded) ? ' hidden' : '';
  return {
    extraClass: `${kind}-group-limit-extra${hiddenClass}`,
    extraAttrs: `data-${kind}-group-limit-extra="${safeKey}"`,
  };
}

function renderGroupLimitToggle(kind, key, totalCount, expanded) {
  if (totalCount <= GROUP_SESSION_PREVIEW_LIMIT) return '';
  const safeKey = escapeHtmlAttr(key);
  const label = expanded ? 'Show less' : 'Show all';
  const dataAttr = kind === 'session' ? 'sgroup-limit-toggle-key' : 'cron-limit-toggle-key';
  const handler = kind === 'session' ? 'toggleSessionGroupLimit' : 'toggleCronGroupLimit';
  return `<button type="button"
          class="${kind}-group-limit-toggle w-full text-left px-3 py-1.5 text-xs text-blue-400 hover:text-blue-300 hover:bg-slate-700/30 rounded-lg transition-colors"
          data-${dataAttr}="${safeKey}"
          aria-expanded="${expanded ? 'true' : 'false'}"
          onclick="event.stopPropagation(); ${handler}(this.dataset.${kind === 'session' ? 'sgroupLimitToggleKey' : 'cronLimitToggleKey'})">${label}</button>`;
}

function updateGroupLimitDom(kind, key, expanded) {
  const extraSelector = `.${kind}-group-limit-extra`;
  const toggleSelector = `.${kind}-group-limit-toggle`;
  const extraDatasetKey = `${kind}GroupLimitExtra`;
  const toggleDatasetKey = kind === 'session' ? 'sgroupLimitToggleKey' : 'cronLimitToggleKey';
  document.querySelectorAll(extraSelector).forEach(el => {
    if (el.dataset[extraDatasetKey] === key) {
      el.classList.toggle('hidden', !expanded);
    }
  });
  document.querySelectorAll(toggleSelector).forEach(btn => {
    if (btn.dataset[toggleDatasetKey] === key) {
      btn.textContent = expanded ? 'Show less' : 'Show all';
      btn.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    }
  });
}

function toggleSessionGroupLimit(key) {
  const expanded = !isGroupLimitExpanded(SESSION_GROUP_LIMIT_STORAGE_KEY, key);
  setGroupLimitExpanded(SESSION_GROUP_LIMIT_STORAGE_KEY, key, expanded);
  updateGroupLimitDom('session', key, expanded);
}

function toggleCronGroupLimit(key) {
  const expanded = !isGroupLimitExpanded(CRON_GROUP_LIMIT_STORAGE_KEY, key);
  setGroupLimitExpanded(CRON_GROUP_LIMIT_STORAGE_KEY, key, expanded);
  updateGroupLimitDom('cron', key, expanded);
}

async function showGroupSelector(sessionId, currentGroup) {
  // Fetch existing groups
  let groups = [];
  try {
    const res = await fetch('/api/sessions/groups');
    if (!res.ok) throw new Error(`Fetch groups failed: ${res.status}`);
    groups = await res.json();
  } catch (err) {
    console.error('Fetch groups failed:', err);
    return;
  }

  // Remove any existing modal
  document.getElementById('group-modal-overlay')?.remove();

  const overlay = document.createElement('div');
  overlay.id = 'group-modal-overlay';
  overlay.className = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';

  const groupButtons = groups.map(g => {
    const isActive = g === currentGroup;
    const activeClass = isActive ? 'bg-purple-600/30 text-purple-300 border-purple-500/50' : 'bg-slate-700 hover:bg-slate-600 text-slate-300 border-transparent';
    return `<button data-group="${escapeHtmlAttr(g)}" class="w-full text-left px-3 py-2 rounded-lg text-sm border transition-colors ${activeClass}">${escapeHtml(g)}</button>`;
  }).join('');

  overlay.innerHTML = `
    <div class="bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72"
         onclick="event.stopPropagation()">
      <p class="text-sm text-slate-300 mb-3 font-semibold">Set Group</p>
      <div class="flex flex-col gap-1.5 mb-3 max-h-48 overflow-y-auto">
        ${currentGroup ? `<button data-group="" class="w-full text-left px-3 py-2 rounded-lg text-sm bg-slate-700 hover:bg-red-600/20 hover:text-red-300 text-slate-400 transition-colors">Remove group</button>` : ''}
        ${groupButtons}
      </div>
      <div class="flex gap-2">
        <input id="new-group-input" type="text" placeholder="New group name..."
               class="flex-1 bg-slate-700 border border-slate-600 rounded-lg px-3 py-1.5 text-sm text-slate-200 placeholder-slate-500 focus:outline-none focus:border-purple-500">
        <button id="new-group-btn" class="px-3 py-1.5 text-sm rounded-lg bg-purple-600 hover:bg-purple-500 text-white transition-colors">Add</button>
      </div>
    </div>`;

  // Handle existing group clicks
  overlay.querySelectorAll('[data-group]').forEach(btn => {
    btn.addEventListener('click', () => {
      const group = btn.dataset.group || null;
      document.getElementById('group-modal-overlay')?.remove();
      setSessionGroup(sessionId, group);
    });
  });

  // Handle new group
  const addNewGroup = () => {
    const input = document.getElementById('new-group-input');
    const name = input.value.trim();
    if (!name) return;
    document.getElementById('group-modal-overlay')?.remove();
    setSessionGroup(sessionId, name);
  };
  overlay.querySelector('#new-group-btn').addEventListener('click', addNewGroup);
  overlay.querySelector('#new-group-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') addNewGroup();
    if (e.key === 'Escape') document.getElementById('group-modal-overlay')?.remove();
  });

  // Close on overlay click
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay) overlay.remove();
  });
  // Close on Escape
  const escHandler = (e) => {
    if (e.key === 'Escape') {
      document.getElementById('group-modal-overlay')?.remove();
      document.removeEventListener('keydown', escHandler);
    }
  };
  document.addEventListener('keydown', escHandler);

  document.body.appendChild(overlay);
  document.getElementById('new-group-input').focus();
}

async function setSessionGroup(sessionId, group) {
  try {
    const res = await fetch(`/api/sessions/${sessionId}/group`, {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({group}),
    });
    if (!res.ok) throw new Error(`Set group failed: ${res.status}`);
    // Refresh the sidebar
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Set group failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Grouped scheduled task rendering
// ---------------------------------------------------------------------------
function renderScheduledSessionItem(s, options = {}) {
  const isActive = SESSION_ID === s.id;
  const indicatorState = getSessionIndicatorState(s);
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
  const extraClass = options.extraClass ? ' ' + options.extraClass : '';
  const extraAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  return `<a href="/?session=${s.id}&filter=scheduled"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}${extraClass}"
     ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
     onclick="event.preventDefault(); switchSession('${s.id}')"
     id="session-${s.id}"${extraAttrs}>
    <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
    <svg id="worker-indicator-${s.id}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/></svg>
    <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && indicatorState === 'idle' ? '' : 'hidden'}"></span>
    ${renderPendingTriggerIndicator(s)}
    <svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task || '')}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>
    ${renderTuiStatusDot(s)}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : ''}
      ${s.last_run_status ? `<span class="block text-xs ${s.last_run_status === 'success' ? 'text-green-400' : s.last_run_status === 'running' ? 'text-yellow-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400')}">Last: ${escapeHtml(s.last_run_status)}${s.last_scheduled_run ? ', ' + formatLastRun(s.last_scheduled_run) : ''}${s.last_run_status === 'failed' && s.schedule_allow_failure ? ' (review needed)' : ''}</span>` : ''}
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
  const limitState = loadGroupLimitState(CRON_GROUP_LIMIT_STORAGE_KEY);

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No project)';
    const groupSessions = groups[key];
    const enabledCount = groupSessions.filter(s => s.schedule_enabled !== false).length;
    const totalCount = groupSessions.length;
    const isCollapsed = collapsedState[key] !== false; // collapsed by default
    const isLimitExpanded = limitState[key] === true;
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
        ${groupSessions.map((s, index) => renderScheduledSessionItem(
          s,
          groupLimitItemOptions('cron', key, s, index, isLimitExpanded)
        )).join('')}
        ${renderGroupLimitToggle('cron', key, groupSessions.length, isLimitExpanded)}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
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

// ---------------------------------------------------------------------------
// Grouped session list rendering (by session.group)
// ---------------------------------------------------------------------------
function renderGroupedSessionList(sessions, filter) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    nav.innerHTML = '<p class="text-slate-500 text-sm px-3 py-2">No sessions yet</p>';
    return;
  }
  // Group by s.group
  const groups = {};
  sessions.forEach(s => {
    const key = s.group || '';
    if (!groups[key]) groups[key] = [];
    groups[key].push(s);
  });
  // Sort: named groups alphabetically, '' (no group) last
  const sortedKeys = Object.keys(groups).sort((a, b) => {
    if (a === '') return 1;
    if (b === '') return -1;
    return a.localeCompare(b);
  });
  // Load collapsed state from localStorage (expanded by default)
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('session-group-collapsed') || '{}'); } catch (e) {}
  const limitState = loadGroupLimitState(SESSION_GROUP_LIMIT_STORAGE_KEY);

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No group)';
    const groupSessions = groups[key];
    const isCollapsed = collapsedState[key] === true; // expanded by default
    const isLimitExpanded = limitState[key] === true;
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtmlAttr(key);

    const groupActions = key ? `
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); renameGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-blue-400 transition-opacity" title="Rename group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/></svg>
      </button>
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); deleteGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-red-400 transition-opacity" title="Delete group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
      </button>` : '';

    html += `<div class="session-group group" data-sgroup-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           data-sgroup-toggle-key="${safeKey}"
           onclick="toggleSessionGroup(this.dataset.sgroupToggleKey)">
        <svg class="w-3 h-3 text-slate-500 transition-transform session-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        ${groupActions}
        <span class="text-xs text-slate-500 ml-auto">${groupSessions.length}</span>
      </div>
      <div class="session-group-items ${isCollapsed ? 'hidden' : ''}" data-sgroup-items="${safeKey}">
        ${groupSessions.map((s, index) => renderSessionItem(
          s,
          filter,
          groupLimitItemOptions('session', key, s, index, isLimitExpanded)
        )).join('')}
        ${renderGroupLimitToggle('session', key, groupSessions.length, isLimitExpanded)}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
}

function toggleSessionGroup(key) {
  let collapsedState = {};
  try { collapsedState = JSON.parse(localStorage.getItem('session-group-collapsed') || '{}'); } catch (e) {}
  const wasCollapsed = collapsedState[key] === true;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem('session-group-collapsed', JSON.stringify(collapsedState));

  const items = Array.from(document.querySelectorAll('.session-group-items'))
    .find(el => el.dataset.sgroupItems === key);
  if (items) items.classList.toggle('hidden');
  const group = Array.from(document.querySelectorAll('.session-group'))
    .find(el => el.dataset.sgroupKey === key);
  if (group) {
    const chevron = group.querySelector('.session-group-chevron');
    if (chevron) chevron.classList.toggle('rotate-90');
  }
}

async function renameGroup(oldName) {
  const newName = prompt(`Rename group "${oldName}" to:`, oldName);
  if (!newName || newName.trim() === '' || newName.trim() === oldName) return;
  const res = await fetch('/api/sessions/groups/rename', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({old_name: oldName, new_name: newName.trim()}),
  });
  if (!res.ok) throw new Error(`Rename group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

async function deleteGroup(groupName) {
  if (!confirm(`Remove group "${groupName}"? Sessions will be ungrouped.`)) return;
  const res = await fetch('/api/sessions/groups/delete', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({group: groupName}),
  });
  if (!res.ok) throw new Error(`Delete group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

function renderSessionItem(s, filter, options = {}) {
  const starSvg = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;
  const isActive = SESSION_ID === s.id;
  const indicatorState = getSessionIndicatorState(s);
  const activeClass = isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
  const starFill = s.starred ? 'currentColor' : 'none';
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const timeStr = s.updated_at ? relativeTime(s.updated_at) : '';
  const timeIso = s.updated_at || '';
  const groupBtn = `
    <button data-current-group="${s.group ? escapeHtmlAttr(s.group) : ''}"
            onclick="event.preventDefault(); event.stopPropagation(); showGroupSelector('${s.id}', this.dataset.currentGroup || null)"
            class="opacity-0 group-hover:opacity-100 p-1 hover:text-purple-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Set group">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z"/></svg>
    </button>`;
  let actions = '';
  if (filter === 'archived') {
    const ratingBadge = s.rating === 'thumbs_up' ? '<span class="text-xs flex-shrink-0" title="Rated: thumbs up">👍</span>'
      : s.rating === 'neutral' ? '<span class="text-xs flex-shrink-0" title="Rated: neutral">—</span>'
      : s.rating === 'thumbs_down' ? '<span class="text-xs flex-shrink-0" title="Rated: thumbs down">👎</span>'
      : '';
    actions = `
      ${ratingBadge}
      <button onclick="event.preventDefault(); event.stopPropagation(); toggleSessionStar('${s.id}', ${s.starred})"
              class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
        <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${starSvg}</svg>
      </button>
      ${groupBtn}
      <button onclick="event.preventDefault(); event.stopPropagation(); unarchiveSession('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 hover:text-green-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Unarchive">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5-5m0 0l5 5m-5-5v12"/></svg>
      </button>
      <button onclick="event.preventDefault(); event.stopPropagation(); confirmDeletePermanently('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 text-slate-500 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Delete permanently">
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
      ${groupBtn}
      <button onclick="event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')"
              class="opacity-0 group-hover:opacity-100 p-1 hover:text-red-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Archive">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/></svg>
      </button>
      ${gearBtn}`;
  }
  const extraClass = options.extraClass ? ' ' + options.extraClass : '';
  const extraAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  return `<a href="/?session=${s.id}&filter=${filter}"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}${extraClass}"
     ondblclick="startRename(event, '${s.id}', '${escapeHtml(s.name)}')"
     onclick="event.preventDefault(); switchSession('${s.id}')"
     id="session-${s.id}"${extraAttrs}>
    <svg id="spinner-${s.id}" class="w-4 h-4 animate-spin text-yellow-400 flex-shrink-0 ${indicatorState === 'thinking' ? '' : 'hidden'}" fill="none" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
    <svg id="worker-indicator-${s.id}" class="w-3.5 h-3.5 text-amber-400 flex-shrink-0 animate-[spin_3s_linear_infinite] ${indicatorState === 'worker_only' ? '' : 'hidden'}" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"/></svg>
    <span id="unread-${s.id}" data-has-unread="${s.has_unread ? 1 : 0}" class="w-2 h-2 rounded-full bg-yellow-400 animate-pulse-dot flex-shrink-0 ${s.has_unread && indicatorState === 'idle' ? '' : 'hidden'}"></span>
    ${renderPendingTriggerIndicator(s)}
    ${s.scheduled_task ? `<svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtml(s.scheduled_task)}"><circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/></svg>` : ''}
    ${renderTuiStatusDot(s)}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${filter === 'scheduled' && s.schedule_cron ? `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + formatNextRun(s.schedule_next_run)}</span>` : `<span class="block text-xs text-slate-500 session-time" data-time="${timeIso}">${timeStr}</span>`}
    </span>
    ${actions}
  </a>`;
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
      scheduled: 'No scheduled sessions',
      search: 'No matching sessions',
    };
    nav.innerHTML = `<p class="text-slate-500 text-sm px-3 py-2">${labels[filter]}</p>`;
    return;
  }
  // Always use grouped rendering for non-search tabs
  if (filter !== 'search') {
    renderGroupedSessionList(sessions, filter);
    return;
  }
  nav.innerHTML = sessions.map(s => renderSessionItem(s, filter)).join('');
  // Resync sessionUnread dict from fresh DOM data
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
  updateRelativeTimes();
  refreshTuiDots();
}



Object.assign(Sidebar, {
  escapeHtmlAttr,
  loadGroupLimitState,
  isGroupLimitExpanded,
  setGroupLimitExpanded,
  resetGroupLimitState,
  shouldLimitHideSession,
  isOverGroupLimitExtra,
  groupLimitItemOptions,
  renderGroupLimitToggle,
  updateGroupLimitDom,
  toggleSessionGroupLimit,
  toggleCronGroupLimit,
  showGroupSelector,
  setSessionGroup,
  renderScheduledSessionItem,
  renderGroupedScheduledList,
  toggleCronGroup,
  renderGroupedSessionList,
  toggleSessionGroup,
  renameGroup,
  deleteGroup,
  renderSessionItem,
  renderSessionList,
});
Sidebar.expose([
  'escapeHtmlAttr',
  'resetGroupLimitState',
  'toggleSessionGroupLimit',
  'toggleCronGroupLimit',
  'showGroupSelector',
  'setSessionGroup',
  'renderScheduledSessionItem',
  'renderGroupedScheduledList',
  'toggleCronGroup',
  'renderGroupedSessionList',
  'toggleSessionGroup',
  'renameGroup',
  'deleteGroup',
  'renderSessionItem',
  'renderSessionList',
]);

})();
