(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Archived view: a keyset-paginated project-grouped tree with a group filter
// strip — the same tree format the Workspace view builds. Pages merge into one
// row list (first delivery wins; the server re-delivers a context ancestor a
// previous page already carried); ordering, membership, and group aggregates
// come from GET /api/sessions/archived (src/runtime/api/sessions.py), and in-list
// operations (unarchive / delete / set group) update the merged list and the
// strip counts in place with no refetch. A task-tree broadcast refreshes the
// loaded range in place: the strip filter, the loaded pages and the scroll
// position stay. The rendered cap and Load more count archived rows only: a
// context row is the tree's scaffolding, never part of the user's archived list.
// ---------------------------------------------------------------------------

const ARCHIVED_PAGE_LIMIT = 100;
// The server clamps limit to 500 (SESSION_PAGE_LIMIT_MAX in src/infra/constants.py;
// clamp(1, 500) in rust/crates/runtime/session-api/src/routes.rs), so a refresh
// asks for that many rows per request.
const ARCHIVED_REFRESH_PAGE_LIMIT = 500;
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
  rows: [],        // every delivered row in first-delivery order: archived rows plus context ancestors
  rowIds: {},      // delivered row id -> true, the merge's dedup key
  rowGroups: {},   // archived row id -> group ('' = ungrouped) for the strip's in-place updates
  loading: false,
  refreshPending: false, // a broadcast arrived while loading: one trailing refresh runs when the in-flight work ends
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

// The archived view's one render form: every repaint of the merged list — a
// page append or an in-list removal — routes through this call, so a delivered
// firing always nests under its scheduled node whatever page delivered it.
function repaintArchivedTree() {
  if (!archivedState.rows.length) {
    archivedState.rowsEl.innerHTML = renderEmptyNote('No archived sessions');
    return;
  }
  Sidebar.renderGroupedSessionList(archivedState.rows, 'archived', {
    container: archivedState.rowsEl,
    staticTime: true,
    groupActions: false,
  });
}

// Merge one page into a list's rows: a row the list already holds keeps its
// first delivery, and the rendered count moves for archived rows only.
function mergeArchivedRows(list, sessions) {
  const fresh = sessions.filter(s => !list.rowIds[s.id]);
  fresh.forEach(s => {
    list.rowIds[s.id] = true;
    list.rows.push(s);
    if (!s.context_only) list.rowGroups[s.id] = s.group || '';
  });
  list.renderedCount += fresh.filter(s => !s.context_only).length;
}

// Merge one page into the tree's row list and repaint the whole tree, so a
// delivered firing always nests under its scheduled node whatever page each
// arrived on.
function appendArchivedRows(sessions) {
  if (!archivedState.rowsEl) return;
  mergeArchivedRows(archivedState, sessions);
  repaintArchivedTree();
}

// Drop rows from the merged list and repaint: an in-list operation's node-only
// row removal becomes a tree repaint, so the removed row's children promote
// instead of vanishing inside its subtree container.
function repaintWithoutRows(doomedIds) {
  const doomed = new Set(doomedIds);
  doomed.forEach(id => {
    delete archivedState.rowIds[id];
    delete archivedState.rowGroups[id];
  });
  archivedState.rows = archivedState.rows.filter(s => !doomed.has(s.id));
  repaintArchivedTree();
}

// One request on the archived list: the active strip filter, and the keyset
// cursor when the request follows another page.
async function requestArchivedPage(limit, before, beforeId) {
  let qs = 'limit=' + limit;
  if (archivedState.group !== null) qs += '&group=' + encodeURIComponent(archivedState.group);
  if (before) {
    qs += '&before=' + encodeURIComponent(before)
        + '&before_id=' + encodeURIComponent(beforeId);
  }
  const res = await fetch('/api/sessions/archived?' + qs);
  if (!res.ok) throw new Error(`Archived fetch failed: ${res.status}`);
  return res.json();
}

async function fetchArchivedPage() {
  if (archivedState.loading || !archivedState.rowsEl) return;
  archivedState.loading = true;
  const epoch = archivedState.epoch;
  try {
    const page = await requestArchivedPage(ARCHIVED_PAGE_LIMIT, archivedState.nextBefore, archivedState.nextBeforeId);
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
    if (epoch === archivedState.epoch) {
      archivedState.loading = false;
      runPendingArchivedRefresh();
    }
  }
}

// The one place a refresh rebuilds the list-derived state: the merged rows with
// their bookkeeping, and the strip totals and cursor the walk's last page carries.
function setArchivedList(list, lastPage) {
  archivedState.rows = list.rows;
  archivedState.rowIds = list.rowIds;
  archivedState.rowGroups = list.rowGroups;
  archivedState.renderedCount = list.renderedCount;
  archivedState.groups = lastPage.groups;
  archivedState.hasMore = lastPage.has_more;
  archivedState.nextBefore = lastPage.next_before;
  archivedState.nextBeforeId = lastPage.next_before_id;
}

// A broadcast refreshes the loaded range from the first page, in requests of the
// server's row cap, until the delivered archived rows cover the loaded count. It
// compares only after the last page; an unchanged list is not repainted.
async function refreshArchivedView() {
  if (archivedState.loading) {
    archivedState.refreshPending = true;
    return;
  }
  archivedState.loading = true;
  const epoch = archivedState.epoch;
  try {
    const loadedCount = archivedState.renderedCount;
    const list = {rows: [], rowIds: {}, rowGroups: {}, renderedCount: 0};
    let before = null;
    let beforeId = null;
    let page;
    do {
      page = await requestArchivedPage(ARCHIVED_REFRESH_PAGE_LIMIT, before, beforeId);
      if (epoch !== archivedState.epoch) return;
      mergeArchivedRows(list, page.sessions);
      before = page.next_before;
      beforeId = page.next_before_id;
    } while (page.has_more && list.renderedCount < loadedCount);
    // Server rows are JSON with a fixed key order, so their serialized form
    // compares every field the row render reads.
    if (JSON.stringify(list.rows) === JSON.stringify(archivedState.rows)) return;
    const nav = document.getElementById('session-list');
    const scrollTop = nav.scrollTop;
    setArchivedList(list, page);
    repaintArchivedTree();
    renderArchivedPills();
    renderArchivedFoot();
    nav.scrollTop = scrollTop;
  } catch (err) {
    console.error('Archived refresh failed:', err);
  } finally {
    if (epoch === archivedState.epoch) {
      archivedState.loading = false;
      runPendingArchivedRefresh();
    }
  }
}

// Broadcasts that arrived during the in-flight work share one trailing refresh.
function runPendingArchivedRefresh() {
  if (!archivedState.refreshPending) return;
  archivedState.refreshPending = false;
  refreshArchivedView();
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
  archivedState.rows = [];
  archivedState.rowIds = {};
  archivedState.rowGroups = {};
  archivedState.loading = false;
  // The page-1 request below starts after any earlier broadcast, so that
  // broadcast needs no trailing refresh.
  archivedState.refreshPending = false;
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

// A star toggle's optimistic flip, mirrored into the merged list: the star
// renders from the stored row, so the next tree repaint (a page append, an
// unarchive) paints the same state instead of reverting the button.
function archivedStarToggled(sessionId, starred) {
  const stored = archivedState.rows.find(s => s.id === sessionId);
  if (stored) stored.starred = starred;
}

// A session left the archived set (unarchive / permanent delete): drop its
// bookkeeping and decrement the strip counts, then repaint the tree without
// its row.
function archivedForgetSession(sessionId) {
  const group = archivedState.rowGroups[sessionId];
  if (group === undefined) return;
  archivedState.renderedCount = Math.max(0, archivedState.renderedCount - 1);
  adjustArchivedGroupCount(group || null, -1);
  repaintWithoutRows([sessionId]);
  renderArchivedPills();
}

// Set group on an archived row: move the strip counts, and drop the row from
// the tree only when it no longer matches the active strip filter (under
// "All" it stays). No refetch; the repaint re-derives the nesting, so the
// dropped row's children promote instead of vanishing with it.
function applyArchivedGroupChange(sessionId, group) {
  const oldGroup = archivedState.rowGroups[sessionId];
  if (oldGroup === undefined) return;
  const next = group || '';
  if (next === oldGroup) return;
  adjustArchivedGroupCount(oldGroup || null, -1);
  adjustArchivedGroupCount(next || null, +1);
  archivedState.rowGroups[sessionId] = next;
  // The stored row moves with the counts: the next tree repaint (a page
  // append, an unarchive) re-derives the groups from it, and the row must
  // land in its new group rather than revert to the old one.
  const stored = archivedState.rows.find(s => s.id === sessionId);
  if (stored) stored.group = next || null;
  const row = document.getElementById('session-' + sessionId);
  if (row) {
    const btn = typeof row.querySelector === 'function' ? row.querySelector('[data-current-group]') : null;
    if (btn && btn.dataset) btn.dataset.currentGroup = next;
  }
  if (archivedState.group !== null && archivedState.group !== next) {
    archivedState.renderedCount = Math.max(0, archivedState.renderedCount - 1);
    repaintWithoutRows([sessionId]);
  }
  renderArchivedPills();
}

const API = {
  loadArchivedView,
  refreshArchivedView,
  setArchivedGroupFilter,
  loadArchivedNextPage,
  archivedStarToggled,
  archivedForgetSession,
  applyArchivedGroupChange,
};
Sidebar.wire(API);

})();
