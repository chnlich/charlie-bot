(function() {
  const Sidebar = globalThis.Sidebar;

// The rows a group's preview paints: the rows that fit the sidebar's list, shared by the groups it holds, so the
// panel's free space goes to session rows. A row's two lines, each group's header and its "Show all" line set the
// arithmetic. The page carries at most SIDEBAR_SCREEN_CAPACITY roots per group
// (rust/crates/runtime/session-api/src/listing.rs), the most this limit asks for.
const GROUP_SESSION_PREVIEW_MIN = 5;
const GROUP_SESSION_PREVIEW_MAX = 20;
const GROUP_SESSION_ROW_PX = 52;
const SESSION_GROUP_HEADER_PX = 28;
const SESSION_GROUP_LIMIT_TOGGLE_PX = 28;

// The rows one group's preview paints come from the height its list holds and the groups that share it.
// Each group's header and "Show all" line come out first, and the groups split the rest, never below the
// minimum. The phone's drawer has no height until it opens, and the archived panel's container grows with
// its rows, so both keep the minimum.
function groupPreviewLimit(nav, groupCount) {
  const height = nav && nav === document.getElementById('session-list') ? nav.clientHeight : 0;
  if (!height) return GROUP_SESSION_PREVIEW_MIN;
  const groups = Math.max(1, groupCount || 1);
  const fixed = groups * (SESSION_GROUP_HEADER_PX + SESSION_GROUP_LIMIT_TOGGLE_PX);
  const rows = Math.floor((height - fixed) / GROUP_SESSION_ROW_PX);
  return Math.max(GROUP_SESSION_PREVIEW_MIN,
    Math.min(GROUP_SESSION_PREVIEW_MAX, Math.floor(rows / groups)));
}
// The matches one search answer carries: the route keeps the newest `SEARCH_RESULT_LIMIT` of them
// (rust/crates/runtime/session-api/src/search.rs). The search list states that cap from this number, so a drift
// shows the notice late or not at all, never a row.
const SEARCH_MATCH_LIMIT = 200;
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

function renderGroupLimitToggle(key, expanded) {
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
  if (expanded && lastGroupedRenderArgs && lastGroupedRenderArgs.listing?.partial) {
    loadFullListing(key);
    return;
  }
  updateGroupLimitDom(key, expanded);
}

// The page carries only the rows the first screen paints, so a group's hidden rows arrive with the full listing. The
// reader's "Show all" fetches it and repaints the list with the group expanded.
function loadFullListing(key) {
  const button = Array.from(document.querySelectorAll('.session-group-limit-toggle'))
    .find(btn => btn.dataset.sgroupLimitToggleKey === key);
  if (button) {
    button.textContent = 'Loading…';
    button.disabled = true;
  }
  const requestSeq = factSeqAtRequest();
  fetch('/api/sessions/?compact=1')
    .then(res => {
      if (!res.ok) throw new Error(`Listing fetch failed: ${res.status}`);
      return res.json();
    })
    .then(payload => {
      // A view the reader left while the fetch was in flight takes no rows: its own load paints it.
      if (currentFilter !== 'all' || searchListPainted) return;
      const sessions = Sidebar.expandSessionColumns(payload);
      applyListingFacts(sessions, requestSeq);
      renderSessionList(sessions, 'all',
        {workspaceBadges: lastGroupedRenderArgs ? lastGroupedRenderArgs.workspaceBadges : ''});
    })
    .catch(err => {
      console.error('Listing fetch failed:', err);
      if (button && button.isConnected) {
        button.textContent = 'Show all';
        button.disabled = false;
      }
    });
}

// ---------------------------------------------------------------------------
// Session-row and group-menu registrations
// ---------------------------------------------------------------------------
// A package adds to the session rows and to the group header menu through
// these entries when its script loads. Each entry checks its argument and
// rejects a duplicate.
const sessionRowBadges = [];
const sessionRowTitleBadges = [];
const sessionRowLines = [];
const sessionMenuItemSources = [];
const groupMenuItems = [];
const groupPreviewLimitExemptions = [];

// A preview limit exemption: predicate(s) answers true for a root row that
// stays outside the preview limit. That row always shows and takes no rank, so
// feature packages own row facts without adding them to the core.
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

// A title badge: render(s) returns the badge HTML that sits on the row's title line after the name, or ''. The badge
// takes no width from the line under the title, which keeps the full width of the name block.
function registerSessionRowTitleBadge(render) {
  if (typeof render !== 'function') throw new Error('Invalid session row title badge registration');
  if (sessionRowTitleBadges.includes(render)) throw new Error('Duplicate session row title badge');
  sessionRowTitleBadges.push(render);
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
// button's dataset at click time. They follow the core items.
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

// The expand chevron draws the collapsed disclosure icon; an expanded one
// turns a quarter with the rotate-90 class. A toggle flips that class on the
// chevron in place, so the turn animates (toggleSessionGroup, applyTreeNodeExpansion).
function expandChevron(expanded, cls, attrs = '') {
  return cbIcon('disclosure', 12, expanded ? `${cls} rotate-90` : cls, attrs);
}

const SESSION_GROUP_CHEVRON_CLASS = 'text-slate-500 transition-transform session-group-chevron';

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

function renderStarButton(s) {
  const starClass = s.starred ? 'text-yellow-400 !opacity-100' : 'hover:text-yellow-400';
  return `<button onclick="${starButtonOnclick(s.id, s.starred)}"
          class="opacity-0 group-hover:opacity-100 p-1 transition-opacity flex-shrink-0 star-btn ${starClass}" title="Star" id="star-${s.id}">
    ${cbIcon(s.starred ? 'starred' : 'star', 14)}
  </button>`;
}

// The one hover-revealed icon button frame for the session row's plain
// actions: a markup change is made here once, not in each renderer. The star's
// dynamic key and id and the Settings button's data attributes stay at
// their own renderers.
function renderRowActionButton(onclick, colorClass, title, iconKey) {
  return `<button onclick="${onclick}"
          class="opacity-0 group-hover:opacity-100 p-1 ${colorClass} transition-opacity flex-shrink-0" title="${title}">
    ${cbIcon(iconKey, 14)}
  </button>`;
}

function renderArchiveButton(s) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); sidebarControl('archiveSession', '${s.id}')`,
      'hover:text-red-400',
      'Archive',
      'archive');
}

// Renders '' for a falsy taskName: the caller's show-guard stays visible in
// the argument it passes.
// A logical session row creates a child logical session under itself: the
// same creation body as New Session with this row as the parent.
function renderNewChildButton(s) {
  return renderRowActionButton(
      `event.preventDefault(); event.stopPropagation(); sidebarControl('createChildSession', '${s.id}')`,
      'hover:text-green-400',
      'New child session',
      'add');
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
// The gear is the row's one always-visible control on a touch screen, and on a
// pointer device it shows over the row's hover cover. Its glyph is then the
// control's only mark, so it takes slate-400. On the open row's blue tint
// rgb(26, 45, 88) slate-500 draws 2.82:1, and on the hovered row's cover
// rgb(37, 49, 68) it draws 2.77:1. Both fall under the 3:1 floor WCAG 1.4.11
// sets for a component. slate-400 draws 5.23:1 and 5.14:1 there and 6.34:1 on
// the plain row, the tone the row's time line keeps.
function renderSettingsButton(s, rowMenu) {
  const menuMarker = rowMenu ? ` data-row-menu="${rowMenu}"` : '';
  return `<button onclick="event.preventDefault(); event.stopPropagation(); sidebarControl('openSessionRowMenu', this, '${s.id}')"
          title="Settings"${menuMarker}
          data-current-group="${s.group ? escapeHtmlAttr(s.group) : ''}"
          data-task-parent="${escapeHtmlAttr(s.task_parent_id || '')}"
          data-profile="${escapeHtmlAttr(s.profile || '')}"${renderSessionMenuAttrs(s)}
          class="opacity-0 group-hover:opacity-100 p-1 text-slate-400 hover:text-slate-300 transition-opacity flex-shrink-0">
    ${cbIcon('settings', 14, 'block')}
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
// after the title line — and actions the trailing button column. titleBadges
// is the prebuilt badge strings that sit on the title line after the name. The
// anchor's session-row class is the plain-CSS hook styles.css's hover reveal
// keys on (positioning context for the row's out-of-flow action buttons).
// The name span's title is the full name on every row. A title the row rule
// shortened (sessionRowTitle) reads whole on hover, and the rename field, the
// switch notice and the archive confirmation read the name from the same
// attribute. updateSidebarSessionName keeps it current on rename.
function renderSessionRowShell(s, {filter, title, activeClass, options, indicators, nameTag, titleBadges, line, actions}) {
  const extraClass = options.extraClass ? ' ' + options.extraClass : '';
  const extraAttrs = options.extraAttrs ? ' ' + options.extraAttrs : '';
  return `<a href="/?session=${s.id}&filter=${filter}"
     class="session-row group flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors ${activeClass}${extraClass}" ondblclick="sidebarControl('startRename', event, '${s.id}')"
     onclick="event.preventDefault(); sidebarControl('switchSession', '${s.id}')"
     id="session-${s.id}"${extraAttrs}>
    ${indicators}
    <span class="session-row-name flex-1 min-w-0">
      <span class="flex items-center gap-1.5 min-w-0">
        <span class="truncate flex-1 min-w-0 session-name" title="${escapeHtmlAttr(s.name)}">${escapeHtml(title)}</span>
        ${nameTag}${titleBadges}
      </span>
      ${line}
    </span>
    ${actions}
  </a>`;
}

// Empty list note shared by the grouped/search lists here and, through the
// namespace, the archived list in archived.js. A markup change is made here
// once, not in each list's empty branch. The note takes the tone of the group
// label it stands in for. Slate-400 reads 6.34:1 on the sidebar, where
// slate-500 read 3.42:1, under the 4.5:1 floor for this 14px text.
function renderEmptyNote(text) {
  return `<p class="text-slate-400 text-sm px-3 py-2">${text}</p>`;
}

// The search overlay's pending and failure notes. They are search paints too,
// so they set the same marker a result paint does, and the Workspace badge
// repaint keeps its rows off this view.
function renderSearchNote(text) {
  searchListPainted = true;
  const nav = document.getElementById('session-list');
  if (nav) nav.innerHTML = renderEmptyNote(text);
}

let lastGroupedRenderArgs = null;

// The preview limit the last paint of the sidebar's list used, and the group count it shared that limit over. The
// observer below repaints only when the list's height moves the limit, so a resize inside one row's band costs no
// paint.
let paintedPreviewLimit = null;
let paintedPreviewGroups = 1;

// The Workspace badges' async repaint: the first paint never waits for the
// badge loaders (filters.js registerWorkspaceBadge). This repaint sources the
// first paint's own args, with no list refetch. It discards itself when the
// view has already moved on (another filter, or the search overlay on screen).
function paintWorkspaceBadges(workspaceBadges) {
  if (currentFilter !== 'all' || searchListPainted) return;
  if (!lastGroupedRenderArgs || lastGroupedRenderArgs.filter !== 'all') return;
  renderSessionList(lastGroupedRenderArgs.sessions, 'all', {workspaceBadges, listing: lastGroupedRenderArgs.listing});
}
// A module the page fetched after its document registers a row decoration (a badge, a line, a menu item) after the
// list painted. The paint it missed leaves the rows it decorates bare. It asks for this repaint, which paints the
// last paint's own arguments again. That means no list refetch: an empty or search paint is redrawn as itself,
// rather than replaced by the rows an earlier paint left behind.
let lastListPaint = null;

// The repaint draws the last paint only where the search box agrees with it: a search paint into a non-empty box, any
// other paint into an empty one. The search note paints over a Workspace paint, so a repaint during the note draws
// nothing, and a search paint left on screen after its box is cleared draws nothing either.
function repaintRenderedRows() {
  if (!lastListPaint) return;
  const {sessions, filter, options} = lastListPaint;
  if ((filter === 'search') !== searchBoxHasText()) return;
  renderSessionList(sessions, filter, options);
}

// The clear's paint of the Workspace: the rows its last paint drew, from memory and with no fetch. renderSessionList
// records them as lastListPaint and clears searchListPainted, so a resize repaint after the clear draws the Workspace,
// and the badge repaint resumes. Returns false when memory holds no Workspace paint.
function restoreWorkspaceFromMemory() {
  const memory = lastGroupedRenderArgs;
  if (!memory || memory.filter !== 'all') return false;
  renderSessionList(memory.sessions, 'all', {workspaceBadges: memory.workspaceBadges, listing: memory.listing});
  return true;
}

// Whether a row refresh moves rows into or out of the Workspace the memory holds: a named row the list lacks, or a
// gone id the list holds. Such a change alters the groups' counts, which only a full listing carries.
function rowsChangeMembership(rows, goneIds) {
  const held = new Set(lastGroupedRenderArgs.sessions.map(s => s.id));
  return rows.some(row => !held.has(row.id)) || goneIds.some(id => held.has(id));
}

// The sidebar's list holds a fixed height, and the rows its preview paints follow that height. The phone's
// drawer opening gives the list its first height, and a window resize changes it. A height that keeps the same
// row count repaints nothing.
function watchSessionListHeight() {
  const nav = document.getElementById('session-list');
  if (!nav || typeof ResizeObserver !== 'function') return;
  new ResizeObserver(() => {
    if (groupPreviewLimit(nav, paintedPreviewGroups) === paintedPreviewLimit) return;
    repaintRenderedRows();
  }).observe(nav);
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
  const listing = options.listing || null;
  lastGroupedRenderArgs = {sessions, filter, workspaceBadges: options.workspaceBadges || '', listing};
  if (!sessions.length) {
    nav.innerHTML = (options.workspaceBadges || '') + renderEmptyNote('No sessions yet');
    return;
  }
  // Grouping follows the root rows. A child row nests under its parent
  // whatever its own group field says.
  const {roots, childrenOf, parentOf} = buildSessionTree(sessions);
  recordTreeShape(childrenOf, parentOf, isTreeNodeExpanded);
  revealActiveSessionOnce(parentOf);
  // The active session's root controls visibility for its whole subtree.
  let activeRootId = SESSION_ID;
  while (activeRootId && parentOf.has(activeRootId)) activeRootId = parentOf.get(activeRootId);
  const {groups, sortedKeys} = groupSessionsBySortedKeys(roots, s => s.group);
  const collapsedState = loadGroupCollapsedState(SESSION_GROUP_COLLAPSED_STORAGE_KEY);
  const limitState = loadGroupLimitState(SESSION_GROUP_LIMIT_STORAGE_KEY);
  const previewLimit = groupPreviewLimit(nav, sortedKeys.length);
  if (nav === document.getElementById('session-list')) {
    paintedPreviewLimit = previewLimit;
    paintedPreviewGroups = sortedKeys.length;
  }

  let html = '';
  for (const key of sortedKeys) {
    const label = key || '(No group)';
    const groupSessions = groups[key];
    const isCollapsed = collapsedState[key] === true; // expanded by default
    const isLimitExpanded = limitState[key] === true;
    const safeKey = escapeHtmlAttr(key);
    // The open session's row joins the preview from past the limit, so the group paints one row more
    // than groupPreviewLimit budgets for the list's height. The preview gives up its last row for it:
    // the header, the open row and the Show all line then stay inside the panel's height. At the
    // preview's floor the height fits fewer rows than the floor paints (a closed phone drawer reports
    // no height at all). The floor keeps its rows, and the open row joins them still.
    const previewRoots =
      groupSessions.filter((root) => !groupPreviewLimitExemptions.some((predicate) => predicate(root)));
    const openRootRank = previewRoots.findIndex((root) => root.id === activeRootId);
    const previewRows = openRootRank >= previewLimit && previewLimit > GROUP_SESSION_PREVIEW_MIN
      ? previewLimit - 1
      : previewLimit;
    let previewRank = 0;
    const groupRows = groupSessions.map((s) => {
      // An exempt root always shows and takes no rank, so the limit counts
      // only the other roots. Those past it are the extra rows.
      const exempt = groupPreviewLimitExemptions.some((predicate) => predicate(s));
      const inPreview = !exempt && previewRank < previewRows;
      if (!exempt) previewRank += 1;
      // The open session's root never hides, since a hidden root hides its subtree.
      const isHideable = !exempt && !inPreview && s.id !== activeRootId;
      return {
        session: s,
        options: groupLimitItemOptions(key, isHideable, isLimitExpanded),
      };
    });

    // The open session's row is never hidden by the limit. A row past the
    // preview would otherwise render after the toggle, so the control that
    // reveals the rest would stand above the row the reader is on. The open
    // row joins the preview rows instead, and the toggle follows it, still
    // leading the rows it hides.
    const firstHidden = groupRows.findIndex((row) => row.options.extraClass);
    const openRow = groupRows.findIndex((row) => row.session.id === activeRootId);
    if (firstHidden !== -1 && openRow > firstHidden) {
      groupRows.splice(firstHidden, 0, groupRows.splice(openRow, 1)[0]);
    }

    // The Threads view hides the New-session-in-group button: a session
    // created there has no platform origin and would fall back to Workspace.
    // The gate derives from the filter, not the call options, so the in-place
    // repaints (removeSessionFromRenderedList, paintWorkspaceBadges) keep it
    // hidden too. The Settings gear stays.
    const createInGroupButton = filter !== 'threads' ? `
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); sidebarControl('createSessionInGroup', this.dataset.groupName)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-green-400 transition-opacity" title="New session in group">
        ${cbIcon('add', 12, 'block')}
      </button>` : '';
    const groupActions = key && options.groupActions !== false ? `${createInGroupButton}
      <button data-group-name="${safeKey}"
              onclick="event.stopPropagation(); sidebarControl('openGroupHeaderMenu', this)"
              class="opacity-0 group-hover:opacity-100 p-0.5 text-slate-500 hover:text-slate-300 transition-opacity" title="Settings">
        ${cbIcon('settings', 12, 'block')}
      </button>` : '';

    // The limit toggle sits in front of the first row it hides. Expanding then
    // leaves Show less where the reader clicked it, not below every extra row.
    let limitTogglePlaced = false;
    const groupItemsHtml = groupRows.map(({session, options: rowOptions}) => {
      let limitToggle = '';
      if (rowOptions.extraClass && !limitTogglePlaced) {
        limitToggle = renderGroupLimitToggle(key, isLimitExpanded);
        limitTogglePlaced = true;
      }
      return limitToggle + renderSessionTree(
        session,
        filter,
        {...rowOptions, staticTime: !!options.staticTime, groupHeader: key},
        childrenOf
      );
    }).join('');
    // A partial listing whose rows all fit the preview has no hidden row to stand the toggle at. The toggle
    // stands after the rows it fetches, and the click loads the full listing. A page that carries rows past the
    // preview already placed it in front of them.
    const groupRowCount = countTreeRows(groupSessions, childrenOf);
    const groupCount = listing && listing.counts && Object.prototype.hasOwnProperty.call(listing.counts, key)
      ? listing.counts[key]
      : groupRowCount;
    const fetchesMore = groupCount > groupRowCount;

    // The header row sits on slate-850. The count is 12 px text there, so it takes slate-400 (6.3:1).
    // slate-500 holds 3.4:1, under the 4.5:1 floor. The chevron and the hover actions are icons, which
    // the 3:1 non-text floor admits at slate-500.
    // The count carries thousands separators, as the app's other counts do, so a five-figure group
    // does not read as one more session number beside the "#104" rows.
    html += `<div class="session-group group" data-sgroup-key="${safeKey}">
      <div class="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-slate-700/30 rounded-lg select-none"
           data-sgroup-toggle-key="${safeKey}"
           onclick="toggleSessionGroup(this.dataset.sgroupToggleKey)">
        ${expandChevron(!isCollapsed, SESSION_GROUP_CHEVRON_CLASS)}
        <span class="text-xs font-semibold text-slate-400 uppercase tracking-wider">${escapeHtml(label)}</span>
        ${groupActions}
        <span class="text-xs text-slate-400 ml-auto">${groupCount.toLocaleString('en-US')}</span>
      </div>
      <div class="session-group-items ${isCollapsed ? 'hidden' : ''}" data-sgroup-items="${safeKey}">
        ${groupItemsHtml}
        ${listing && listing.partial && fetchesMore && !limitTogglePlaced ? renderGroupLimitToggle(key, isLimitExpanded) : ''}
      </div>
    </div>`;
  }
  nav.innerHTML = (options.workspaceBadges || '') + html;
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
    if (chevron) chevron.classList.toggle('rotate-90', wasCollapsed);
  }
}

// A group-header create (session-view.js) marks its target expanded before the
// create's repaint, so the new row appears visible inside the group it names.
function expandSessionGroup(key) {
  const collapsedState = loadGroupCollapsedState(SESSION_GROUP_COLLAPSED_STORAGE_KEY);
  collapsedState[key] = false;
  localStorage.setItem(SESSION_GROUP_COLLAPSED_STORAGE_KEY, JSON.stringify(collapsedState));
}

// Model label for a session row, from the one label function. A worker row carries the pool id in `pool` when its
// backend is a pool, since its `backend` then holds the member its run uses. A pool row shows its whole label, on its
// own line under the date (renderSessionTimeLine). A single-option row shows the option without the backend-family
// prefix, because beside the date the 320px sidebar leaves only ~67px. Returns null when the session carries no
// backend id.
function sessionBackendLabel(session) {
  const backendId = (session && session.backend) || '';
  if (!backendId) return null;
  const sessionBackend = session.pool || backendId;
  const pool = isPoolBackend(sessionBackend);
  const full = sessionModelLabel(sessionBackend, session.pool_member, session.native_model, 'full');
  const display = pool ? full : sessionModelLabel(sessionBackend, null, null, 'compact');
  return { full, display, pool };
}

function renderSessionTimeLine(s, timeIso, timeHtml, staticTime = false) {
  const label = sessionBackendLabel(s);
  // A static row formats its time once at build. Without data-time it stays
  // outside updateRelativeTimes's [data-time] sweep, so a long archived list
  // never joins the periodic full-table refresh.
  const timeAttr = staticTime ? '' : ` data-time="${timeIso}"`;
  // The line sits on slate-850, where slate-500 holds about 3.4:1.
  // slate-400 is still quieter than the name and holds about 6.3:1.
  if (label && label.pool) {
    // A pool row's label takes its own line under the date, the layout of the scheduled-task lines (cron.js). Beside
    // the date the 320px sidebar leaves a label about 67px wide, so the pool and member cannot both show. Both lines
    // are block boxes, so the row is one line taller than a single-option row. The label wraps rather than cutting:
    // the whole label is the fact the row adds, and styles.css breaks it inside the row.
    return `<span class="block text-xs text-slate-400">`
      + `<span class="session-time block"${timeAttr}>${timeHtml}</span>`
      + `<span class="session-backend block" title="${escapeHtmlAttr(label.full)}">`
      + `${escapeHtml(label.display)}</span>`
      + `</span>`;
  }
  const model = label
    ? `<span class="session-backend truncate" title="${escapeHtmlAttr(label.full)}">${escapeHtml(label.display)}</span>`
    : '';
  return `<span class="flex items-center gap-1.5 text-xs text-slate-400">`
    + `<span class="session-time flex-shrink-0"${timeAttr}>${timeHtml}</span>`
    + model
    + `</span>`;
}

// The row's updated_at as an ISO string. A column row carries epoch minutes and a listing route's row an ISO string;
// both reach the row renderer. sessionUpdatedMs (utils.js) reads the same two shapes for ordering.
function sessionUpdatedIso(session) {
  const value = session && session.updated_at;
  if (typeof value === 'number') return new Date(value * 60000).toISOString();
  return value || '';
}

// ---------------------------------------------------------------------------
// Task-tree nesting: a child task node renders under its parent row.
// ---------------------------------------------------------------------------
// The expanded set is held in memory only, so a reload starts every parent
// collapsed. A persisted set would mask a preview-cap regression.
const treeExpandedNodes = new Set();
// The session whose ancestors the last paint opened (one reveal per switch).
let lastRevealedSessionId = null;


// Roots are the rows without a parent in this list (a null parent, or a parent
// that is archived or filtered out). Every other row nests under its parent.
// Roots keep the list order. Children put logical sessions before worker
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
  const newestFirst = (a, b) => sessionUpdatedMs(b) - sessionUpdatedMs(a);
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
  const title = `${childCount} child task${childCount === 1 ? '' : 's'}`;
  return treeChevron(sessionId, treeExpandedNodes.has(sessionId), title);
}

