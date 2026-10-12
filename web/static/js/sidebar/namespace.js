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

  // Server-reported unread state per session, so the unread dot can be
  // restored after the spinner hides.
  exposeState('sessionUnread', {});
  exposeState('switching', false);
  exposeState('currentFilter', 'all');
  exposeState('statusPollMs', 3000);
  exposeState('thinkingStart', null);

  // The sidebar's rendered controls (the onclick strings of the rows, the
  // group headers and the pending-trigger bell) call sidebarControl(name,
  // ...args). The file that defines a control's handler registers it under
  // its name when its script loads, so a renderer names a handler of a file
  // loaded after it only as a string.
  const sidebarControls = new Map();

  function registerSidebarControl(name, handler) {
    if (typeof name !== 'string' || !name || typeof handler !== 'function') {
      throw new Error('Invalid sidebar control registration');
    }
    if (sidebarControls.has(name)) throw new Error('Duplicate sidebar control: ' + name);
    sidebarControls.set(name, handler);
  }

  function sidebarControl(name, ...args) {
    const handler = sidebarControls.get(name);
    if (!handler) throw new Error('Unregistered sidebar control: ' + name);
    return handler(...args);
  }

  Sidebar.wire = wire;
  wire({registerSidebarControl, sidebarControl});
  global.Sidebar = Sidebar;
})(globalThis);
