(function() {
  const Sidebar = globalThis.Sidebar;

const GROUP_SESSION_PREVIEW_LIMIT = 5;
const SESSION_GROUP_LIMIT_STORAGE_KEY = 'session-group-list-expanded';
const SESSION_GROUP_COLLAPSED_STORAGE_KEY = 'session-group-collapsed';
const groupLimitState = {
  [SESSION_GROUP_LIMIT_STORAGE_KEY]: {},
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

function groupLimitItemOptions(key, isHideable, expanded) {
  if (!isHideable) return {};
  const safeKey = escapeHtmlAttr(key);
  const hiddenClass = expanded ? '' : ' hidden';
  return {
    extraClass: `session-group-limit-extra${hiddenClass}`,
    extraAttrs: `data-session-group-limit-extra="${safeKey}"`,
  };
}

function renderGroupLimitToggle(key, hasExtraRows, expanded) {
  if (!hasExtraRows) return '';
  const safeKey = escapeHtmlAttr(key);
  const label = expanded ? 'Show less' : 'Show all';
  return `<button type="button"
          class="session-group-limit-toggle w-full text-left px-3 py-1.5 text-xs text-blue-400 hover:text-blue-300 hover:bg-slate-700/30 rounded-lg transition-colors"
          data-sgroup-limit-toggle-key="${safeKey}"
          aria-expanded="${expanded ? 'true' : 'false'}"
          onclick="event.stopPropagation(); toggleSessionGroupLimit(this.dataset.sgroupLimitToggleKey)">${label}</button>`;
}

function updateGroupLimitDom(key, expanded) {
  document.querySelectorAll('.session-group-limit-extra').forEach(el => {
    if (el.dataset.sessionGroupLimitExtra === key) {
      el.classList.toggle('hidden', !expanded);
    }
  });
  document.querySelectorAll('.session-group-limit-toggle').forEach(btn => {
    if (btn.dataset.sgroupLimitToggleKey === key) {
      btn.textContent = expanded ? 'Show less' : 'Show all';
      btn.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    }
  });
}

function toggleSessionGroupLimit(key) {
  const expanded = !isGroupLimitExpanded(SESSION_GROUP_LIMIT_STORAGE_KEY, key);
  setGroupLimitExpanded(SESSION_GROUP_LIMIT_STORAGE_KEY, key, expanded);
  updateGroupLimitDom(key, expanded);
}

// ---------------------------------------------------------------------------
// Session-row and group-menu registrations
// ---------------------------------------------------------------------------
// A package adds to the session rows and to the group header menu through
// these entries when its script loads. Each entry checks its argument and
// rejects a duplicate.
const sessionRowBadges = [];
const sessionRowLines = [];
const sessionMenuItemSources = [];
const groupMenuItems = [];
const groupPreviewLimitExemptions = [];

// A preview limit exemption: predicate(s) answers true for a root row that
// stays outside the 5-row preview limit. That row always shows and takes no
// rank, so feature packages own row facts without adding them to the core.
function registerGroupPreviewLimitExemption(predicate) {
  if (typeof predicate !== 'function') throw new Error('Invalid group preview limit exemption registration');
  if (groupPreviewLimitExemptions.includes(predicate)) throw new Error('Duplicate group preview limit exemption');
  groupPreviewLimitExemptions.push(predicate);
}

// A row badge: render(s) returns the badge HTML for the row, or ''. The
// badges follow the row's own indicators, in registration order.
function registerSessionRowBadge(render) {
  if (typeof render !== 'function') throw new Error('Invalid session row badge registration');
  if (sessionRowBadges.includes(render)) throw new Error('Duplicate session row badge');
  sessionRowBadges.push(render);
}

// A row line: render(s) returns the HTML of the row's line, or null to keep
// the time line. The first registered line that is not null wins.
function registerSessionRowLine(render) {
  if (typeof render !== 'function') throw new Error('Invalid session row line registration');
  if (sessionRowLines.includes(render)) throw new Error('Duplicate session row line');
  sessionRowLines.push(render);
}

// Settings menu items of a normal row: rowAttrs(s) names the row facts the
// items read, as {camelCaseKey: value}, and the Settings button carries each
// as a data attribute. items(attrs, sessionId) builds the items from the
// button's dataset at click time; they follow the core items.
function registerSessionMenuItems(source) {
  if (!source || typeof source.items !== 'function'
      || (source.rowAttrs !== undefined && typeof source.rowAttrs !== 'function')) {
    throw new Error('Invalid session menu items registration');
  }
  if (sessionMenuItemSources.some(known => known.items === source.items)) {
    throw new Error('Duplicate session menu items');
  }
  sessionMenuItemSources.push(source);
}

// The registered row facts as the Settings button's data attributes: a
// camelCase dataset key is its kebab-case data- attribute.
function renderSessionMenuAttrs(s) {
  return sessionMenuItemSources
    .flatMap(source => Object.entries(source.rowAttrs ? source.rowAttrs(s) : {}))
    .map(([key, value]) => `\n          data-${key.replace(/[A-Z]/g, c => '-' + c.toLowerCase())}="${escapeHtmlAttr(value)}"`)
    .join('');
}

// A group header menu item: onSelect(groupName). The registered items lead
// the menu, in registration order.
function registerGroupMenuItem(item) {
  if (!item || !item.label || typeof item.onSelect !== 'function') {
    throw new Error('Invalid group menu item registration');
  }
  if (groupMenuItems.some(known => known.label === item.label)) {
    throw new Error('Duplicate group menu item: ' + item.label);
  }
  groupMenuItems.push(item);
}

// Session-row action buttons: a markup change to one of these buttons is made
// here once, not in each renderer.
const STAR_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"/>`;

// The one archive-box outline: the Archive action button (a normal row's and
// a worker row's -- archiving is not deletion, so it gets its own icon).
const ARCHIVE_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M20 13V6a2 2 0 00-2-2H6a2 2 0 00-2 2v7m16 0v5a2 2 0 01-2 2H6a2 2 0 01-2-2v-5m16 0h-2.586a1 1 0 00-.707.293l-2.414 2.414a1 1 0 01-.707.293h-3.172a1 1 0 01-.707-.293l-2.414-2.414A1 1 0 006.586 13H4"/>`;

// The one right-chevron outline: the session-group collapse toggle and the
// tree-row expand chevron below. Each call site keeps its own <svg> wrapper.
const CHEVRON_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/>`;

const PLUS_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4"/>`;

// Single source for both pill strips: through the namespace, the sidebar
// filter strip (filters.js) and the archived view's group strip (archived.js).
function filterPillClass(active) {
  return active
    ? 'filter-pill px-2.5 py-1 text-xs rounded-full font-medium transition-colors bg-blue-600/20 text-blue-300'
    : 'filter-pill px-2.5 py-1 text-xs rounded-full font-medium transition-colors text-slate-400 hover:text-slate-200';
}

// The one construction of the star button's onclick body. It renders below
// for a fresh list render and, through the namespace, filters.js's
// toggleSessionStar re-applies it after a star toggle
// (Sidebar.starButtonOnclick). The starred argument is the state the next
// click should toggle away from.
function starButtonOnclick(id, starred) {
  return `event.preventDefault(); event.stopPropagation(); sidebarControl('toggleSessionStar', '${id}', ${starred})`;
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
// actions: a markup change is made here once, not in each renderer. The star's
// dynamic fill and id and the Settings button's data attributes stay at
// their own renderers.
function renderRowActionButton(onclick, colorClass, title, svgBody, activeBtnClass) {
  return `<button onclick="${onclick}"
          class="opacity-0 group-hover:opacity-100 p-1 ${colorClass} transition-opacity flex-shrink-0 ${activeBtnClass}" title="${title}">
    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">${svgBody}</svg>
  </button>`;
}

function renderArchiveButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); sidebarControl('archiveSession', '${s.id}')`,
      'hover:text-red-400',
      'Archive',
      ARCHIVE_SVG_PATH,
      activeBtnClass);
}

// Renders '' for a falsy taskName: the caller's show-guard stays visible in
// the argument it passes.
// A logical session row creates a child logical session under itself: the
// same creation body as New Session with this row as the parent.
function renderNewChildButton(s, activeBtnClass) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); sidebarControl('createChildSession', '${s.id}')`,
      'hover:text-green-400',
      'New child session',
      PLUS_SVG_PATH,
      activeBtnClass);
}

// The row's Settings hover button: opens the shared row menu (row-menu.js)
// with the facts its items need as data attributes. Those facts are the
// current group, task-tree parent, profile and registered row facts. It
// passes them because openSessionRowMenu (session-menus.js) reads them off
// the anchor at click time. The optional rowMenu argument marks which item list the anchor's row
// builds: the archived row reuses the markup and the data attributes
// (archived.js's [data-current-group] rewrite included). Only the menu
// differs. It writes its own markup the way the star does: the frame renderer
// takes no extra attributes.
function renderSettingsButton(s, activeBtnClass, rowMenu) {
  const menuMarker = rowMenu ? ` data-row-menu="${rowMenu}"` : '';
  return `<button onclick="event.preventDefault(); event.stopPropagation(); sidebarControl('openSessionRowMenu', this, '${s.id}')"
          title="Settings"${menuMarker}
          data-current-group="${s.group ? escapeHtmlAttr(s.group) : ''}"
          data-task-parent="${escapeHtmlAttr(s.task_parent_id || '')}"
          data-profile="${escapeHtmlAttr(s.profile || '')}"${renderSessionMenuAttrs(s)}
          class="opacity-0 group-hover:opacity-100 p-1 text-slate-500 hover:text-slate-300 transition-opacity flex-shrink-0 ${activeBtnClass}">
    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">${Sidebar.GEAR_SVG_PATH}<circle cx="12" cy="12" r="3"/></svg>
  </button>`;
}

// Session-row highlight: a tint change is made here once, not in each renderer.
function sessionRowActiveClass(isActive) {
  return isActive ? 'bg-blue-600/20 text-blue-300' : 'hover:bg-slate-700/50 text-slate-300';
}

// The one session-row frame: the anchor open tag, the name span, and the
// closing tag.
// A markup change to the row frame is made here once, not in each renderer.
// indicators and line are prebuilt strings — the status column and the content
// after the name span — and actions the trailing button column. The anchor's
// session-row class is the plain-CSS hook styles.css's hover reveal keys on
// (positioning context for the row's out-of-flow action buttons).
function renderSessionRowShell(s, {filter, activeClass, options, indicators, line, actions}) {
  const extraClass = options.extraClass ? ' ' + options.extraClass : '';
  const extraAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  return `<a href="/?session=${s.id}&filter=${filter}"
     class="session-row group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}${extraClass}" ondblclick="sidebarControl('startRename', event, '${s.id}')"
     onclick="event.preventDefault(); sidebarControl('switchSession', '${s.id}')"
     id="session-${s.id}"${extraAttrs}>
    ${indicators}
    <span class="flex-1 min-w-0">
      <span class="truncate block session-name">${escapeHtml(s.name)}</span>
      ${line}
    </span>
    ${actions}
  </a>`;
}

// Empty list note shared by the grouped/search lists here and, through the
// namespace, the archived list in archived.js. A markup change is made here
// once, not in each list's empty branch.
function renderEmptyNote(text) {
  return `<p class="text-slate-500 text-sm px-3 py-2">${text}</p>`;
}

// Every sidebar render path rebuilds rows in place from a session list, so the
// unread map must be refolded from that same list or unread badges go stale.
function resyncSessionUnread(sessions) {
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
}

let lastGroupedRenderArgs = null;

// The Workspace badges' async repaint: the first paint never waits for the
// badge loaders (filters.js registerWorkspaceBadge). This repaint sources the
// first paint's own args, with no list refetch. It discards itself when the
// view has already moved on (another filter, or the search overlay on screen).
function paintWorkspaceBadges(workspaceBadges) {
  if (currentFilter !== 'all' || searchListPainted) return;
  if (!lastGroupedRenderArgs || lastGroupedRenderArgs.filter !== 'all') return;
  renderGroupedSessionList(lastGroupedRenderArgs.sessions, 'all', {workspaceBadges});
}
// Whether the last renderSessionList call painted the search overlay. The
// search flow never assigns currentFilter, so this marker — not the filter
// state — is what tells the delete path a flat search list is on screen.
let searchListPainted = false;

// ---------------------------------------------------------------------------
// Grouped session list rendering (by session.group)
// ---------------------------------------------------------------------------
function renderGroupedSessionList(sessions, filter, options = {}) {
  const nav = options.container || document.getElementById('session-list');
  lastGroupedRenderArgs = {sessions, filter, workspaceBadges: options.workspaceBadges || ''};
  if (!sessions.length) {
    nav.innerHTML = (options.workspaceBadges || '') + renderEmptyNote('No sessions yet');
    return;
  }
  // Grouping follows the root rows; a child row nests under its parent
  // whatever its own group field says.
  const {roots, childrenOf, parentOf} = buildSessionTree(sessions);
  recordTreeShape(childrenOf, parentOf);
  revealActiveSessionOnce(parentOf);
  // The active session's root controls visibility for its whole subtree.
  let activeRootId = SESSION_ID;
  while (activeRootId && parentOf.has(activeRootId)) activeRootId = parentOf.get(activeRootId);
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
    let previewRank = 0;
    let hasExtraRows = false;
    const groupRows = groupSessions.map((s) => {
      // An exempt root always shows and takes no rank, so the limit counts
      // only the other roots; those past it are the extra rows.
      const exempt = groupPreviewLimitExemptions.some((predicate) => predicate(s));
      const inPreview = !exempt && previewRank < GROUP_SESSION_PREVIEW_LIMIT;
      if (!exempt) previewRank += 1;
      // The open session's root never hides, since a hidden root hides its subtree.
      const isHideable = !exempt && !inPreview && s.id !== activeRootId;
      // Show all draws only when the group hides a root, so a lone visible
      // extra row never renders an empty toggle.
      if (isHideable) hasExtraRows = true;
      return {
        session: s,
        options: groupLimitItemOptions(key, isHideable, isLimitExpanded),
      };
    });

    // The Threads view hides the New-session-in-group button: a session
    // created there has no platform origin and would fall back to Workspace.
    // The gate derives from the filter, not the call options, so the in-place
    // repaints (removeSessionFromRenderedList, paintWorkspaceBadges) keep it
    // hidden too; the Settings gear stays.
    const createInGroupButton = filter !== 'threads' ? `
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); sidebarControl('createSessionInGroup', this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-green-400 transition-opacity" title="New session in group">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">${PLUS_SVG_PATH}</svg>
      </button>` : '';
    const groupActions = key && options.groupActions !== false ? `${createInGroupButton}
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); sidebarControl('openGroupHeaderMenu', this)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity" title="Settings">
        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">${Sidebar.GEAR_SVG_PATH}<circle cx="12" cy="12" r="3"/></svg>
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
        ${groupRows.map(({session, options: rowOptions}) => renderSessionTree(
          session,
          filter,
          {...rowOptions, staticTime: !!options.staticTime},
          childrenOf
        )).join('')}
        ${renderGroupLimitToggle(key, hasExtraRows, isLimitExpanded)}
      </div>
    </div>`;
  }
  nav.innerHTML = (options.workspaceBadges || '') + html;
  resyncSessionUnread(sessions);
  // Parent rows take their collapsed-subtree stand-ins now that the rows exist.
  if (typeof Sidebar.refreshTreeIndicators === 'function') Sidebar.refreshTreeIndicators();
  updateRelativeTimes();
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

// A group-header create (session-view.js) marks its target expanded before the
// create's repaint, so the new row appears visible inside the group it names.
function expandSessionGroup(key) {
  const collapsedState = loadGroupCollapsedState(SESSION_GROUP_COLLAPSED_STORAGE_KEY);
  collapsedState[key] = false;
  localStorage.setItem(SESSION_GROUP_COLLAPSED_STORAGE_KEY, JSON.stringify(collapsedState));
}

// Model label for a session row: BACKEND_OPTIONS carries the config-authored
// label ("CC · Opus 5"). The row shows it without the backend-family prefix
// because the 320px sidebar leaves only ~67px next to the timestamp. Returns
// null when the session carries no backend id.
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
// The expanded set is held in memory only, so a reload starts every parent
// collapsed (a persisted expand state once hid a preview-cap regression).
const treeExpandedNodes = new Set();
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

// The active session's ancestors open once per switch, so a node reached by
// a deep link or a fresh create is visible. A later manual collapse holds
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

// The count includes direct children whose profile is not 'worker'; worker
// leaves stay nested but do not add to it. All and Threads hold every
// unarchived child, so the count reflects active non-worker child sessions.
function renderTreeChildCount(sessionId, nonWorkerChildCount) {
  const title = `${nonWorkerChildCount} active child session${nonWorkerChildCount === 1 ? '' : 's'}`;
  return `<span data-tree-child-count="${sessionId}"
         class="text-[10px] leading-none text-slate-400 tabular-nums flex-shrink-0"
         title="${title}">${nonWorkerChildCount}</span>`;
}

// The worker glyph leads its row — the chevron's slot at the chevron's
// size, in every paint tree or flat — and a worker row never draws a chevron.
function renderWorkerLeafIcon() {
  return `<svg class="w-3 h-3 text-slate-500 flex-shrink-0" title="Worker (implementation leaf)" fill="none" stroke="currentColor" viewBox="0 0 24 24">${LEAF_SVG_PATH}</svg>`;
}

// An archived worker delivered its work: the row leads with a check instead
// of the leaf glyph.
const CHECK_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7"/>`;

function renderWorkerDeliveredIcon() {
  return `<svg class="w-3 h-3 text-green-500 flex-shrink-0" title="Worker (delivered)" fill="none" stroke="currentColor" viewBox="0 0 24 24">${CHECK_SVG_PATH}</svg>`;
}

// One row followed by its subtree. The outer wrapper carries the row's
// group-limit class and attributes, so a root hidden by the 5-row preview
// hides its subtree with it and Show all reveals both. The inner container
// carries the expand state. Each level indents 22px behind a guide line.
// Every row rendered here receives treeChildCount, 0 for a leaf. Its presence
// is the one marker that tells a row renderer a tree row from a flat one.
// The flat paints (search results) pass none.
function renderSessionTree(s, filter, options, childrenOf) {
  const children = childrenOf.get(s.id) || [];
  const treeNonWorkerChildCount = children.filter(child => child.profile !== 'worker').length;
  const row = renderSessionItem(s, filter, {
    ...options,
    treeChildCount: children.length,
    treeNonWorkerChildCount,
  });
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

// Expansion is display only: child rows show or hide and the chevron rotates.
// A row's icon reads facts only (status.js paints from states, the unread map
// and the last grouped paint's tree), so no indicator repaint follows a toggle.
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
  // A context row (the archived tree's unarchived ancestor) renders dimmed
  // with an active tag. It is not a row the user archived, so it takes none
  // of the archived row's unarchive/delete actions and none of the live row's
  // mutating ones.
  const isContextRow = filter === 'archived' && s.context_only === true;
  // A worker leaf is identified by its icon and carries the archive action alone.
  const isWorker = s.profile === 'worker';
  const isWorkerRow = isWorker && !isArchivedRow;
  const activeClass = sessionRowActiveClass(isActive);
  const activeBtnClass = isActive ? '!opacity-100' : '';
  const timeStr = s.updated_at ? relativeTime(s.updated_at) : '';
  const timeIso = s.updated_at || '';
  let actions = '';
  if (isContextRow) {
    actions = renderStarButton(s, activeBtnClass);
  } else if (isArchivedRow) {
    // The archived row: three direct buttons; the group move and the delete
    // moved into the archived Settings menu (openArchivedRowMenu in
    // session-menus.js).
    actions = `
      ${renderStarButton(s, activeBtnClass)}
      ${renderRowActionButton(
          `event.preventDefault(); event.stopPropagation(); sidebarControl('unarchiveSession', '${s.id}')`,
          'hover:text-green-400',
          'Unarchive',
          '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5-5m0 0l5 5m-5-5v12"/>',
          activeBtnClass)}
      ${renderSettingsButton(s, activeBtnClass, 'archived')}`;
  } else if (isWorkerRow) {
    actions = renderArchiveButton(s, activeBtnClass);
  } else {
    // A normal row (manager or legacy): four direct buttons; rename, group
    // move, Task & context and the registered items moved into the Settings
    // menu (openSessionRowMenu in session-menus.js).
    actions = `
      ${renderStarButton(s, activeBtnClass)}
      ${renderNewChildButton(s, activeBtnClass)}
      ${renderArchiveButton(s, activeBtnClass)}
      ${renderSettingsButton(s, activeBtnClass)}`;
  }
  const indicators = isArchivedRow ? '' : [
      renderSessionIndicators(s),
      renderPendingTriggerIndicator(s),
      renderPendingPlanApprovalIndicator(s),
      ...sessionRowBadges.map(render => render(s)),
  ].join('\n    ');
  // Every tree row leads with one 12px marker so a level's markers line up
  // in one column. A logical row draws the chevron even with no children:
  // same markup and toggle behavior, with nothing to reveal below it. A
  // worker row draws the worker glyph instead, never a chevron. A flat row
  // (search results, the Archived tab) draws neither. The indicators follow
  // the lead marker.
  // The count beside the chevron uses direct children whose profile is not
  // 'worker'; worker leaves still nest, and the chevron title still counts
  // every child task. Only the all and threads paints draw the count.
  const showChildCount = 'treeChildCount' in options
      && options.treeNonWorkerChildCount > 0
      && (filter === 'all' || filter === 'threads');
  const lead = [
      isWorker
          ? (isArchivedRow ? renderWorkerDeliveredIcon() : renderWorkerLeafIcon())
          : ('treeChildCount' in options ? renderTreeChevron(s.id, options.treeChildCount) : ''),
      showChildCount ? renderTreeChildCount(s.id, options.treeNonWorkerChildCount) : '',
      indicators,
  ].join('\n    ');
  // The first registered row line that answers replaces the time line.
  const line = sessionRowLines.reduce((found, render) => found ?? render(s), null)
      ?? renderSessionTimeLine(s, timeIso, timeStr, !!options.staticTime);
  // A context row dims and carries the active tag after the line, as the mock
  // shows it. The tag is placed outside the name span so a long name cannot
  // push it out.
  return renderSessionRowShell(s, {
    filter,
    activeClass: activeClass + (isContextRow ? ' opacity-60' : ''),
    options,
    indicators: lead,
    line,
    actions: actions + (isContextRow
      ? '<span class="px-1.5 py-0.5 rounded bg-slate-700 text-[10px] font-medium text-slate-400 flex-shrink-0">active</span>'
      : ''),
  });
}

function renderSessionList(sessions, filter, options = {}) {
  searchListPainted = (filter === 'search');
  // The grouped paints (Workspace and Later) nest rows and record their tree
  // shape themselves; a flat paint shows each row's own facts.
  recordTreeShape(new Map(), new Map());
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    const labels = {
      all: 'No sessions yet',
      threads: 'No chat threads',
      starred: 'No starred sessions',
      archived: 'No archived sessions',
      search: 'No matching sessions',
    };
    // The Workspace badges appear on the empty paint too: with zero sessions a badge
    // is still the one thing paging the maintainer.
    nav.innerHTML = (filter === 'all' ? (options.workspaceBadges || '') : '')
      + renderEmptyNote(labels[filter]);
    return;
  }
  // Always use grouped rendering for non-search tabs
  if (filter !== 'search') {
    renderGroupedSessionList(sessions, filter, options);
    return;
  }
  const truncationHint = filter === 'search' && sessions.length >= 200
    ? renderEmptyNote('Showing the newest 200 matches — narrow the search.')
    : '';
  nav.innerHTML = sessions.map(s => renderSessionItem(s, filter)).join('') + truncationHint;
  resyncSessionUnread(sessions);
  updateRelativeTimes();
}