// One tree row's expand chevron. applyTreeNodeExpansion turns this element in
// place, so the rotation animates.
function treeChevron(sessionId, expanded, title) {
  return expandChevron(expanded,
      'text-slate-500 transition-transform cursor-pointer flex-shrink-0 tree-chevron',
      `data-tree-toggle="${sessionId}" role="button" aria-expanded="${expanded ? 'true' : 'false'}" title="${title}"`
      + ' onclick="event.preventDefault(); event.stopPropagation(); toggleTreeNode(this.dataset.treeToggle)"');
}

// The lead marker on a tree row with no child tasks: the chevron's 12px slot,
// inert. No button, no pointer cursor and no click handler, so a tap here
// reaches the row's own link like any other part of the row.
function renderTreeLeadSlot() {
  return '<span class="w-3 h-3 flex-shrink-0" aria-hidden="true"></span>';
}

// The count includes direct children whose profile is not 'worker'. Worker
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
  return cbIcon('worker', 12, 'text-slate-500 flex-shrink-0', 'title="Worker (implementation leaf)"');
}

// An archived worker delivered its work: the row leads with a check instead
// of the leaf glyph.
function renderWorkerDeliveredIcon() {
  return cbIcon('done', 12, 'text-green-500 flex-shrink-0', 'title="Worker (delivered)"');
}

