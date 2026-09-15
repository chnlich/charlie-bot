(function() {
  const Sidebar = globalThis.Sidebar;

// ---------------------------------------------------------------------------
// Session tree — the primary task navigation (v2 task nodes)
// ---------------------------------------------------------------------------
// Owner of tree rendering, expansion, selection and this view's request/cache
// lifecycle. Every row comes from GET /api/sessions/tree (one revision-bound,
// server-ordered page chain per level); the client never derives parentage or
// state itself. Live updates ride the task_tree_changed sidebar event and
// re-read only the affected levels.

const EXPANDED_STORAGE_KEY = 'charliebot-tree-expanded';
const TREE_PAGE_LIMIT = 100;

const tree = {
  rows: new Map(),        // id -> row (latest server fact for the node)
  levels: new Map(),      // parentId ('' for roots) -> {ids, nextCursor, revision, fetched}
  expanded: new Set(),    // node ids whose children container is open
  includeArchived: false,
  loading: new Set(),     // parentIds with an in-flight page fetch
  staleNotice: new Map(), // parentId -> human explanation of a 409 refresh
  highlighted: null,      // search-highlighted node id
  gen: 0,                 // view generation: a filter change/reload supersedes in-flight pages
  levelEpoch: new Map(),  // parentId -> newest fetch epoch; a stale page may never overwrite fresh facts
};

function loadExpandedSet() {
  // A corrupt stored blob degrades to no saved expansion rather than breaking
  // the render path, so the catch stays.
  try {
    const raw = JSON.parse(localStorage.getItem(EXPANDED_STORAGE_KEY) || '[]');
    return new Set(Array.isArray(raw) ? raw.filter((v) => typeof v === 'string') : []);
  } catch (_err) {
    return new Set();
  }
}

function persistExpanded() {
  try {
    localStorage.setItem(EXPANDED_STORAGE_KEY, JSON.stringify([...tree.expanded]));
  } catch (err) {
    console.error('persistExpanded failed:', err);
  }
}

// -- data ------------------------------------------------------------------

async function fetchLevel(parentId) {
  // Paginate the level to exhaustion: a page boundary must never silently
  // hide children. Appends are deduped by id at render time. Overlapping
  // fetches for one level (a live-data refresh racing an earlier pagination
  // fetch) are ordered by epoch: only the newest may write rows/level state,
  // so a stale response can never overwrite fresher server facts.
  const key = parentId || '';
  const epoch = (tree.levelEpoch.get(key) || 0) + 1;
  tree.levelEpoch.set(key, epoch);
  const gen = tree.gen;
  let cursor = null;
  let revision = null;
  const ids = [];
  do {
    const params = new URLSearchParams({include_archived: String(tree.includeArchived), limit: String(TREE_PAGE_LIMIT)});
    if (parentId) params.set('parent_id', parentId);
    if (cursor) params.set('cursor', cursor);
    const res = await fetch('/api/sessions/tree?' + params.toString(), {cache: 'no-store'});
    if (gen !== tree.gen || tree.levelEpoch.get(key) !== epoch) return null; // superseded
    if (res.status === 409) {
      // The tree moved during pagination: refetch this level from fresh facts
      // and keep a visible explanation until the next data-driven refresh or
      // user toggle of the level.
      const detail = await res.json().catch(() => ({}));
      tree.staleNotice.set(key, detail.detail?.message || 'Task tree changed while loading');
      tree.levels.delete(key);
      return fetchLevel(parentId);
    }
    if (!res.ok) throw new Error('tree fetch failed: ' + res.status);
    const page = await res.json();
    if (gen !== tree.gen || tree.levelEpoch.get(key) !== epoch) return null; // superseded
    revision = page.tree_revision;
    for (const row of page.items) {
      tree.rows.set(row.id, row);
      if (!ids.includes(row.id)) ids.push(row.id);
    }
    cursor = page.next_cursor;
  } while (cursor);
  tree.levels.set(key, {ids, nextCursor: null, revision, fetched: true});
  return ids;
}

async function ensureLevel(parentId, opts = {}) {
  const key = parentId || '';
  if (!opts.force) {
    if (tree.levels.get(key)?.fetched) return tree.levels.get(key).ids;
    if (tree.loading.has(key)) return null;
  }
  // A forced refetch (live data-change refresh) bypasses the in-flight guard:
  // the change landed after the running fetch started, so waiting for it would
  // drop the fresher page and leave the level empty until the next event.
  tree.loading.add(key);
  try {
    return await fetchLevel(parentId);
  } catch (err) {
    console.error('ensureLevel failed:', err);
    return null;
  } finally {
    tree.loading.delete(key);
  }
}

function invalidateLevel(parentId) {
  tree.levels.delete(parentId || '');
}

function rowOf(id) {
  return tree.rows.get(id) || null;
}

// -- rendering --------------------------------------------------------------

const WORK_STATE_DOT = {
  running: 'bg-blue-500',
  attention: 'bg-red-500',
  waiting: 'bg-amber-400',
  idle: 'bg-slate-600',
};

function rowMainClass(row) {
  if (row.id === SESSION_ID) return 'bg-blue-600/20 text-blue-200';
  if (row.archived) return 'text-slate-500 hover:bg-slate-700/40';
  return 'text-slate-200 hover:bg-slate-700/50';
}

function profileLabel(profile) {
  return profile === 'manager' ? 'Manager' : 'Worker';
}

function taskStateLabel(state) {
  return state.charAt(0).toUpperCase() + state.slice(1);
}

function badgeEl(className, text, title) {
  const span = document.createElement('span');
  span.className = className;
  if (title) span.title = title;
  span.textContent = text;
  return span;
}

function buildRowBadges(container, row) {
  const dot = WORK_STATE_DOT[row.work_state] || 'bg-slate-600';
  container.appendChild(badgeEl(
    'text-[10px] uppercase tracking-wide text-slate-400 border border-slate-600 rounded px-1 py-px whitespace-nowrap',
    profileLabel(row.profile)));
  const work = badgeEl('flex items-center gap-1 text-[11px] text-slate-400 whitespace-nowrap', row.work_state, 'Work state');
  const dotSpan = document.createElement('span');
  dotSpan.className = 'w-1.5 h-1.5 rounded-full flex-shrink-0 ' + dot + (row.work_state === 'running' ? ' animate-pulse' : '');
  work.prepend(dotSpan);
  container.appendChild(work);
  if (row.task_state !== 'open') {
    container.appendChild(badgeEl('text-[11px] text-slate-500 whitespace-nowrap', taskStateLabel(row.task_state)));
  }
  if (row.open_descendant_count > 0 || row.attention_descendant_count > 0) {
    const counts = badgeEl('text-[11px] text-slate-500 whitespace-nowrap',
      row.open_descendant_count + ' open' + (row.attention_descendant_count > 0 ? ' · ' + row.attention_descendant_count + ' attention' : ''),
      'Descendant tasks');
    container.appendChild(counts);
  }
  if (row.archived) {
    container.appendChild(badgeEl('text-[10px] text-slate-500 border border-slate-700 rounded px-1 py-px whitespace-nowrap', 'archived'));
  }
}

// One tree row. Built with DOM APIs and textContent for every name/state
// string — names and goals are user data and are never interpolated as HTML.
//
// Name space is allocated before badge space: the name is a flex item with a
// 55%-of-row width floor, and the role/status/count badge group is one
// shrink-proof item that wraps to a second line as a whole when it no longer
// fits beside it. A root manager with attention and subtree counts used to
// squeeze the name to zero visible width on the 320px sidebar; now the name
// keeps over half the row at any depth, compact rows stay single-line, and
// nothing relies on hover.
function buildRowElement(row, depth) {
  const el = document.createElement('div');
  el.className = 'tree-row';
  el.setAttribute('role', 'treeitem');
  el.setAttribute('aria-expanded', tree.expanded.has(row.id) ? 'true' : 'false');
  el.dataset.nodeId = row.id;
  el.tabIndex = 0;
  el.id = 'tree-node-' + row.id;

  const inner = document.createElement('div');
  inner.className = 'flex flex-wrap items-center gap-x-1.5 gap-y-0.5 rounded-lg px-2 py-1.5 cursor-pointer transition-colors min-w-0 ' + rowMainClass(row);
  inner.style.paddingLeft = (8 + depth * 16) + 'px';
  if (row.id === tree.highlighted) inner.classList.add('ring-1', 'ring-blue-400');

  const hasChildren = row.child_count > 0;
  const chevron = document.createElement('button');
  chevron.type = 'button';
  chevron.className = 'w-4 h-4 flex-shrink-0 flex items-center justify-center text-slate-500 hover:text-slate-300 rounded' + (hasChildren ? '' : ' invisible');
  chevron.setAttribute('aria-label', (tree.expanded.has(row.id) ? 'Collapse ' : 'Expand ') + row.name);
  chevron.dataset.action = 'toggle';
  chevron.innerHTML = '<svg class="w-3 h-3 transition-transform' + (tree.expanded.has(row.id) ? ' rotate-90' : '') + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7"/></svg>';
  inner.appendChild(chevron);

  const name = document.createElement('span');
  name.className = 'flex-1 min-w-[55%] truncate text-sm session-name';
  name.textContent = row.name;
  name.title = row.name;
  name.dataset.action = 'open';
  inner.appendChild(name);

  const badges = document.createElement('span');
  badges.className = 'flex items-center gap-1.5 flex-shrink-0 tree-meta-row';
  buildRowBadges(badges, row);
  inner.appendChild(badges);

  const addBtn = document.createElement('button');
  addBtn.type = 'button';
  // Manager rows carry an always-visible (keyboard-reachable) add-subtask
  // button; worker rows are leaves and never show one.
  addBtn.className = 'w-5 h-5 flex-shrink-0 items-center justify-center text-slate-500 hover:text-blue-300 rounded tree-add-child '
    + (row.profile === 'manager' ? 'flex' : 'hidden');
  addBtn.setAttribute('aria-label', 'New subtask under ' + row.name);
  addBtn.title = 'New subtask';
  addBtn.dataset.action = 'add-child';
  addBtn.innerHTML = '<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4"/></svg>';
  inner.appendChild(addBtn);

  el.appendChild(inner);
  return el;
}

function childrenContainerId(nodeId) {
  return 'tree-children-' + nodeId;
}

function buildChildrenContainer(nodeId, depth) {
  const container = document.createElement('div');
  container.className = 'tree-children';
  container.id = childrenContainerId(nodeId);
  container.setAttribute('role', 'group');
  container.dataset.depth = String(depth);
  fillChildrenContainer(container, nodeId, depth);
  return container;
}

function fillChildrenContainer(container, nodeId, depth) {
  container.textContent = '';
  const notice = tree.staleNotice.get(nodeId);
  if (notice) {
    const n = document.createElement('div');
    n.className = 'text-xs text-amber-300 px-2 py-1';
    n.textContent = notice;
    container.appendChild(n);
  }
  const level = tree.levels.get(nodeId);
  const loading = !level && tree.loading.has(nodeId);
  if (loading) {
    const p = document.createElement('p');
    p.className = 'text-slate-500 text-xs px-3 py-1.5';
    p.textContent = 'Loading tasks...';
    container.appendChild(p);
    return;
  }
  if (!level) return;
  for (const childId of level.ids) {
    const childRow = rowOf(childId);
    if (!childRow) continue;
    container.appendChild(buildSubtree(childRow, depth));
  }
}

function buildSubtree(row, depth) {
  const wrap = document.createElement('div');
  wrap.appendChild(buildRowElement(row, depth));
  if (tree.expanded.has(row.id)) {
    wrap.appendChild(buildChildrenContainer(row.id, depth + 1));
  }
  return wrap;
}

function ensureTreeDelegation(nav) {
  if (nav.dataset.treeDelegated === '1') return;
  nav.dataset.treeDelegated = '1';
  nav.addEventListener('click', onTreeRowClick);
  nav.addEventListener('keydown', onTreeRowKeydown);
}

function renderTree() {
  const nav = document.getElementById('session-list');
  if (!nav) return;
  nav.textContent = '';
  nav.setAttribute('role', 'tree');
  nav.setAttribute('aria-label', 'Task tree');
  ensureTreeDelegation(nav);
  const rootsNotice = tree.staleNotice.get('');
  if (rootsNotice) {
    nav.appendChild(badgeEl('text-xs text-amber-300 px-2 py-1', rootsNotice));
  }

  const header = document.createElement('div');
  header.className = 'flex items-center gap-2 px-2 pb-1';
  const showArchived = document.createElement('label');
  showArchived.className = 'flex items-center gap-1.5 text-xs text-slate-400 cursor-pointer select-none';
  const box = document.createElement('input');
  box.type = 'checkbox';
  box.id = 'tree-show-archived';
  box.className = 'accent-blue-500';
  box.checked = tree.includeArchived;
  box.addEventListener('change', () => {
    tree.includeArchived = box.checked;
    tree.levels.clear();
    renderTree();
    ensureLevel(null).then(() => renderTree());
  });
  showArchived.appendChild(box);
  const label = document.createElement('span');
  label.textContent = 'Show archived';
  showArchived.appendChild(label);
  header.appendChild(showArchived);
  nav.appendChild(header);

  const roots = tree.levels.get('');
  if (!roots) {
    const p = document.createElement('p');
    p.className = 'text-slate-500 text-sm px-3 py-2';
    p.textContent = tree.loading.has('') ? 'Loading tasks...' : 'No task tree loaded.';
    nav.appendChild(p);
    return;
  }
  if (roots.ids.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'px-3 py-3 text-sm text-slate-500';
    empty.textContent = 'No task trees yet. Use "New task" to create a root task.';
    nav.appendChild(empty);
    return;
  }
  for (const rootId of roots.ids) {
    const row = rowOf(rootId);
    if (row) nav.appendChild(buildSubtree(row, 0));
  }
}

// -- interactions -----------------------------------------------------------

async function toggleTreeNode(nodeId) {
  tree.staleNotice.delete(nodeId);
  if (tree.expanded.has(nodeId)) {
    tree.expanded.delete(nodeId);
    persistExpanded();
    const container = document.getElementById(childrenContainerId(nodeId));
    const rowEl = document.getElementById('tree-node-' + nodeId);
    if (container) container.remove();
    if (rowEl) rowEl.setAttribute('aria-expanded', 'false');
    const chevron = rowEl?.querySelector('button[data-action="toggle"] svg');
    if (chevron) chevron.classList.remove('rotate-90');
    return;
  }
  tree.expanded.add(nodeId);
  persistExpanded();
  const rowEl = document.getElementById('tree-node-' + nodeId);
  if (rowEl) {
    rowEl.setAttribute('aria-expanded', 'true');
    const chevron = rowEl.querySelector('button[data-action="toggle"] svg');
    if (chevron) chevron.classList.add('rotate-90');
  }
  await ensureLevel(nodeId);
  if (rowEl && tree.expanded.has(nodeId)) {
    // Insert (or refresh) the children container right after the row.
    document.getElementById(childrenContainerId(nodeId))?.remove();
    const container = buildChildrenContainer(nodeId, subtreeDepthOf(nodeId) + 1);
    rowEl.after(container);
  }
}

function subtreeDepthOf(nodeId) {
  // Depth from the row's own padding: the row element carries it in style.
  const rowEl = document.getElementById('tree-node-' + nodeId);
  const inner = rowEl?.firstElementChild;
  if (!inner) return 0;
  const pad = parseInt(inner.style.paddingLeft || '8', 10) || 8;
  return Math.max(0, Math.round((pad - 8) / 16));
}

async function openTreeNode(nodeId) {
  await switchSession(nodeId);
}

function onTreeRowClick(event) {
  const actionEl = event.target.closest('[data-action]');
  const rowEl = event.target.closest('.tree-row');
  if (!rowEl) return;
  const nodeId = rowEl.dataset.nodeId;
  const action = actionEl?.dataset.action || 'open';
  if (action === 'toggle') {
    toggleTreeNode(nodeId);
  } else if (action === 'add-child') {
    if (globalThis.TaskPanel) globalThis.TaskPanel.openChildModal(nodeId);
  } else if (action === 'open') {
    if (actionEl && actionEl.closest('button')) { toggleTreeNode(nodeId); return; }
    openTreeNode(nodeId);
  }
}

function onTreeRowKeydown(event) {
  const rowEl = event.target.closest('.tree-row');
  if (!rowEl) return;
  const nodeId = rowEl.dataset.nodeId;
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    openTreeNode(nodeId);
  } else if (event.key === 'ArrowRight') {
    event.preventDefault();
    if (!tree.expanded.has(nodeId) && (rowOf(nodeId)?.child_count || 0) > 0) toggleTreeNode(nodeId);
  } else if (event.key === 'ArrowLeft') {
    event.preventDefault();
    if (tree.expanded.has(nodeId)) toggleTreeNode(nodeId);
  } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    const rows = [...document.querySelectorAll('#session-list .tree-row')];
    const idx = rows.indexOf(rowEl);
    const next = rows[idx + (event.key === 'ArrowDown' ? 1 : -1)];
    if (next) { event.preventDefault(); next.focus(); }
  }
}

