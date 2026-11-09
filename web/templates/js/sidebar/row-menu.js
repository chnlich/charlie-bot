// ---------------------------------------------------------------------------
// Settings popover: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A reader reaches the popover only by clicking a row's Settings button, so the
// sidebar loads the interaction scripts together on the first control call.
// This file replaces the include of js/sidebar/row-menu.js, because the template
// loader reads web/templates before web/static and the page includes the same
// name.
Sidebar.deferSidebarInteractions();
