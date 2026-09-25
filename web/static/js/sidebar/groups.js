(function() {
  const Sidebar = globalThis.Sidebar;

const GROUP_SESSION_PREVIEW_LIMIT = 5;
const SESSION_GROUP_LIMIT_STORAGE_KEY = 'session-group-list-expanded';
const CRON_GROUP_LIMIT_STORAGE_KEY = 'cron-group-list-expanded';
const SESSION_GROUP_COLLAPSED_STORAGE_KEY = 'session-group-collapsed';
const CRON_GROUP_COLLAPSED_STORAGE_KEY = 'cron-group-collapsed';
const groupLimitState = {
  [SESSION_GROUP_LIMIT_STORAGE_KEY]: {},
  [CRON_GROUP_LIMIT_STORAGE_KEY]: {},
};

function loadGroupLimitState(storageKey) {
  return groupLimitState[storageKey];
}

function isGroupLimitExpanded(storageKey, key) {
  return loadGroupLimitState(storageKey)[key] === true;
}

function setGroupLimitExpanded(storageKey, key, expanded) {
  const state = loadGroupLimitState(storageKey);
  state[key] = expanded;
}

function resetGroupLimitState() {
  groupLimitState[SESSION_GROUP_LIMIT_STORAGE_KEY] = {};
  groupLimitState[CRON_GROUP_LIMIT_STORAGE_KEY] = {};
}

// A corrupt stored blob degrades to no saved state rather than breaking the
// render/toggle path, so the catch stays.
function loadGroupCollapsedState(storageKey) {
  try { return JSON.parse(localStorage.getItem(storageKey) || '{}'); } catch (e) { return {}; }
}

// The one key order both grouped renderers use: named groups alphabetically,
// '' (the keyless bucket) last.
function groupSessionsBySortedKeys(sessions, keyFn) {
  const groups = {};
  sessions.forEach(s => {
    const key = keyFn(s) || '';
    if (!groups[key]) groups[key] = [];
    groups[key].push(s);
  });
  const sortedKeys = Object.keys(groups).sort((a, b) => {
    if (a === '') return 1;
    if (b === '') return -1;
    return a.localeCompare(b);
  });
  return {groups, sortedKeys};
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

const GROUP_MODAL_OVERLAY_ID = 'group-modal-overlay';

function closeGroupModal() {
  document.getElementById(GROUP_MODAL_OVERLAY_ID)?.remove();
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
  closeGroupModal();

  const overlay = document.createElement('div');
  overlay.id = GROUP_MODAL_OVERLAY_ID;
  overlay.className = MODAL_OVERLAY_CLASS;

  const groupButtons = groups.map(g => {
    const isActive = g === currentGroup;
    const activeClass = isActive ? 'bg-purple-600/30 text-purple-300 border-purple-500/50' : 'bg-slate-700 hover:bg-slate-600 text-slate-300 border-transparent';
    return `<button data-group="${escapeHtmlAttr(g)}" class="w-full text-left px-3 py-2 rounded-lg text-sm border transition-colors ${activeClass}">${escapeHtml(g)}</button>`;
  }).join('');

  overlay.innerHTML = `
    <div class="${MODAL_DIALOG_CLASS}"
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
      closeGroupModal();
      setSessionGroup(sessionId, group);
    });
  });

  // Handle new group
  const addNewGroup = () => {
    const input = document.getElementById('new-group-input');
    const name = input.value.trim();
    if (!name) return;
    closeGroupModal();
    setSessionGroup(sessionId, name);
  };
  overlay.querySelector('#new-group-btn').addEventListener('click', addNewGroup);
  overlay.querySelector('#new-group-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') addNewGroup();
    if (e.key === 'Escape') closeGroupModal();
  });

  // Close on overlay click
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay) overlay.remove();
  });
  // Close on Escape
  const escHandler = (e) => {
    if (e.key === 'Escape') {
      closeGroupModal();
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
      headers: JSON_HEADERS,
      body: JSON.stringify({group}),
    });
    if (!res.ok) throw new Error(`Set group failed: ${res.status}`);
    if (currentFilter === 'archived') {
      // The archived view updates the row and filter-strip counts in place;
      // a refetch would rebuild the whole paginated list.
      applyArchivedGroupChange(sessionId, group);
      return;
    }
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Set group failed:', err);
  }
}

// ---------------------------------------------------------------------------
// Grouped scheduled task rendering
// ---------------------------------------------------------------------------
// Global cron load-failure badge, rendered from the broken cron entries the
// Scheduled filter fetch hands the renderer (name order); clicking opens the
// cron editor on the first broken task. Empty when nothing is broken.
function renderCronErrorBadge(brokenTasks) {
  const broken = brokenTasks || [];
  if (!broken.length) return '';
  return `<div class="mx-3 my-2 px-3 py-2 rounded-lg bg-red-900/40 border border-red-700/50 text-red-300 text-xs cursor-pointer"
       role="button" title="Open the first failed task"
       onclick="openCronEditor('${escapeHtml(broken[0].name)}')">⚠ ${broken.length} scheduled tasks failed to load</div>`;
}

// Session-row action buttons shared by renderScheduledSessionItem and
// renderSessionItem: a markup change to one of these buttons lands here, not
// in one renderer.
const STAR_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;

// The one trash-can outline: archive/delete action buttons below and, through
// the namespace, filters.js's delete-confirm modal (Sidebar.TRASH_SVG_PATH).
const TRASH_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/>`;

// The one cog-outline body: the cron-edit button below and, through the
// namespace, status.js's worker indicator (Sidebar.GEAR_SVG_PATH). Each
// call site keeps its own center markup.
const GEAR_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"/>`;

// The one right-chevron outline: the cron-group and session-group collapse
// toggles below and, through the namespace, workers.js's thread-card chevron
// (Sidebar.CHEVRON_SVG_PATH). Each call site keeps its own <svg> wrapper.
const CHEVRON_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>`;

// The one pencil outline: the session-row rename button below and the
// session-group rename button.
const PENCIL_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z"/>`;

// The one clock-badge body (face plus hands): renderScheduledBadge below
// and, through the namespace, workers.js's trigger-card icon
// (Sidebar.CLOCK_SVG_BODY). Each call site keeps its own <svg> wrapper.
const PLUS_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4"/>`;
const DOC_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414A1 1 0 0121 9.414V19a2 2 0 01-2 2z"/>`;
const CLOCK_SVG_BODY = `<circle cx="12" cy="12" r="10" stroke-width="2"/><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6l4 2"/>`;

// The one modal chrome for the sidebar's JS-built overlays: showGroupSelector's
// group modal below and, through the namespace, filters.js's delete-confirm
// modal (Sidebar.MODAL_OVERLAY_CLASS / Sidebar.MODAL_DIALOG_CLASS). Each dialog
// keeps its own extras (filters.js's adds text-center) and inner markup.
const MODAL_OVERLAY_CLASS = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';
const MODAL_DIALOG_CLASS = 'bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72';

// The one construction of the star button's onclick body: rendered below for
// a fresh list render and, through the namespace, re-applied by filters.js's
// toggleSessionStar after a star toggle (Sidebar.starButtonOnclick). The
// starred argument is the state the next click should toggle away from.
function starButtonOnclick(id, starred) {
  return `event.preventDefault(); event.stopPropagation(); toggleSessionStar('${id}', ${starred})`;
}

function renderStarButton(s, activeBtnClass) {
  const starFill = s.starred ? 'currentColor' : 'none';
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  return `<button onclick="${starButtonOnclick(s.id, s.starred)}"
          class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass} ${activeBtnClass}" title="Star" id="star-${s.id}">
    <svg class="w-3.5 h-3.5" fill="${starFill}" stroke="currentColor" viewBox="0 0 24 24">${STAR_SVG_PATH}</svg>
  </button>`;
}

// The one hover-revealed icon button frame for the session row's plain
// actions: a markup change lands here, not in one renderer. The star's
// dynamic fill and id, the cron gear's guard, and the set-group button's
// data attribute stay at their own renderers.
function renderRowActionButton(onclick, colorClass, title, svgBody, activeBtnClass) {
  return `<button onclick="${onclick}"
          class="opacity-0 group-hover:opacity-100 p-1 ${colorClass} transition-opacity flex-shrink-0 ${activeBtnClass}" title="${title}">
    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">${svgBody}</svg>
  </button>`;
}

function renderRenameButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); startRename(event, '${s.id}')`,
      'hover:text-blue-400',
      'Rename',
      PENCIL_SVG_PATH,
      activeBtnClass);
}

function renderArchiveButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); archiveSession('${s.id}')`,
      'hover:text-red-400',
      'Archive',
      TRASH_SVG_PATH,
      activeBtnClass);
}

// Renders '' for a falsy taskName: the caller's show-guard stays visible in
// the argument it passes.
// A logical session row creates a child logical session under itself: the
// same creation body as New Session with this row as the parent.
function renderNewChildButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); createChildSession('${s.id}')`,
      'hover:text-green-400',
      'New child session',
      PLUS_SVG_PATH,
      activeBtnClass);
}

// A logical session row opens the read-only Task & context dialog
// (modals.js): its task record over the context its next Run assembles.
function renderTaskContextButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); openTaskContextModal('${s.id}')`,
      'hover:text-blue-300',
      'Task &amp; context',
      DOC_SVG_PATH,
      activeBtnClass);
}