// -- view lifecycle ---------------------------------------------------------

function enterTreeFilter() {
  tree.gen++;
  if (tree.expanded.size === 0 && !tree._restored) {
    tree.expanded = loadExpandedSet();
    tree._restored = true;
  }
  renderTree();
  ensureLevel(null).then((ids) => {
    if (ids === null) return;
    // Restore expansion: fetch each expanded node's children, then paint once.
    const jobs = [...tree.expanded].map((id) => ensureLevel(id));
    return Promise.all(jobs);
  }).then(() => {
    renderTree();
    if (SESSION_ID && rowOf(SESSION_ID)) highlightNode(SESSION_ID);
  });
}

function highlightNode(nodeId) {
  document.querySelectorAll('#session-list .tree-row').forEach((el) => {
    const inner = el.firstElementChild;
    if (!inner) return;
    inner.classList.remove('bg-blue-600/20', 'text-blue-200');
    if (el.dataset.nodeId === nodeId && !rowOf(nodeId)?.archived) {
      inner.classList.add('bg-blue-600/20', 'text-blue-200');
    }
  });
}

// Expand the full path to *nodeId* (server-provided ancestor chain), then
// highlight it. Used on deep links/reload and by search. The chain entries may
// be full rows (search hits) or bare refs ({id, name} from the session
// detail): bare refs are NEVER cached as rows — tree_page fetches are the only
// row source, so every rendered row carries the server's derived facts.
async function revealNode(nodeId, ancestorRefs) {
  const chainIds = (ancestorRefs || [])
    .map((a) => (typeof a === 'string' ? a : a.id))
    .reverse();
  chainIds.push(nodeId);
  // The path's base level first: the archived deep-link caller clears every
  // cached level before revealing, and the render reads the tree from the
  // roots level down — without this refetch the reveal leaves an empty tree.
  await ensureLevel(null);
  for (const id of chainIds) {
    if (id === nodeId) break;
    tree.expanded.add(id);
    await ensureLevel(id);
  }
  tree.expanded.delete(nodeId); // selecting a node does not force-open it
  persistExpanded();
  renderTree();
  const el = document.getElementById('tree-node-' + nodeId);
  if (el) {
    el.scrollIntoView({block: 'nearest'});
    highlightNode(nodeId);
  }
}

