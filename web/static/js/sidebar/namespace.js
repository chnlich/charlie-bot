(function(global) {
  const Sidebar = global.Sidebar || {};
  const state = Sidebar.state || {};
  Sidebar.state = state;
  // The search's request, discard and list-paint counts, which the search tests read as window.sidebarSearchCounters.
  global.sidebarSearchCounters = {fetch: 0, render: 0, discard: 0};

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
  // globals is enough for both. The sidebarOnly list stays reachable through Sidebar.
  // A module whose every member is onclick-reachable passes one list only.
  function wire(globals, sidebarOnly) {
    Object.assign(Sidebar, globals, sidebarOnly || {});
    expose(Object.keys(globals));
  }

  exposeState('switching', false);
  exposeState('currentFilter', 'all');
  exposeState('statusPollMs', 3000);
  exposeState('thinkingStart', null);

  // The sidebar's rendered controls (the onclick strings of the rows, the
  // group headers and the pending-trigger bell) call sidebarControl(name,
  // ...args). The file that defines a control's handler registers it under
  // its name when its script loads. A renderer therefore names a handler of a
  // file loaded after it only as a string.
  const sidebarControls = new Map();

  function registerSidebarControl(name, handler) {
    if (typeof name !== 'string' || !name || typeof handler !== 'function') {
      throw new Error('Invalid sidebar control registration');
    }
    if (sidebarControls.has(name)) throw new Error('Duplicate sidebar control: ' + name);
    sidebarControls.set(name, handler);
  }

  // A control whose script is deferred: the name and the load that fetches the group. A call before
  // the group arrives loads it, then dispatches to the handler the script registers.
  const deferredControls = new Map();

  // The browser clears an event's currentTarget when its dispatch ends, and a deferred handler
  // reads it after the download. Put back the value the call was made with.
  function restoreCurrentTarget(args, currentTarget) {
    const event = args[0];
    if (currentTarget && event && typeof event.preventDefault === 'function') {
      Object.defineProperty(event, 'currentTarget', {value: currentTarget, configurable: true});
    }
  }

  function sidebarControl(name, ...args) {
    const handler = sidebarControls.get(name);
    if (handler) return handler(...args);
    const load = deferredControls.get(name);
    if (!load) throw new Error('Unregistered sidebar control: ' + name);
    const currentTarget = args[0] ? args[0].currentTarget : null;
    return load().then(
      () => {
        restoreCurrentTarget(args, currentTarget);
        return sidebarControl(name, ...args);
      },
      (error) => console.error('the sidebar interaction scripts did not load: ' + error.message),
    );
  }

  // Whether the sidebar paints a list the server can refresh row by row and scope by filter: the Workspace, the Later
  // view and every registered view. The archive view owns its pagination and a search answers its own query, so both
  // name their rows by id instead. The row refresh and the status poll both read it, so it lives in the first sidebar
  // file they load after.
  function sidebarRowsRefreshable() {
    const searchInput = global.document.getElementById('sidebar-search');
    return global.currentFilter !== 'archived' && !(searchInput && searchInput.value.trim());
  }

  // The sidebar's interaction scripts: the Settings menus, the rename and session-action dialogs,
  // the archive and group actions, and the move picker. A page paints its rows without them, and only a control
  // click reaches them. Each include of one is replaced by a loader under web/templates that calls
  // deferSidebarInteractions. The first control call or forwarded global loads the whole group. Each
  // module's own registerSidebarControl and Sidebar.wire then replace the forwards.
  const SESSION_ACTIONS_SCRIPT = '/static/js/sidebar/session-actions.js';
  const SESSION_MOVE_SCRIPT = '/static/js/sidebar/session-move.js';
  const INTERACTION_SCRIPTS = [
    '/static/js/sidebar/row-menu.js',
    '/static/js/sidebar/modals.js',
    SESSION_ACTIONS_SCRIPT,
    '/static/js/sidebar/session-menus.js',
    SESSION_MOVE_SCRIPT,
  ];
  const INTERACTION_CONTROLS = [
    'startRename', 'archiveSession', 'unarchiveSession', 'openPendingTriggers',
    'openSessionRowMenu', 'openGroupHeaderMenu',
  ];
  const INTERACTION_GLOBALS = [
    'openRowMenu', 'closeRowMenu', 'startRename', 'handleRenameKey', 'commitRename',
    'openSessionActionModal', 'closeSessionActionModal', 'submitSessionActionModal',
    'forkSession', 'eloneSession', 'openTaskContextModal', 'closeTaskContextModal',
    'archiveSession', 'unarchiveSession', 'confirmDeletePermanently', 'showGroupSelector',
    'renameGroup', 'deleteGroup', 'openPendingTriggers', 'openSessionMovePicker',
  ];
  const INTERACTION_SIDEBAR_ONLY = [
    'removeSessionRowInline', 'deleteSessionPermanently', 'setSessionGroup',
    'openSessionRowMenu', 'openGroupHeaderMenu',
  ];

  let interactionLoad = null;
  let interactionsDeferred = false;

  // One memo entry per file: a loaded file never reloads, and a failed file retries alone.
  function loadInteractionScript(path) {
    return loadPageScripts('sidebar-interactions:' + path, [path]);
  }

  // The stylesheet and four of the scripts start together, and the scripts run in completion order. The interaction
  // files read no binding that another one registers at load, with one exception: the move picker reads
  // Sidebar.modalChrome at click time, and session-actions.js registers it when it runs. So the picker's file starts
  // once session-actions.js has run.
  function loadSidebarInteractions() {
    if (interactionLoad === null) {
      let loading;
      loading = Promise.all([
        loadPageStyles('sidebar-interactions', ['/static/css/sidebar-interactions.css'])
          .catch((error) => {
            console.error(error.message);
            if (interactionLoad === loading) interactionLoad = null;
          }),
        ...INTERACTION_SCRIPTS.filter((path) => path !== SESSION_MOVE_SCRIPT).map(loadInteractionScript),
        loadInteractionScript(SESSION_ACTIONS_SCRIPT).then(() => loadInteractionScript(SESSION_MOVE_SCRIPT)),
      ]);
      interactionLoad = loading;
      // A failed download clears its memo so the next click retries that file alone. The caller logs script failures.
      loading.catch(() => { if (interactionLoad === loading) interactionLoad = null; });
    }
    return interactionLoad;
  }

  function forwardInteraction(name) {
    return (...args) => {
      const currentTarget = args[0] ? args[0].currentTarget : null;
      return loadSidebarInteractions().then(
        () => {
          restoreCurrentTarget(args, currentTarget);
          return Sidebar[name](...args);
        },
        (error) => console.error('the sidebar interaction scripts did not load: ' + error.message),
      );
    };
  }

  function deferSidebarInteractions() {
    if (interactionsDeferred) return;
    interactionsDeferred = true;
    for (const name of INTERACTION_CONTROLS) deferredControls.set(name, loadSidebarInteractions);
    const globals = {};
    for (const name of INTERACTION_GLOBALS) globals[name] = forwardInteraction(name);
    const sidebarOnly = {};
    for (const name of INTERACTION_SIDEBAR_ONLY) sidebarOnly[name] = forwardInteraction(name);
    Sidebar.wire(globals, sidebarOnly);
  }

  Sidebar.wire = wire;
  wire({registerSidebarControl, sidebarControl, sidebarRowsRefreshable, deferSidebarInteractions});
  global.Sidebar = Sidebar;
})(globalThis);