function renderCronGearButton(taskName, activeBtnClass) {
  if (!taskName) return '';
  return `<button onclick="event.preventDefault(); event.stopPropagation(); openCronEditor('${escapeHtml(taskName)}')"
          class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Edit task config">
    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">${GEAR_SVG_PATH}<circle cx="12" cy="12" r="3"/></svg>
  </button>`;
}

// Scheduled-task clock badge shared by renderScheduledSessionItem and
// renderSessionItem: a markup change lands here, not in one renderer.
function renderScheduledBadge(s) {
  return `<svg class="w-3 h-3 flex-shrink-0 ${s.schedule_enabled === false ? 'text-slate-500' : 'text-blue-400'}" fill="none" stroke="currentColor" viewBox="0 0 24 24" title="Scheduled: ${escapeHtmlAttr(s.scheduled_task)}">${CLOCK_SVG_BODY}</svg>`;
}

// Cron line shared by renderScheduledSessionItem and renderSessionItem's
// scheduled branch: a markup change lands here, not in one renderer. The
// caller's show-guard stays at the call site: renderScheduledSessionItem
// gates on s.schedule_cron alone, renderSessionItem also gates on the
// 'scheduled' filter.
function renderSessionScheduleLine(s) {
  return `<span class="block text-xs text-slate-500">${escapeHtml(s.schedule_cron)} (${escapeHtml(s.schedule_timezone || '')})</span><span class="block text-xs text-slate-500">${s.schedule_enabled === false ? 'Disabled' : 'Next: ' + relativeTime(s.schedule_next_run)}</span>`;
}