// Called from the session switch machinery: keep the active node visible.
function onSessionShown(session) {
  if (!session || !session.profile) return;
  if (currentFilter !== 'tasks') return;
  if (document.getElementById('tree-node-' + session.id)) {
    highlightNode(session.id);
    return;
  }
  // The node is not rendered (fresh deep link or collapsed path): one bounded
  // detail read learns its place AND its derived visibility — a completed
  // worker's autoarchive is a row fact on the detail, not session status —
  // then reveals the full path. A deep link to an archived node implies
  // archived visibility; the toggle reflects it so the state the user sees is
  // the state they can change. Cached levels predate the flip: drop them so
  // the reveal refetches with archived rows included.
  fetch('/api/sessions/' + session.id, {cache: 'no-store'})
    .then((res) => (res.ok ? res.json() : Promise.reject(new Error(String(res.status)))))
    .then(async (detail) => {
      if (detail.archived && !tree.includeArchived) {
        tree.includeArchived = true;
        const box = document.getElementById('tree-show-archived');
        if (box) box.checked = true;
        tree.levels.clear();
      }
      await revealNode(session.id, detail.ancestors || []);
    })
    .catch((err) => console.error('onSessionShown reveal failed:', err));
}

// Live updates: re-read the affected levels only. Idempotent — every refresh
// repaints from server facts, so duplicate notifications change nothing.
const pendingTreeEvents = new Set();
let treeEventTimer = null;

