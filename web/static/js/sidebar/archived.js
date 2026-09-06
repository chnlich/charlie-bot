(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Archived view: a flat keyset-paginated list with a group filter strip.
// Pages append as sibling containers, so rows already on screen are never
// rewritten; ordering, membership, and group aggregates come from
// GET /api/sessions/archived (src/api/sessions.py), and in-list operations
// (unarchive / delete / set group) update rows and strip counts in place
// with no refetch.
// ---------------------------------------------------------------------------

const ARCHIVED_PAGE_LIMIT = 100;
// Auto-append stops at the cap; the Load more button turns deeper scrolling
// into an explicit action instead of an unbounded DOM.
const ARCHIVED_RENDER_CAP = 2000;
const ARCHIVED_SCROLL_THRESHOLD_PX = 300;

const archivedState = {
  epoch: 0,        // bumped on every reset; an in-flight fetch from an older epoch discards its response
  group: null,     // active strip filter: null = all, '' = ungrouped, name = that group
  groups: [],      // [{group: name|null, total}] over the whole archived set
  nextBefore: null,
  nextBeforeId: null,
  hasMore: false,
  renderedCount: 0,
  rowGroups: {},   // session id -> group ('' = ungrouped) for every rendered row
  loading: false,
  pillsEl: null,
  rowsEl: null,
  footEl: null,
};

function archivedTotalCount() {
  return archivedState.groups.reduce((sum, g) => sum + g.total, 0);
}

function renderArchivedPills() {
  if (!archivedState.pillsEl) return;
  const pill = (onclickExpr, dataAttr, label, total, isActive) => {
    const cls = Sidebar.filterPillClass(isActive);
    return `<button type="button"${dataAttr} onclick="${onclickExpr}" class="${cls}">${escapeHtml(label)} <span class="text-slate-500">${total}</span></button>`;
  };
  const parts = [pill('setArchivedGroupFilter(null)', '', 'All', archivedTotalCount(), archivedState.group === null)];
  for (const g of archivedState.groups) {
    const value = g.group === null ? '' : g.group;
    parts.push(pill(
        'setArchivedGroupFilter(this.dataset.agroup)',
        ` data-agroup="${escapeHtmlAttr(value)}"`,
        g.group === null ? '(No group)' : g.group,
        g.total,
        archivedState.group === value,
    ));
  }
  archivedState.pillsEl.innerHTML = parts.join('');
}

function renderArchivedFoot() {
  const el = archivedState.footEl;
  if (!el) return;
  if (!archivedState.hasMore) {
    el.innerHTML = '';
    return;
  }
  if (archivedState.renderedCount >= ARCHIVED_RENDER_CAP) {
    el.innerHTML = `<button type="button" onclick="loadArchivedNextPage()"
        class="w-full text-left px-3 py-1.5 text-xs text-blue-400 hover:text-blue-300 hover:bg-slate-700/30 rounded-lg transition-colors">Load more</button>`;
  } else {
    el.innerHTML = '';
  }
}

function appendArchivedRows(sessions) {
  if (!archivedState.rowsEl) return;
  if (!sessions.length) {
    if (!archivedState.renderedCount) {
      archivedState.rowsEl.innerHTML = renderEmptyNote('No archived sessions');
    }
    return;
  }
  const pageEl = document.createElement('div');
  pageEl.className = 'space-y-1';
  pageEl.innerHTML = sessions.map(s => {
    archivedState.rowGroups[s.id] = s.group || '';
    return renderSessionItem(s, 'archived', {staticTime: true});
  }).join('');
  archivedState.rowsEl.appendChild(pageEl);
  archivedState.renderedCount += sessions.length;
  sessions.forEach(s => { sessionUnread[s.id] = !!s.has_unread; });
}

async function fetchArchivedPage() {
  if (archivedState.loading || !archivedState.rowsEl) return;
  archivedState.loading = true;
  const epoch = archivedState.epoch;
  try {
    let qs = 'limit=' + ARCHIVED_PAGE_LIMIT;
    if (archivedState.group !== null) qs += '&group=' + encodeURIComponent(archivedState.group);
    if (archivedState.nextBefore) {
      qs += '&before=' + encodeURIComponent(archivedState.nextBefore)
          + '&before_id=' + encodeURIComponent(archivedState.nextBeforeId);
    }
    const res = await fetch('/api/sessions/archived?' + qs);
    if (!res.ok) throw new Error(`Archived fetch failed: ${res.status}`);
    const page = await res.json();
    if (epoch !== archivedState.epoch) return;
    archivedState.groups = page.groups;
    archivedState.hasMore = page.has_more;
    archivedState.nextBefore = page.next_before;
    archivedState.nextBeforeId = page.next_before_id;
    appendArchivedRows(page.sessions);
    renderArchivedPills();
    renderArchivedFoot();
  } catch (err) {
    console.error('Archived fetch failed:', err);
  } finally {
    if (epoch === archivedState.epoch) archivedState.loading = false;
  }
}

function ensureArchivedScrollHandler(nav) {
  if (nav._archivedScrollHooked) return;
  nav._archivedScrollHooked = true;
  nav.addEventListener('scroll', () => {
    if (currentFilter !== 'archived') return;
    if (!archivedState.hasMore || archivedState.loading) return;
    if (archivedState.renderedCount >= ARCHIVED_RENDER_CAP) return;
    if (nav.scrollHeight - nav.scrollTop - nav.clientHeight > ARCHIVED_SCROLL_THRESHOLD_PX) return;
    fetchArchivedPage();
  });
}

function resetArchivedList() {
  archivedState.nextBefore = null;
  archivedState.nextBeforeId = null;
  archivedState.hasMore = false;
  archivedState.renderedCount = 0;
  archivedState.rowGroups = {};
  archivedState.loading = false;
  const nav = document.getElementById('session-list');
  if (!nav) return;
  nav.innerHTML = '';
  archivedState.pillsEl = document.createElement('div');
  archivedState.pillsEl.className = 'flex flex-wrap gap-1 px-2 py-1';
  archivedState.rowsEl = document.createElement('div');
  archivedState.rowsEl.className = 'space-y-1';
  archivedState.footEl = document.createElement('div');
  nav.appendChild(archivedState.pillsEl);
  nav.appendChild(archivedState.rowsEl);
  nav.appendChild(archivedState.footEl);
  ensureArchivedScrollHandler(nav);
  renderArchivedPills();
  fetchArchivedPage();
}

// Entry point from the sidebar tab: resets the strip filter and loads page 1.
function loadArchivedView() {
  archivedState.epoch += 1;
  archivedState.group = null;
  resetArchivedList();
}

// Strip pill click: null = all, '' = ungrouped, name = that group.
function setArchivedGroupFilter(group) {
  archivedState.epoch += 1;
  archivedState.group = group === undefined ? null : group;
  resetArchivedList();
}

function loadArchivedNextPage() {
  fetchArchivedPage();
}

function adjustArchivedGroupCount(group, delta) {
  const entry = archivedState.groups.find(g => g.group === group);
  if (entry) {
    entry.total += delta;
    if (entry.total <= 0) archivedState.groups = archivedState.groups.filter(g => g.total > 0);
    return;
  }
  if (delta > 0) {
    archivedState.groups.push({group, total: delta});
    archivedState.groups.sort((a, b) => {
      if (a.group === null) return 1;
      if (b.group === null) return -1;
      return a.group.localeCompare(b.group);
    });
  }
}

// A session left the archived set (unarchive / permanent delete): drop its
// bookkeeping and decrement the strip counts. Row removal itself is the
// caller's removeSessionRowInline.
function archivedForgetSession(sessionId) {
  const group = archivedState.rowGroups[sessionId];
  if (group === undefined) return;
  delete archivedState.rowGroups[sessionId];
  archivedState.renderedCount = Math.max(0, archivedState.renderedCount - 1);
  adjustArchivedGroupCount(group || null, -1);
  renderArchivedPills();
}

// Set group on an archived row: move the strip counts, update the row's group
// button, and remove the row only when it no longer matches the active strip
// filter (under "All" it stays in place). No refetch, no rewrite of other rows.
function applyArchivedGroupChange(sessionId, group) {
  const oldGroup = archivedState.rowGroups[sessionId];
  if (oldGroup === undefined) return;
  const next = group || '';
  if (next === oldGroup) return;
  adjustArchivedGroupCount(oldGroup || null, -1);
  adjustArchivedGroupCount(next || null, +1);
  archivedState.rowGroups[sessionId] = next;
  const row = document.getElementById('session-' + sessionId);
  if (row) {
    const btn = typeof row.querySelector === 'function' ? row.querySelector('[data-current-group]') : null;
    if (btn && btn.dataset) btn.dataset.currentGroup = next;
    if (archivedState.group !== null && archivedState.group !== next) {
      row.remove();
      delete archivedState.rowGroups[sessionId];
      archivedState.renderedCount = Math.max(0, archivedState.renderedCount - 1);
    }
  }
  renderArchivedPills();
}

// One name list: Object.assign puts each export on Sidebar, and the same keys
// become bare globals. Adding a function here is enough for both.
const API = {
  loadArchivedView,
  setArchivedGroupFilter,
  loadArchivedNextPage,
  archivedForgetSession,
  applyArchivedGroupChange,
};
Object.assign(Sidebar, API);
Sidebar.expose(Object.keys(API));

})();
