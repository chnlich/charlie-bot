// ---------------------------------------------------------------------------
// The sidebar's Settings menus (sidebar/session-menus.js): the normal row's,
// the archived row's and the named group header's. groups.js renders each
// Settings button with the facts its items need as data attributes, and the
// button's click opens the menu through sidebarControl. Each item calls the
// action of the file that defines it.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

// The normal row's Settings menu: openRowMenu (row-menu.js) draws, places and
// closes the popover; this builds its items from the Settings button's data
// attributes. Each item keeps its old direct button's condition: a task-tree
// child moves with its parent (no group move), and a legacy row has no task
// record behind Task & context. The registered items follow
// (registerSessionMenuItems).
function desktopSessionMenuItems(attrs, sessionId) {
  const items = [{label: 'Rename', onSelect: (event) => startRename(event, sessionId)}];
  if (!attrs.taskParent) {
    items.push({label: 'Move to group…',
      onSelect: () => showGroupSelector(sessionId, attrs.currentGroup || null)});
  }
  if (attrs.profile) {
    items.push({label: 'Task & context', onSelect: () => openTaskContextModal(sessionId)});
  }
  Sidebar.sessionMenuItemSources.forEach(source => items.push(...source.items(attrs, sessionId)));
  return items;
}

// The touch menu's star item reads the row's starred state off the star
// button at open time. filters.js's toggleSessionStar repaints that button
// in place (the text-yellow-400 flip), so the class never goes stale
// between renders.
function touchStarItem(sessionId) {
  const star = document.getElementById('star-' + sessionId);
  const starred = !!star && star.classList.contains('text-yellow-400');
  return {label: starred ? 'Remove from Later' : 'Add to Later',
    onSelect: () => toggleSessionStar(sessionId, starred)};
}

function openSessionRowMenu(anchor, sessionId) {
  const attrs = anchor.dataset;
  // The archived row reuses the Settings button's markup and data attributes;
  // its marker picks the archived item list instead of the normal one.
  if (attrs.rowMenu === 'archived') {
    openArchivedRowMenu(anchor, sessionId);
    return;
  }
  const items = [];
  if (window.matchMedia('(hover: none)').matches) {
    // Touch: the direct buttons are hidden behind the gear. So the menu
    // leads with the star toggle and the child creation, and trails with the
    // archive -- the three actions with no desktop direct button left.
    items.push(touchStarItem(sessionId));
    items.push({label: 'New child session', onSelect: () => createChildSession(sessionId)});
    items.push(...desktopSessionMenuItems(attrs, sessionId));
    items.push({separator: true});
    items.push({label: 'Archive', onSelect: () => archiveSession(sessionId)});
  } else {
    items.push(...desktopSessionMenuItems(attrs, sessionId));
  }
  openRowMenu(anchor, items);
}

// The archived row's Settings menu: each item keeps its old direct button's
// condition. A task-tree child moves with its parent (no group move), and
// the delete keeps its red danger styling. Touch leads with the star toggle
// and Unarchive (the direct buttons a touch row hides); a child's menu keeps
// the separator around the group move's absence.
function openArchivedRowMenu(anchor, sessionId) {
  const attrs = anchor.dataset;
  const touch = window.matchMedia('(hover: none)').matches;
  const items = [];
  if (touch) {
    items.push(touchStarItem(sessionId));
    items.push({label: 'Unarchive', onSelect: () => unarchiveSession(sessionId)});
  }
  if (!attrs.taskParent) {
    items.push({label: 'Move to group…',
      onSelect: () => showGroupSelector(sessionId, attrs.currentGroup || null)});
  }
  // The delete rides a separator wherever the menu has an item above it: on
  // touch always, and on desktop only when the group move is present. A
  // desktop child's menu is the delete alone.
  if (!attrs.taskParent || touch) {
    items.push({separator: true});
  }
  items.push({label: 'Delete permanently', danger: true,
    onSelect: () => confirmDeletePermanently(sessionId)});
  openRowMenu(anchor, items);
}

// The named group header's Settings menu: the gear reads the group name off
// its own data attribute at click time (no module-level state). Each item
// keeps its old direct button's call. The registered items lead the menu
// (registerGroupMenuItem).
function openGroupHeaderMenu(anchor) {
  const groupName = anchor.dataset.groupName;
  openRowMenu(anchor, [
    ...Sidebar.groupMenuItems.map(item => ({label: item.label, onSelect: () => item.onSelect(groupName)})),
    {label: 'Rename group', onSelect: () => renameGroup(groupName)},
    {separator: true},
    {label: 'Delete group', danger: true, onSelect: () => deleteGroup(groupName)},
  ]);
}

// The Settings buttons that groups.js renders open these menus.
registerSidebarControl('openSessionRowMenu', openSessionRowMenu);
registerSidebarControl('openGroupHeaderMenu', openGroupHeaderMenu);
Sidebar.wire({}, {openSessionRowMenu, openGroupHeaderMenu});

})();