// Session-row highlight shared by renderScheduledSessionItem and
// renderSessionItem: a tint change lands here, not in one renderer.
function sessionRowActiveClass(isActive) {
  return isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
}

// A projected legacy worker-thread row's target: the thread's transcript
// opened in the main chat view, read-only, addressed by the owning session.
function openThreadView(sessionId, threadId) {
  window.location.href = '/?session=' + encodeURIComponent(sessionId)
    + '&thread=' + encodeURIComponent(threadId);
}

// The one session-row frame shared by renderScheduledSessionItem and
// renderSessionItem: the anchor open tag, the name span, and the closing tag.
// A markup change to the row frame lands here, not in one renderer.
// indicators and line are prebuilt strings — the status column and the content
// after the name span — and actions the trailing button column.
function renderSessionRowShell(s, {filter, activeClass, options, indicators, line, actions}) {
  const extraClass = options.extraClass ? ' ' + options.extraClass : '';
  const extraAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  // A projected legacy worker-thread row has no session behind its id: the
  // click opens the thread's transcript in the main chat view (the 4.1 URL)
  // instead of switching sessions, and a double click never starts a rename.
  const click = s.worker_thread
    ? `openThreadView('${s.worker_thread.session_id}', '${s.worker_thread.thread_id}')`
    : `switchSession('${s.id}')`;
  const dblclick = s.worker_thread ? '' : ` ondblclick="startRename(event, '${s.id}')"`;
  return `<a href="/?session=${s.id}&filter=${filter}"
     class="group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}${extraClass}"${dblclick}
     onclick="event.preventDefault(); ${click}"
     id="session-${s.id}"${extraAttrs}>
    ${indicators}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${line}
    </span>
    ${actions}
  </a>`;
}

function renderScheduledSessionItem(s, options = {}) {
  const isActive = SESSION_ID === s.id;
  const activeClass = sessionRowActiveClass(isActive);
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const actions = `
    ${renderStarButton(s, activeBtnClass)}
    ${renderRenameButton(s, activeBtnClass)}
    ${renderArchiveButton(s, activeBtnClass)}
    ${renderCronGearButton(s.scheduled_task, activeBtnClass)}`;
  // The tree chevron leads the indicator slot, as in renderSessionItem: the
  // Scheduled tab's cron rows nest their worker leaves behind it.
  const indicators = [
      options.treeChildCount ? renderTreeChevron(s.id, options.treeChildCount) : '',
      renderSessionIndicators(s),
      renderPendingTriggerIndicator(s),
      renderPendingPlanApprovalIndicator(s),
      renderScheduledBadge(s),
      renderTuiStatusDot(s),
  ].join('\n    ');
  const line = `${s.schedule_cron ? renderSessionScheduleLine(s) : ''}
      ${s.last_run_status ? `<span class="block text-xs ${s.last_run_status === 'success' ? 'text-green-400' : s.last_run_status === 'running' ? 'text-yellow-400' : s.last_run_status === 'skipped' ? 'text-slate-400' : (s.schedule_allow_failure ? 'text-amber-400' : 'text-red-400')}">Last: ${escapeHtml(s.last_run_status)}${s.last_scheduled_run ? ', ' + formatBubbleTime(s.last_scheduled_run) : ''}${s.last_run_status === 'failed' && s.schedule_allow_failure ? ' (review needed)' : ''}</span>` : ''}`;
  return renderSessionRowShell(s, {filter: 'scheduled', activeClass, options, indicators, line, actions});
}