function onTreeChanged(sessionId) {
  pendingTreeEvents.add(sessionId);
  if (treeEventTimer) return;
  treeEventTimer = setTimeout(() => {
    const ids = [...pendingTreeEvents];
    pendingTreeEvents.clear();
    treeEventTimer = null;
    // The data refresh (affected levels + the open session's panel hooks)
    // always runs, so a report/close/ack/Run/creation fact is never lost while
    // another sidebar filter is showing and the cached levels stay fresh for
    // the return to the tree. Only the on-screen repaint is gated: another
    // filter owns #session-list right now.
    void refreshAffectedLevels(ids, {render: currentFilter === 'tasks'});
  }, 150);
}

async function refreshAffectedLevels(sessionIds, opts = {}) {
  // Refetch exactly the levels the changed nodes live on, plus each ancestor
  // level above them (the counts on ancestor rows are server facts).
  const levels = new Set(['']);
  for (const sid of sessionIds) {
    if (rowOf(sid)) {
      let cur = sid;
      const guard = new Set();
      while (cur && !guard.has(cur)) {
        guard.add(cur);
        const row = rowOf(cur);
        const key = row ? (row.task_parent_id || '') : null;
        if (key === null) break;
        levels.add(key);
        cur = key;
      }
    } else {
      // The node was never rendered: one bounded detail read learns its place
      // (task_parent_id + the ancestor chain), never a whole-tree rescan.
      try {
        const res = await fetch('/api/sessions/' + encodeURIComponent(sid), {cache: 'no-store'});
        if (res.ok) {
          const detail = await res.json();
          const chain = [detail, ...(detail.ancestors || [])]; // nearest-first
          for (let i = 0; i < chain.length; i++) {
            const parent = chain[i + 1];
            levels.add(parent ? parent.id : '');
          }
        } else {
          for (const id of tree.expanded) levels.add(id);
        }
      } catch (err) {
        console.error('refreshAffectedLevels detail read failed:', err);
        for (const id of tree.expanded) levels.add(id);
      }
    }
    if (rowOf(sid) && tree.expanded.has(sid)) levels.add(sid);
  }
  for (const level of levels) invalidateLevel(level);
  for (const level of levels) await ensureLevel(level === '' ? null : level, {force: true});
  // Fresh server facts landed: a prior pagination-conflict explanation is
  // obsolete.
  tree.staleNotice.clear();
  if (opts.render !== false) {
    renderTree();
    if (SESSION_ID) highlightNode(SESSION_ID);
  }
  if (globalThis.TaskPanel) globalThis.TaskPanel.onTreeChanged(sessionIds);
  if (globalThis.TaskRunsPanel) globalThis.TaskRunsPanel.onTreeChanged(sessionIds);
  if (globalThis.TaskContextPanel) globalThis.TaskContextPanel.onTreeChanged(sessionIds);
}

