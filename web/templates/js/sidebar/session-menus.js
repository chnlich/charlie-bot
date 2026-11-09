// ---------------------------------------------------------------------------
// Settings menus: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A reader reaches the row and group menus only by clicking a Settings control,
// so the sidebar loads the interaction scripts together on the first control
// call. This file replaces the include of js/sidebar/session-menus.js, because
// the template loader reads web/templates before web/static and the page
// includes the same name.
Sidebar.deferSidebarInteractions();