// Empty list note shared by the scheduled/grouped/search lists here and,
// through the namespace, the archived list in archived.js: a markup change
// lands here, not in each list's empty branch.
function renderEmptyNote(text) {
  return `<p class="text-slate-500 text-sm px-3 py-2">${text}</p>`;
}

// Every sidebar render path rebuilds rows in place from a session list, so the
// unread map must be refolded from that same list or unread badges go stale.
function resyncSessionUnread(sessions) {
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
}

function renderGroupedScheduledList(sessions, options = {}) {
  const nav = document.getElementById('session-list');
  const brokenTasks = options.brokenTasks || [];
  lastScheduledRenderArgs = {sessions, brokenTasks};
  const badgeHtml = renderCronErrorBadge(brokenTasks);
  if (!sessions.length) {
    nav.innerHTML = badgeHtml + renderEmptyNote('No scheduled sessions');
    return;
  }
  // The same parent/child tree the All tab builds: a projected worker leaf
  // nests under the cron session that ran it instead of sitting at group
  // level, and only the roots group by project.
  const {roots, childrenOf, parentOf} = buildSessionTree(sessions);
  lastTreeChildrenOf = childrenOf;
  lastTreeParentOf = parentOf;
  revealActiveSessionOnce(parentOf);
  const {groups, sortedKeys} = groupSessionsBySortedKeys(roots, s => s.schedule_project);
  const collapsedState = loadGroupCollapsedState(CRON_GROUP_COLLAPSED_STORAGE_KEY);
  const limitState = loadGroupLimitState(CRON_GROUP_LIMIT_STORAGE_KEY);

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No project)';
    const groupRoots = groups[key];
    // Counts and the preview cap read the roots alone: a leaf belongs to its
    // cron session's subtree, not to the group's row budget.
    const enabledCount = groupRoots.filter(s => s.schedule_enabled !== false).length;
    const totalCount = groupRoots.length;
    const isCollapsed = collapsedState[key] !== false; // collapsed by default
    const isLimitExpanded = limitState[key] === true;
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtml(key);

    html += `<div class="cron-group" data-group-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           onclick="toggleCronGroup('${safeKey}')">
        <svg class="w-3 h-3 text-slate-500 transition-transform cron-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          ${CHEVRON_SVG_PATH}
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        <span class="text-xs text-slate-500 ml-auto">${enabledCount}/${totalCount} enabled</span>
      </div>
      <div class="cron-group-items ${isCollapsed ? 'hidden' : ''}" data-group-items="${safeKey}">
        ${groupRoots.map((s, index) => renderSessionTree(
          s,
          'scheduled',
          groupLimitItemOptions('cron', key, s, index, isLimitExpanded),
          childrenOf,
          renderScheduledSessionItem
        )).join('')}
        ${renderGroupLimitToggle('cron', key, groupRoots.length, isLimitExpanded)}
      </div>
    </div>`;
  }
  nav.innerHTML = badgeHtml + html;
  resyncSessionUnread(sessions);
  // A collapsed cron row stands in for its hidden leaves' running/unread state.
  if (typeof Sidebar.refreshTreeIndicators === 'function') Sidebar.refreshTreeIndicators();
  updateRelativeTimes();
  refreshTuiDots();
}

function toggleCronGroup(key) {
  const collapsedState = loadGroupCollapsedState(CRON_GROUP_COLLAPSED_STORAGE_KEY);
  const wasCollapsed = collapsedState[key] !== false;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem(CRON_GROUP_COLLAPSED_STORAGE_KEY, JSON.stringify(collapsedState));

  const items = document.querySelector(`[data-group-items="${key}"]`);
  if (items) items.classList.toggle('hidden');
  const group = document.querySelector(`[data-group-key="${key}"]`);
  if (group) {
    const chevron = group.querySelector('.cron-group-chevron');
    if (chevron) chevron.classList.toggle('rotate-90');
  }
}

let lastGroupedRenderArgs = null;
let lastScheduledRenderArgs = null;
// Whether the last renderSessionList call painted the search overlay. The
// search flow never assigns currentFilter, so this marker — not the filter
// state — is what tells the delete path a flat search list is on screen.
let searchListPainted = false;