// Search inside the tasks filter: hits arrive with their server-built ancestor
// path; expand each path level and reveal the matches in the real tree.
async function searchTree(query) {
  tree.gen++;
  const gen = tree.gen;
  if (!query.trim()) {
    tree.highlighted = null;
    enterTreeFilter();
    return;
  }
  renderTreeWithMessage('Searching tasks...');
  try {
    const res = await fetch('/api/sessions/tree/search?q=' + encodeURIComponent(query.trim()), {cache: 'no-store'});
    if (!res.ok) throw new Error('tree search failed: ' + res.status);
    const body = await res.json();
    if (gen !== tree.gen) return;
    if (!body.items.length) {
      renderTreeWithMessage('No tasks match "' + query.trim() + '".');
      return;
    }
    await ensureLevel(null); // the path's base level, so the reveal renders
    // Reveal the first hit's path (the server caps hits; every hit carries its
    // own path, and the first is the newest match).
    const hit = body.items[0];
    tree.rows.set(hit.row.id, hit.row);
    tree.highlighted = hit.row.id;
    await revealNode(hit.row.id, hit.ancestors || []);
  } catch (err) {
    console.error('searchTree failed:', err);
    renderTreeWithMessage('Task search failed: ' + (err && err.message ? err.message : err));
  }
}