// One row followed by its subtree. The outer wrapper carries the row's
// group-limit class and attributes, so a root hidden by the preview limit
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
      ${children.map(c => renderSessionTree(c, filter, {groupHeader: options.groupHeader}, childrenOf)).join('')}
    </div>
  </div>`;
}

// Expansion is display only: child rows show or hide and the chevron changes glyph.
// The subtree waiting mark is the one icon that reads the expansion, so the
// toggle repaints the row's indicators. The other icons read facts only.
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
  Sidebar.refreshSessionIndicator(sessionId);
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
  const timeIso = sessionUpdatedIso(s);
  const timeHtml = timeIso ? timeLabelHtml(relativeTime(timeIso)) : '';
  let actions = '';
  if (isContextRow) {
    actions = renderStarButton(s);
  } else if (isArchivedRow) {
    // The archived row: three direct buttons. The group move and the delete
    // are in the archived Settings menu (openArchivedRowMenu in
    // session-menus.js).
    actions = `
      ${renderStarButton(s)}
      ${renderRowActionButton(
          `event.preventDefault(); event.stopPropagation(); sidebarControl('unarchiveSession', '${s.id}')`,
          'hover:text-green-400',
          'Unarchive',
          'unarchive')}
      ${renderSettingsButton(s, 'archived')}`;
  } else if (isWorkerRow) {
    actions = renderArchiveButton(s);
  } else {
    // A normal row (manager or legacy): four direct buttons. Rename, group
    // move, Task & context and the registered items are in the Settings
    // menu (openSessionRowMenu in session-menus.js).
    actions = `
      ${renderStarButton(s)}
      ${renderNewChildButton(s)}
      ${renderArchiveButton(s)}
      ${renderSettingsButton(s)}`;
  }
  const indicators = [
      ...(isArchivedRow ? [] : [
        renderSessionIndicators(s),
        renderPendingTriggerIndicator(s),
        renderPendingPlanApprovalIndicator(s),
        ...sessionRowBadges.map(render => render(s)),
      ]),
      ...(s.status === 'archived' ? [] : [renderWaitingMarks(s)]),
  ].join('\n    ');
  // Every tree row leads with one 12px marker so a level's markers line up
  // in one column. A row with child tasks draws the chevron that expands
  // them. A row without any draws an inert slot of the same size. Its
  // leading edge still opens the session instead of hitting an expander
  // with nothing to show. A worker row draws the worker glyph
  // instead, never a chevron. A flat row (the search results) draws neither.
  // The Archived tab paints through this tree, so its rows lead with the same
  // marker. The indicators follow the lead marker.
  // The count beside the chevron uses direct children whose profile is not
  // 'worker'. Worker leaves still nest, and the chevron title still counts
  // every child task. Only the all and threads paints draw the count.
  const showChildCount = 'treeChildCount' in options
      && options.treeNonWorkerChildCount > 0
      && (filter === 'all' || filter === 'threads');
  const treeRow = 'treeChildCount' in options;
  const lead = [
      isWorker
          ? (isArchivedRow ? renderWorkerDeliveredIcon() : renderWorkerLeafIcon())
          : (treeRow
              ? (options.treeChildCount > 0 ? renderTreeChevron(s.id, options.treeChildCount) : renderTreeLeadSlot())
              : ''),
      showChildCount ? renderTreeChildCount(s.id, options.treeNonWorkerChildCount) : '',
      indicators,
  ].join('\n    ');
  // The first registered row line that answers replaces the time line.
  const line = sessionRowLines.reduce((found, render) => found ?? render(s), null)
      ?? renderSessionTimeLine(s, timeIso, timeHtml, !!options.staticTime);
  // A context row dims and carries the active tag on its name line. The tag
  // stays beside the name span, not inside it, so a long name truncates before
  // the tag does, and the time line keeps the column's full width.
  return renderSessionRowShell(s, {
    filter,
    title: sessionRowTitle(s, filter, options.groupHeader),
    activeClass: activeClass + (isContextRow ? ' opacity-60' : ''),
    options,
    indicators: lead,
    titleBadges: isArchivedRow ? '' : sessionRowTitleBadges.map((render) => render(s)).join(''),
    line,
    nameTag: isContextRow
      ? '<span class="ml-auto px-1.5 py-0.5 rounded bg-slate-700 text-[10px] leading-none font-medium text-slate-400 flex-shrink-0">active</span>'
      : '',
    actions,
  });
}

function renderSessionList(sessions, filter, options = {}) {
  globalThis.sidebarSearchCounters.render += 1;
  if ((filter === 'search') !== searchBoxHasText()) globalThis.sidebarSearchCounters.stale_paints += 1;
  lastListPaint = {sessions, filter, options};
  searchListPainted = (filter === 'search');
  // The grouped paints (Workspace and Later) nest rows and record their tree
  // shape themselves. A flat paint shows each row's own facts.
  recordTreeShape(new Map(), new Map(), isTreeNodeExpanded);
  const nav = document.getElementById('session-list');
  if (!sessions.length) {
    if (filter !== 'search') {
      lastGroupedRenderArgs = {
        sessions,
        filter,
        workspaceBadges: options.workspaceBadges || '',
        listing: options.listing || null,
      };
    }
    const labels = {
      all: 'No sessions yet',
      threads: 'No chat threads',
      starred: 'No sessions saved for later',
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
  // A full search answer is the newest matches only, and the notice saying so stands above the first row.
  // A reader who scans the rows first has already concluded that the match they want is not there.
  // Below them, the notice followed every row of the list.
  const capNotice = sessions.length >= SEARCH_MATCH_LIMIT
    ? renderEmptyNote('Showing the newest ' + SEARCH_MATCH_LIMIT + ' matches — narrow the search.')
    : '';
  nav.innerHTML = capNotice + sessions.map(s => renderSessionItem(s, filter)).join('');
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
  renderSessionList(
      lastGroupedRenderArgs.sessions.filter(s => !doomed.has(s.id)),
      lastGroupedRenderArgs.filter,
      {workspaceBadges: lastGroupedRenderArgs.workspaceBadges, listing: lastGroupedRenderArgs.listing});
  return true;
}

// The Workspace's row refresh: each refreshed row replaces its older copy or joins the list, and each gone id leaves
// it. The list repaints in last-update order. The sort is stable, so rows that did not move keep the order of the last
// full list. Returns false when no grouped list is painted.
function mergeRowsIntoRenderedList(rows, goneIds) {
  if (!lastGroupedRenderArgs) return false;
  const merged = new Map(lastGroupedRenderArgs.sessions.map(s => [s.id, s]));
  rows.forEach(row => merged.set(row.id, row));
  goneIds.forEach(id => merged.delete(id));
  const sessions = Array.from(merged.values()).sort((a, b) => sessionUpdatedMs(b) - sessionUpdatedMs(a));
  renderSessionList(sessions, lastGroupedRenderArgs.filter,
      {workspaceBadges: lastGroupedRenderArgs.workspaceBadges, listing: lastGroupedRenderArgs.listing});
  return true;
}



const GLOBALS = {
  registerGroupPreviewLimitExemption,
  registerSessionRowBadge,
  registerSessionRowTitleBadge,
  registerSessionRowLine,
  registerSessionMenuItems,
  registerGroupMenuItem,
  renderEmptyNote,
  renderSearchNote,
  paintWorkspaceBadges,
  repaintRenderedRows,
  resetGroupLimitState,
  toggleSessionGroupLimit,
  toggleSessionGroup,
  renderSessionItem,
  renderSessionList,
  mergeRowsIntoRenderedList,
  restoreWorkspaceFromMemory,
  rowsChangeMembership,
  toggleTreeNode,
};
const SIDEBAR_ONLY = {
  sessionMenuItemSources,
  sessionBackendLabel,
  groupMenuItems,
  filterPillClass,
  starButtonOnclick,
  removeSessionFromRenderedList,
  buildSessionTree,
  renderSessionTree,
  isTreeNodeExpanded,
  expandTreeNode,
  expandSessionGroup,
  renderGroupedSessionList,
  expandSessionColumns,
};
watchSessionListHeight();
Sidebar.wire(GLOBALS, SIDEBAR_ONLY);

})();