// ---------------------------------------------------------------------------
// Grouped session list rendering (by session.group)
// ---------------------------------------------------------------------------
function renderGroupedSessionList(sessions, filter, options = {}) {
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    nav.innerHTML = renderEmptyNote('No sessions yet');
    return;
  }
  lastGroupedRenderArgs = {sessions, filter};
  // Grouping follows the root rows; a child row nests under its parent
  // whatever its own group field says.
  const {roots, childrenOf, parentOf} = buildSessionTree(sessions);
  lastTreeChildrenOf = childrenOf;
  lastTreeParentOf = parentOf;
  revealActiveSessionOnce(parentOf);
  const {groups, sortedKeys} = groupSessionsBySortedKeys(roots, s => s.group);
  const collapsedState = loadGroupCollapsedState(SESSION_GROUP_COLLAPSED_STORAGE_KEY);
  const limitState = loadGroupLimitState(SESSION_GROUP_LIMIT_STORAGE_KEY);

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No group)';
    const groupSessions = groups[key];
    const isCollapsed = collapsedState[key] === true; // expanded by default
    const isLimitExpanded = limitState[key] === true;
    const chevronClass = isCollapsed ? '' : 'rotate-90';
    const safeKey = escapeHtmlAttr(key);
    const taskRowOptions = (s, index) =>
      groupLimitItemOptions('session', key, s, index, isLimitExpanded);

    const groupActions = key ? `
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); renameGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-blue-400 transition-opacity" title="Rename group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">${PENCIL_SVG_PATH}</svg>
      </button>
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); deleteGroup(this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-red-400 transition-opacity" title="Delete group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">${TRASH_SVG_PATH}</svg>
      </button>` : '';

    html += `<div class="session-group group" data-sgroup-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           data-sgroup-toggle-key="${safeKey}"
           onclick="toggleSessionGroup(this.dataset.sgroupToggleKey)">
        <svg class="w-3 h-3 text-slate-500 transition-transform session-group-chevron ${chevronClass}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          ${CHEVRON_SVG_PATH}
        </svg>
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        ${groupActions}
        <span class="text-xs text-slate-500 ml-auto">${countTreeRows(groupSessions, childrenOf)}</span>
      </div>
      <div class="session-group-items ${isCollapsed ? 'hidden' : ''}" data-sgroup-items="${safeKey}">
        ${groupSessions.map((s, index) => renderSessionTree(
          s,
          filter,
          taskRowOptions(s, index),
          childrenOf
        )).join('')}
        ${renderGroupLimitToggle('session', key, groupSessions.length, isLimitExpanded)}
      </div>
    </div>`;
  }
  nav.innerHTML = html;
  resyncSessionUnread(sessions);
  // Parent rows take their collapsed-subtree stand-ins now that the rows exist.
  if (typeof Sidebar.refreshTreeIndicators === 'function') Sidebar.refreshTreeIndicators();
  updateRelativeTimes();
  refreshTuiDots();
}