function renderTreeWithMessage(message) {
  const nav = document.getElementById('session-list');
  if (!nav) return;
  nav.textContent = '';
  const p = document.createElement('p');
  p.className = 'text-slate-400 text-sm px-3 py-2';
  p.textContent = message;
  nav.appendChild(p);
}

// Idempotent expansion (unlike toggleTreeNode): used by automation and by
// reveal paths that must not collapse an already-open level.
async function ensureExpanded(nodeId) {
  tree.expanded.add(nodeId);
  persistExpanded();
  await ensureLevel(nodeId);
  const rowEl = document.getElementById('tree-node-' + nodeId);
  if (rowEl && tree.expanded.has(nodeId)) {
    document.getElementById(childrenContainerId(nodeId))?.remove();
    rowEl.after(buildChildrenContainer(nodeId, subtreeDepthOf(nodeId) + 1));
  }
}

const API = {
  enterTreeFilter,
  ensureExpanded,
  onSessionShown,
  onTreeChanged,
  searchTree,
  revealNode,
  highlightNode,
  toggleTreeNode,
  invalidateAll: () => { tree.levels.clear(); },
  state: tree,
};
Sidebar.SessionTree = API;
Sidebar.wire({}, {
  onTreeRowClick,
  onTreeRowKeydown,
});

})();
