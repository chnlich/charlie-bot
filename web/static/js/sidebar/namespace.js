(function(global) {
  const Sidebar = global.Sidebar || {};
  const state = Sidebar.state || {};
  Sidebar.state = state;

  function ensureState(name, value) {
    if (!(name in state)) state[name] = value;
  }

  function expose(names) {
    names.forEach((name) => { global[name] = Sidebar[name]; });
  }

  function exposeState(name, value) {
    ensureState(name, typeof global[name] === 'undefined' ? value : global[name]);
    Object.defineProperty(global, name, {
      configurable: true,
      get() { return state[name]; },
      set(value) { state[name] = value; },
    });
  }

  // Two disjoint lists: Object.assign puts both on Sidebar, and only globals'
  // keys become bare globals for the onclick strings. Adding a function to
  // globals is enough for both; sidebarOnly stays reachable through Sidebar.
  // A module whose every member is onclick-reachable passes one list only.
  function wire(globals, sidebarOnly) {
    Object.assign(Sidebar, globals, sidebarOnly || {});
    expose(Object.keys(globals));
  }

  exposeState('sessionUnread', {});
  exposeState('switching', false);
  exposeState('currentFilter', 'all');
  exposeState('statusPollMs', 3000);
  exposeState('workersLoadedForSession', null);
  exposeState('workersLoadInflightForSession', null);
  exposeState('workersListEtag', null);
  exposeState('thinkingStart', null);

  Sidebar.expose = expose;
  Sidebar.wire = wire;
  global.Sidebar = Sidebar;
})(globalThis);
