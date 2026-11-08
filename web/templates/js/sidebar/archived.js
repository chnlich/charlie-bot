// ---------------------------------------------------------------------------
// Archived view: the loader the document carries in place of the view.
// ---------------------------------------------------------------------------
// The Archive filter's view owns its keyset-paginated fetch, and a reader meets
// it only after choosing that filter; the Workspace, Later and Threads views take
// the one-shot listing instead. The view's module stays at
// web/static/js/sidebar/archived.js, which the static route serves and the page
// tests read; this file is what the document carries, because the loader's search
// path reads web/templates before web/static and the page includes the same name.
//
// Each wired entry forwards to the module and starts its download on the first
// call, which is the filter's selection. The module's own Sidebar.wire then
// replaces these forwards, so a later call runs the module directly. A call that
// arrives while the download runs waits for it, and the calls keep their order.
(function() {
  const Sidebar = globalThis.Sidebar;
  const SOURCE = '/static/js/sidebar/archived.js';

  function load() {
    return loadPageScripts('sidebar-archived-view', [SOURCE]);
  }

  function forward(name) {
    return function() {
      const args = arguments;
      return load().then(() => Sidebar[name].apply(Sidebar, args));
    };
  }

  Sidebar.wire({
    loadArchivedView: forward('loadArchivedView'),
    refreshArchivedView: forward('refreshArchivedView'),
    setArchivedGroupFilter: forward('setArchivedGroupFilter'),
    loadArchivedNextPage: forward('loadArchivedNextPage'),
    archivedStarToggled: forward('archivedStarToggled'),
    archivedForgetSession: forward('archivedForgetSession'),
    applyArchivedGroupChange: forward('applyArchivedGroupChange'),
  });
})();
