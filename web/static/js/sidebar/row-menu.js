// ---------------------------------------------------------------------------
// The sidebar's one shared settings popover (sidebar/row-menu.js): openRowMenu
// renders a single div.row-menu appended to document.body, fixed-positioned
// under the anchor's right edge (flipped above the anchor near the viewport
// bottom, clamped inside the viewport horizontally), and closeRowMenu removes
// it. Every anchor is groups.js's: the normal row's openSessionRowMenu, the
// archived row's variant of it (the button's data-row-menu marker picks the
// archived item list) and the group header's openGroupHeaderMenu.
// ---------------------------------------------------------------------------
(function() {
  const Sidebar = globalThis.Sidebar;

  const EDGE_MARGIN = 8;
  const ANCHOR_GAP = 4;

  let menuEl = null;
  let anchorEl = null;
  let sessionListEl = null;
  let onDocumentPointerDown = null;
  let onDocumentKeyDown = null;

  function closeRowMenu() {
    if (!menuEl) return;
    document.removeEventListener('pointerdown', onDocumentPointerDown, true);
    document.removeEventListener('keydown', onDocumentKeyDown, true);
    sessionListEl.removeEventListener('scroll', closeRowMenu);
    window.removeEventListener('resize', closeRowMenu);
    menuEl.remove();
    menuEl = null;
    anchorEl = null;
    sessionListEl = null;
  }

  // Fixed position under the anchor's right edge, flipped above the anchor
  // when the open-below placement would pass the viewport bottom, clamped
  // inside the viewport horizontally.
  function placeMenu(menu, anchor) {
    const anchorRect = anchor.getBoundingClientRect();
    const menuRect = menu.getBoundingClientRect();
    let top = anchorRect.bottom + ANCHOR_GAP;
    if (top + menuRect.height > window.innerHeight) {
      top = anchorRect.top - menuRect.height - ANCHOR_GAP;
    }
    let left = anchorRect.right - menuRect.width;
    if (left < EDGE_MARGIN) left = EDGE_MARGIN;
    const maxLeft = window.innerWidth - menuRect.width - EDGE_MARGIN;
    if (left > maxLeft) left = maxLeft;
    menu.style.top = Math.round(top) + 'px';
    menu.style.left = Math.round(left) + 'px';
  }

  function appendSeparator(menu) {
    const sep = document.createElement('div');
    sep.className = 'row-menu-sep';
    sep.setAttribute('role', 'separator');
    menu.appendChild(sep);
  }

  function appendItem(menu, item) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = item.danger ? 'row-menu-item row-menu-item-danger' : 'row-menu-item';
    button.setAttribute('role', 'menuitem');
    button.textContent = item.label;
    // Close first, then act: onSelect sees the menu already gone. The click
    // event rides along -- rename anchors its input off the event.
    button.addEventListener('click', function(event) {
      closeRowMenu();
      item.onSelect(event);
    });
    menu.appendChild(button);
  }

  function openRowMenu(anchor, items) {
    // Toggle: opening from the anchor of the open menu just closes it.
    const closesSelf = menuEl !== null && anchorEl === anchor;
    closeRowMenu();
    if (closesSelf) return;

    const menu = document.createElement('div');
    menu.className = 'row-menu';
    menu.setAttribute('role', 'menu');
    items.forEach(function(item) {
      if (item.separator) appendSeparator(menu);
      else appendItem(menu, item);
    });
    document.body.appendChild(menu);
    menuEl = menu;
    anchorEl = anchor;
    placeMenu(menu, anchor);

    // A pointerdown on the anchor stays open: the anchor's own onclick runs
    // openRowMenu again, which toggles the menu closed.
    onDocumentPointerDown = function(event) {
      const target = event && event.target;
      if (menuEl.contains(target) || anchorEl.contains(target)) return;
      closeRowMenu();
    };
    onDocumentKeyDown = function(event) {
      if (event.key === 'Escape') closeRowMenu();
    };
    document.addEventListener('pointerdown', onDocumentPointerDown, true);
    document.addEventListener('keydown', onDocumentKeyDown, true);
    sessionListEl = document.getElementById('session-list');
    sessionListEl.addEventListener('scroll', closeRowMenu);
    window.addEventListener('resize', closeRowMenu);
  }

  Sidebar.wire({openRowMenu, closeRowMenu});
})();
