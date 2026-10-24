// ---------------------------------------------------------------------------
// The sidebar's session and group actions (sidebar/session-actions.js): a
// session's archive, unarchive and permanent delete, the group move, the
// group rename and delete, and the pending-trigger bell's open. The row
// buttons and the bell call them through sidebarControl, and the Settings
// menus (session-menus.js) call them as globals.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// The one modal chrome for the sidebar's JS-built overlays: the group modal
// and the delete-confirm modal below. Each dialog keeps its own extras (the
// delete confirm adds text-center) and inner markup.
const MODAL_OVERLAY_CLASS = 'fixed inset-0 z-[9999] bg-black/60 flex items-center justify-center';
const MODAL_DIALOG_CLASS = 'bg-slate-800 rounded-xl shadow-xl border border-slate-700 p-5 w-72';

// The one trash-can outline: real deletion only -- the delete-confirm modal
// below; the archived row's Delete permanently and the group header's Delete
// group are menu items, not icons.
const TRASH_SVG_PATH = `<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"/>`;

// ---------------------------------------------------------------------------
// Archive, unarchive and permanent delete
// ---------------------------------------------------------------------------
// Inline removal shared by archive / unarchive / delete: the list never
// refetches — grouped views repaint in place from the last-rendered list
// minus the session (preview backfill, counts and Show-all toggle resync),
// the archived tab and the search overlay fall back to removing just the row
// node, and when it was the session being viewed the view moves to the first
// remaining rendered session.
async function removeSessionRowInline(sessionId) {
  delete sessionUnread[sessionId];
  const repainted = Sidebar.removeSessionFromRenderedList(sessionId);
  if (!repainted) {
    const row = document.getElementById('session-' + sessionId);
    if (row) row.remove();
  }

  if (SESSION_ID !== sessionId) {
    updateSidebarHighlight(SESSION_ID);
    return;
  }
  let nextId = null;
  document.querySelectorAll('a[id^="session-"]').forEach(el => {
    const sid = el.id.slice('session-'.length);
    if (!nextId && sid && sid !== sessionId) nextId = sid;
  });
  if (nextId) {
    await switchSession(nextId);
  } else {
    renderNoActiveSessionView();
  }
}

async function archiveSession(id) {
  try {
    const res = await fetch(`/api/sessions/${id}/archive`, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({request_id: crypto.randomUUID()}),
    });
    if (!res.ok) throw new Error(await archiveRefusalDetail(res));
    await removeSessionRowInline(id);
  } catch (err) {
    console.error('Archive failed:', err);
    showToast(err.message, true);
  }
}

// The server's refusal sentence for the row's user: a 409 archive refusal
// carries its blockers (the nodes holding an unfinished run) beside the
// message; any other refusal body keeps whatever detail it carries.
async function archiveRefusalDetail(res) {
  let detail = `Archive failed: ${res.status}`;
  try {
    const body = await res.json();
    const d = body && body.detail;
    if (typeof d === 'string') {
      detail = d;
    } else if (d && typeof d === 'object') {
      detail = d.message || detail;
      if (Array.isArray(d.blockers) && d.blockers.length) detail += '\n' + d.blockers.join('\n');
    }
  } catch (err) {
    // A bodyless refusal keeps the status line.
  }
  return detail;
}

async function deleteSessionPermanently(sessionId) {
  const res = await fetch(`/api/sessions/${sessionId}/permanent`, { method: 'DELETE' });
  if (!res.ok) throw new Error(`Permanent delete failed: ${res.status}`);
  if (currentFilter === 'archived') archivedForgetSession(sessionId);
  await removeSessionRowInline(sessionId);
}

async function unarchiveSession(id) {
  try {
    const res = await fetch(`/api/sessions/${id}/unarchive`, { method: 'POST' });
    if (!res.ok) throw new Error(await archiveRefusalDetail(res));
    // A task node's restore answers with the restored ids: the target plus
    // its archived ancestor chain, so the archived list drops them all. A
    // legacy session's restore answers with the session metadata; its row is
    // the only one that leaves.
    let restored = [id];
    const body = await res.json().catch(() => null);  // an unreadable body keeps the single-row drop
    if (body && Array.isArray(body.restored)) restored = body.restored;
    if (currentFilter === 'archived') {
      restored.forEach(rid => archivedForgetSession(rid));
      await removeSessionRowInline(id);
      return;
    }
    switchSidebarFilter(currentFilter);
  } catch (err) {
    console.error('Unarchive failed:', err);
    showToast(err.message, true);
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
  overlay.className = MODAL_OVERLAY_CLASS;
  overlay.innerHTML = `
    <div class="${MODAL_DIALOG_CLASS} text-center"
         onclick="event.stopPropagation()">
      <svg class="w-8 h-8 text-red-400 mx-auto mb-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        ${TRASH_SVG_PATH}
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
// Group move, rename and delete
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// The pending-trigger bell
// ---------------------------------------------------------------------------
// The bell's click: open the session like a row click does, and expand its
// tray once it lands. On the already-open session the tray expands in place.
function openPendingTriggers(sid) {
  if (!expandPendingTriggersTray(sid)) switchSession(sid);
}

// The session rows' archive and unarchive buttons (groups.js) and the
// pending-trigger bell (status.js).
registerSidebarControl('archiveSession', archiveSession);
registerSidebarControl('unarchiveSession', unarchiveSession);
registerSidebarControl('openPendingTriggers', openPendingTriggers);

const GLOBALS = {
  archiveSession,
  unarchiveSession,
  confirmDeletePermanently,
  showGroupSelector,
  renameGroup,
  deleteGroup,
  openPendingTriggers,
};
const SIDEBAR_ONLY = {
  removeSessionRowInline,
  deleteSessionPermanently,
  setSessionGroup,
};
Sidebar.wire(GLOBALS, SIDEBAR_ONLY);

})();