// The rows an inline delete takes with it: the session plus every row whose
// task_parent_id chain within this list reaches it. That is worker leaves and
// task-node descendants at any depth.
// Dropping only the clicked row would let buildSessionTree promote those
// children to top-level roots until the next list fetch. So the repaint walks
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
// whole subtree out of the last-rendered list and repaint in place. That list
// is the one the grouped renderer stored at paint time. The preview window
// then backfills and counts and toggles resync, with no refetch. The archived
// view builds its own paginated tree and the search overlay is keyed by the
// marker above, so both keep the caller's node-only row removal (false).
function removeSessionFromRenderedList(sessionId) {
  if (currentFilter === 'archived' || searchListPainted) return false;
  if (!lastGroupedRenderArgs) return false;
  const doomed = subtreeIdsToRemove(lastGroupedRenderArgs.sessions, sessionId);
  renderGroupedSessionList(
      lastGroupedRenderArgs.sessions.filter(s => !doomed.has(s.id)),
      lastGroupedRenderArgs.filter,
      {workspaceBadges: lastGroupedRenderArgs.workspaceBadges});
  return true;
}



const GLOBALS = {
  registerGroupPreviewLimitExemption,
  registerSessionRowBadge,
  registerSessionRowLine,
  registerSessionMenuItems,
  registerGroupMenuItem,
  renderEmptyNote,
  paintWorkspaceBadges,
  resetGroupLimitState,
  toggleSessionGroupLimit,
  toggleSessionGroup,
  renderSessionItem,
  renderSessionList,
  toggleTreeNode,
};
const SIDEBAR_ONLY = {
  sessionMenuItemSources,
  groupMenuItems,
  filterPillClass,
  starButtonOnclick,
  removeSessionFromRenderedList,
  resyncSessionUnread,
  buildSessionTree,
  renderSessionTree,
  isTreeNodeExpanded,
  expandTreeNode,
  expandSessionGroup,
  renderGroupedSessionList,
};
Sidebar.wire(GLOBALS, SIDEBAR_ONLY);

})();
