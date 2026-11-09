// ---------------------------------------------------------------------------
// Session actions: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// A reader reaches archive, unarchive and the pending-trigger bell only by
// clicking a rendered control, so the sidebar loads the interaction scripts
// together on the first control call. This file replaces the include of
// js/sidebar/session-actions.js, because the template loader reads
// web/templates before web/static and the page includes the same name.
Sidebar.deferSidebarInteractions();