function toggleSessionGroup(key) {
  const collapsedState = loadGroupCollapsedState(SESSION_GROUP_COLLAPSED_STORAGE_KEY);
  const wasCollapsed = collapsedState[key] === true;
  collapsedState[key] = !wasCollapsed;
  localStorage.setItem(SESSION_GROUP_COLLAPSED_STORAGE_KEY, JSON.stringify(collapsedState));

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
    headers: JSON_HEADERS,
    body: JSON.stringify({old_name: oldName, new_name: newName.trim()}),
  });
  if (!res.ok) throw new Error(`Rename group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

async function deleteGroup(groupName) {
  if (!confirm(`Remove group "${groupName}"? Sessions will be ungrouped.`)) return;
  const res = await fetch('/api/sessions/groups/delete', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({group: groupName}),
  });
  if (!res.ok) throw new Error(`Delete group failed: ${res.status}`);
  switchSidebarFilter(currentFilter);
}

// Model label for a session row: BACKEND_OPTIONS carries the config-authored label
// ("CC · Opus 5"); the row shows it without the backend-family prefix because the
// 320px sidebar leaves only ~67px next to the timestamp. Returns null when the
// session carries no backend id.
function sessionBackendLabel(session) {
  const backendId = (session && session.backend) || '';
  if (!backendId) return null;
  const options = typeof BACKEND_OPTIONS === 'undefined' ? null : BACKEND_OPTIONS;
  const full = (options && options[backendId]) || backendId;
  const sep = full.indexOf(' · ');
  return { full, display: sep === -1 ? full : full.slice(sep + 3) };
}

function renderSessionTimeLine(s, timeIso, timeStr, staticTime = false) {
  const label = sessionBackendLabel(s);
  const model = label
    ? `<span class="session-backend truncate" title="${escapeHtmlAttr(label.full)}">${escapeHtml(label.display)}</span>`
    : '';
  // A static row formats its time once at build; without data-time it stays
  // outside updateRelativeTimes's [data-time] sweep, so a long archived list
  // never joins the periodic full-table refresh.
  const timeAttr = staticTime ? '' : ` data-time="${timeIso}"`;
  return `<span class="flex items-center gap-1.5 text-xs text-slate-500">`
    + `<span class="session-time flex-shrink-0"${timeAttr}>${timeStr}</span>`
    + model
    + `</span>`;
}

// ---------------------------------------------------------------------------
// Task-tree nesting: a child task node renders under its parent row.
// ---------------------------------------------------------------------------
// The expanded set lives in memory only, so a reload starts every parent
// collapsed (a persisted expand state once hid a preview-cap regression).
const treeExpandedNodes = new Set();
let lastTreeChildrenOf = new Map();
let lastTreeParentOf = new Map();
// The session whose ancestors the last paint opened (one reveal per switch).
let lastRevealedSessionId = null;

const LEAF_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z"/>`;

// Roots are the rows without a parent in this list (a null parent, or a parent
// that is archived or filtered out); every other row nests under its parent.
// Roots keep the list order; children put logical sessions before worker
// leaves, each newest first. The server rejects cycles, so the walk ends.
function buildSessionTree(sessions) {
  const ids = new Set(sessions.map(s => s.id));
  const childrenOf = new Map();
  const parentOf = new Map();
  const roots = [];
  sessions.forEach(s => {
    const parent = s.task_parent_id;
    if (parent && ids.has(parent)) {
      if (!childrenOf.has(parent)) childrenOf.set(parent, []);
      childrenOf.get(parent).push(s);
      parentOf.set(s.id, parent);
    } else {
      roots.push(s);
    }
  });
  const newestFirst = (a, b) => String(b.updated_at || '').localeCompare(String(a.updated_at || ''));
  childrenOf.forEach(list => list.sort((a, b) => {
    const workerOrder = (a.profile === 'worker' ? 1 : 0) - (b.profile === 'worker' ? 1 : 0);
    return workerOrder || newestFirst(a, b);
  }));
  return {roots, childrenOf, parentOf};
}

function countTreeRows(rows, childrenOf) {
  return rows.reduce((n, s) => n + 1 + countTreeRows(childrenOf.get(s.id) || [], childrenOf), 0);
}

function isTreeNodeExpanded(sessionId) {
  return treeExpandedNodes.has(sessionId);
}

// The children of the last grouped paint, for indicator aggregation.
function treeChildIds(sessionId) {
  return (lastTreeChildrenOf.get(sessionId) || []).map(s => s.id);
}

// The parent of a nested row in the last grouped paint (null for a root).
function treeParentId(sessionId) {
  return lastTreeParentOf.get(sessionId) || null;
}

// The active session's ancestors open once per switch, so a node reached by
// a deep link or a fresh create is visible; a later manual collapse holds
// until the next switch.
function revealActiveSessionOnce(parentOf) {
  if (!SESSION_ID || SESSION_ID === lastRevealedSessionId) return;
  lastRevealedSessionId = SESSION_ID;
  for (let parent = parentOf.get(SESSION_ID); parent; parent = parentOf.get(parent)) {
    treeExpandedNodes.add(parent);
  }
}

function renderTreeChevron(sessionId, childCount) {
  const expanded = treeExpandedNodes.has(sessionId);
  return `<svg class="w-3 h-3 text-slate-500 transition-transform cursor-pointer flex-shrink-0 tree-chevron ${expanded ? 'rotate-90' : ''}"
         data-tree-toggle="${sessionId}" role="button" aria-expanded="${expanded ? 'true' : 'false'}"
         title="${childCount} child task${childCount === 1 ? '' : 's'}"
         onclick="event.preventDefault(); event.stopPropagation(); toggleTreeNode(this.dataset.treeToggle)"
         fill="none" stroke="currentColor" viewBox="0 0 24 24">${CHEVRON_SVG_PATH}</svg>`;
}

function renderWorkerLeafIcon() {
  return `<svg class="w-3.5 h-3.5 text-slate-500 flex-shrink-0" title="Worker (implementation leaf)" fill="none" stroke="currentColor" viewBox="0 0 24 24">${LEAF_SVG_PATH}</svg>`;
}

// An archived worker delivered its work: the row shows a check in the leaf
// icon's place.
const CHECK_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7"/>`;

function renderWorkerDeliveredIcon() {
  return `<svg class="w-3.5 h-3.5 text-green-500 flex-shrink-0" title="Worker (delivered)" fill="none" stroke="currentColor" viewBox="0 0 24 24">${CHECK_SVG_PATH}</svg>`;
}

// One row followed by its subtree. The outer wrapper carries the row's
// group-limit class and attributes, so a root hidden by the 5-row preview
// hides its subtree with it and Show all reveals both; the inner container
// carries the expand state. Each level indents 22px behind a guide line.
// rowRenderer (optional) renders the top row alone; the subtree rows always
// render through renderSessionItem — the Scheduled tab's cron rows are the
// one caller whose root row form (renderScheduledSessionItem) differs from
// its subtree's.
function renderSessionTree(s, filter, options, childrenOf, rowRenderer) {
  const renderRow = rowRenderer || ((row, opts) => renderSessionItem(row, filter, opts));
  const children = childrenOf.get(s.id) || [];
  const row = renderRow(s, {...options, treeChildCount: children.length});
  if (!children.length) return row;
  const wrapClass = ['tree-subtree', options.extraClass || ''].filter(Boolean).join(' ');
  const wrapAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  const hiddenClass = treeExpandedNodes.has(s.id) ? '' : ' hidden';
  return `${row}<div class="${wrapClass}"${wrapAttrs}>
    <div class="tree-children ml-4 pl-1.5 mt-0.5 border-l border-slate-600${hiddenClass}" data-tree-children="${s.id}">
      ${children.map(c => renderSessionTree(c, filter, {}, childrenOf)).join('')}
    </div>
  </div>`;
}

function applyTreeNodeExpansion(sessionId, expanded) {
  if (expanded) treeExpandedNodes.add(sessionId); else treeExpandedNodes.delete(sessionId);
  const selectorId = CSS.escape(sessionId);
  document.querySelectorAll(`[data-tree-children="${selectorId}"]`).forEach(el => {
    el.classList.toggle('hidden', !expanded);
  });
  document.querySelectorAll(`[data-tree-toggle="${selectorId}"]`).forEach(el => {
    el.classList.toggle('rotate-90', expanded);
    el.setAttribute('aria-expanded', expanded ? 'true' : 'false');
  });
  // A parent's indicators stand in for its collapsed subtree: repaint it for
  // the new expand state from the facts already applied.
  if (typeof Sidebar.refreshSessionIndicator === 'function') Sidebar.refreshSessionIndicator(sessionId);
}

function toggleTreeNode(sessionId) {
  applyTreeNodeExpansion(sessionId, !treeExpandedNodes.has(sessionId));
}

function expandTreeNode(sessionId) {
  if (!treeExpandedNodes.has(sessionId)) applyTreeNodeExpansion(sessionId, true);
}

function renderSessionItem(s, filter, options = {}) {
  recordRenderedSessionStatus(s);
  const isActive = SESSION_ID === s.id;
  // An archived session keeps the archived row form in every view (archived
  // tab, search results): unarchive/delete actions, and none of the live-state
  // indicators, which archived sessions cannot carry.
  const isArchivedRow = filter === 'archived' || s.status === 'archived';
  // A worker leaf is identified by its icon and carries the archive action alone.
  const isWorker = s.profile === 'worker';
  const isWorkerRow = isWorker && !isArchivedRow;
  const activeClass = sessionRowActiveClass(isActive);
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const timeStr = s.updated_at ? relativeTime(s.updated_at) : '';
  const timeIso = s.updated_at || '';
  const groupBtn = `
    <button data-current-group="${s.group ? escapeHtmlAttr(s.group) : ''}"
            onclick="event.preventDefault(); event.stopPropagation(); showGroupSelector('${s.id}', this.dataset.currentGroup || null)"
            class="opacity-0 group-hover:opacity-100 p-1 hover:text-purple-400 transition-opacity flex-shrink-0 ${activeBtnClass}" title="Set group">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z"/></svg>
    </button>`;
  // A projected legacy worker-thread row is read-only: no archive, star,
  // rename, group, create-child or Task & context action.
  let actions = '';
  if (s.worker_thread) {
    actions = '';
  } else if (isArchivedRow) {
    actions = `
      ${renderStarButton(s, activeBtnClass)}
      ${groupBtn}
      ${renderRowActionButton(
          `event.preventDefault(); event.stopPropagation(); unarchiveSession('${s.id}')`,
          'hover:text-green-400',
          'Unarchive',
          '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5-5m0 0l5 5m-5-5v12"/>',
          activeBtnClass)}
      ${renderRowActionButton(
          `event.preventDefault(); event.stopPropagation(); confirmDeletePermanently('${s.id}')`,
          'text-slate-500 hover:text-red-400',
          'Delete permanently',
          TRASH_SVG_PATH,
          activeBtnClass)}`;
  } else if (isWorkerRow) {
    actions = renderArchiveButton(s, activeBtnClass);
  } else {
    // A legacy session (profile null) has no task-tree context behind the
    // Task & context dialog (the endpoint answers 400 for it).
    const taskContextBtn = s.profile === null ? '' : renderTaskContextButton(s, activeBtnClass);
    actions = `
      ${renderStarButton(s, activeBtnClass)}
      ${renderRenameButton(s, activeBtnClass)}
      ${groupBtn}
      ${renderNewChildButton(s, activeBtnClass)}
      ${taskContextBtn}
      ${renderArchiveButton(s, activeBtnClass)}
      ${renderCronGearButton(filter === 'scheduled' ? s.scheduled_task : '', activeBtnClass)}`;
  }
  const indicators = isArchivedRow ? '' : [
      renderSessionIndicators(s),
      renderPendingTriggerIndicator(s),
      renderPendingPlanApprovalIndicator(s),
      s.scheduled_task ? renderScheduledBadge(s) : '',
      renderTuiStatusDot(s),
  ].join('\n    ');
  // The tree chevron leads the indicator slot and the worker glyph closes it,
  // so a tree row keeps the one row shell every other row kind uses.
  const lead = [
      options.treeChildCount ? renderTreeChevron(s.id, options.treeChildCount) : '',
      indicators,
      isWorker ? (isArchivedRow ? renderWorkerDeliveredIcon() : renderWorkerLeafIcon()) : '',
  ].join('\n    ');
  const line = filter === 'scheduled' && s.schedule_cron
      ? renderSessionScheduleLine(s)
      : renderSessionTimeLine(s, timeIso, timeStr, !!options.staticTime);
  return renderSessionRowShell(s, {filter, activeClass, options, indicators: lead, line, actions});
}

function renderSessionList(sessions, filter, options = {}) {
  searchListPainted = (filter === 'search');
  // The grouped paints (All and Scheduled) nest rows and set the tree maps
  // themselves; a flat paint shows each row's own facts.
  lastTreeChildrenOf = new Map();
  lastTreeParentOf = new Map();
  if (filter === 'scheduled') {
    renderGroupedScheduledList(sessions, options);
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
    nav.innerHTML = renderEmptyNote(labels[filter]);
    return;
  }
  // Always use grouped rendering for non-search tabs
  if (filter !== 'search') {
    renderGroupedSessionList(sessions, filter);
    return;
  }
  const truncationHint = filter === 'search' && sessions.length >= 200
    ? renderEmptyNote('Showing the newest 200 matches — narrow the search.')
    : '';
  nav.innerHTML = sessions.map(s => renderSessionItem(s, filter)).join('') + truncationHint;
  resyncSessionUnread(sessions);
  updateRelativeTimes();
  refreshTuiDots();
}

// The rows an inline delete takes with it: the session plus every row whose
// task_parent_id chain within this list reaches it — projected worker-thread
// leaves (task_parent_id = parent id) and task-node descendants at any depth.
// Dropping only the clicked row would let buildSessionTree promote those
// children to top-level roots until the next list fetch, so the repaint walks
// buildSessionTree's own parent relation rather than a second definition. A
// relation cycle in client data ends the walk (the visited set).
function subtreeIdsToRemove(sessions, sessionId) {
  const {childrenOf} = buildSessionTree(sessions);
  const doomed = new Set([sessionId]);
  const pending = [sessionId];
  while (pending.length) {
    for (const child of childrenOf.get(pending.pop()) || []) {
      if (doomed.has(child.id)) continue;
      doomed.add(child.id);
      pending.push(child.id);
    }
  }
  return doomed;
}

// Inline-delete repaint for the grouped views: filter the removed session's
// whole subtree out of the last-rendered list (the args each grouped renderer
// stored at paint time) and repaint in place, so the preview window backfills
// and counts and toggles resync — with no refetch. The archived tab owns its
// own paginated list and the search overlay is keyed by the marker above, so
// both keep the caller's node-only row removal (false).
function removeSessionFromRenderedList(sessionId) {
  if (currentFilter === 'archived' || searchListPainted) return false;
  if (currentFilter === 'scheduled') {
    if (!lastScheduledRenderArgs) return false;
    const doomed = subtreeIdsToRemove(lastScheduledRenderArgs.sessions, sessionId);
    renderGroupedScheduledList(
        lastScheduledRenderArgs.sessions.filter(s => !doomed.has(s.id)),
        {brokenTasks: lastScheduledRenderArgs.brokenTasks});
    return true;
  }
  if (!lastGroupedRenderArgs) return false;
  const doomed = subtreeIdsToRemove(lastGroupedRenderArgs.sessions, sessionId);
  renderGroupedSessionList(
      lastGroupedRenderArgs.sessions.filter(s => !doomed.has(s.id)),
      lastGroupedRenderArgs.filter);
  return true;
}



const GLOBALS = {
  renderEmptyNote,
  openThreadView,
  resetGroupLimitState,
  toggleSessionGroupLimit,
  toggleCronGroupLimit,
  showGroupSelector,
  toggleCronGroup,
  toggleSessionGroup,
  renameGroup,
  deleteGroup,
  renderSessionItem,
  renderSessionList,
  toggleTreeNode,
};
const SIDEBAR_ONLY = {
  TRASH_SVG_PATH,
  starButtonOnclick,
  GEAR_SVG_PATH,
  CHEVRON_SVG_PATH,
  CLOCK_SVG_BODY,
  MODAL_OVERLAY_CLASS,
  MODAL_DIALOG_CLASS,
  removeSessionFromRenderedList,
  resyncSessionUnread,
  buildSessionTree,
  renderSessionTree,
  isTreeNodeExpanded,
  expandTreeNode,
  treeChildIds,
  treeParentId,
  setSessionGroup,
  renderScheduledSessionItem,
  renderGroupedScheduledList,
};
Sidebar.wire(GLOBALS, SIDEBAR_ONLY);

})();
