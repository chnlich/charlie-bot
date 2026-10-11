// ---------------------------------------------------------------------------
// The core registration entries a package page part uses: registerTab
// (tabs.js) and registerWsMessageHandler (websocket.js). Each package registers
// its tab and its event types through them, so deleting a package leaves no
// name of it in the core files.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, readPackageStatic } = require('./read_static');
const { loadWebsocketContext } = require('./websocket_context_stub');

function fakeElement(id) {
  const classes = new Set();
  return {
    id,
    style: {},
    classList: {
      add: (...names) => names.forEach((name) => classes.add(name)),
      remove: (...names) => names.forEach((name) => classes.delete(name)),
      contains: (name) => classes.has(name),
      toggle: (name, force) => (force ? classes.add(name) : classes.delete(name)),
    },
  };
}

function loadTabsContext({isMobile = false} = {}) {
  const elements = new Map(
      ['tab-chat', 'tab-plans', 'btn-chat', 'btn-chat-plans', 'btn-pkg'].map((id) => [id, fakeElement(id)]));
  const calls = [];
  const context = {
    document: {getElementById: (id) => elements.get(id) || null},
    platform: {isMobile},
    planPanel: {onTabShown: () => calls.push('plans.onTabShown')},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readStatic('tabs.js'), context, {filename: 'tabs.js'});
  return {context, elements, calls};
}

function packageTab(calls, overrides = {}) {
  return {
    id: 'pkg',
    showsChat: true,
    fullScreenOnMobile: true,
    keepOnSessionSwitch: true,
    onShow: () => calls.push('pkg.onShow'),
    onHide: () => calls.push('pkg.onHide'),
    ...overrides,
  };
}

test('registerTab refuses an incomplete entry and a second entry for one id', () => {
  const {context, calls} = loadTabsContext();
  const {onHide, ...withoutOnHide} = packageTab(calls);

  assert.throws(() => context.registerTab(withoutOnHide), /Invalid tab registration/);
  assert.throws(() => context.registerTab(packageTab(calls, {id: 'chat'})), /Duplicate tab: chat/);
  assert.throws(() => context.switchTab('pkg'), /Unknown tab: pkg/);
});

test('switchTab shows the chosen tab, hides every other, and marks its button', () => {
  const {context, elements, calls} = loadTabsContext();
  context.registerTab(packageTab(calls));

  context.switchTab('pkg');
  const afterShow = [...calls];
  context.switchTab('chat');

  assert.deepEqual(afterShow, ['pkg.onShow']);
  assert.ok(calls.includes('pkg.onHide'));
  assert.equal(elements.get('tab-chat').classList.contains('hidden'), false);
  assert.ok(elements.get('tab-plans').classList.contains('hidden'));
  assert.ok(elements.get('btn-chat').classList.contains('bg-blue-600/20'));
  assert.ok(elements.get('btn-pkg').classList.contains('text-slate-400'));
});

test('a full-screen tab hides the chat column on a phone and keeps it on a desktop', () => {
  const desktop = loadTabsContext({isMobile: false});
  const phone = loadTabsContext({isMobile: true});
  desktop.context.registerTab(packageTab(desktop.calls));
  phone.context.registerTab(packageTab(phone.calls));

  desktop.context.switchTab('pkg');
  phone.context.switchTab('pkg');

  assert.equal(desktop.elements.get('tab-chat').classList.contains('hidden'), false);
  assert.ok(phone.elements.get('tab-chat').classList.contains('hidden'));
});

test('a session switch keeps a tab that keeps, and returns to chat from plans', () => {
  const {context, calls} = loadTabsContext();
  context.registerTab(packageTab(calls));

  context.switchTab('pkg');
  const fromPackageTab = context.tabAfterSessionSwitch();
  context.switchTab('chat-plans');
  const fromPlans = context.tabAfterSessionSwitch();

  assert.equal(fromPackageTab, 'pkg');
  assert.equal(fromPlans, 'chat');
  assert.ok(calls.includes('plans.onTabShown'));
});

test('a registered message handler runs for its event type after the event cursor advances', () => {
  const {context} = loadWebsocketContext({SESSION_ID: 'session-a'});
  const seen = [];
  context.registerWsMessageHandler('pkg_event', (ev) => seen.push([ev.payload, context.eventCursor]));

  context.handleWSEvent({type: 'pkg_event', payload: 'p', event_index: 7}, 'session-a', 0);

  assert.deepEqual(seen, [['p', 8]]);
  assert.throws(() => context.registerWsMessageHandler('pkg_event', () => {}), /Duplicate WebSocket message handler/);
  assert.throws(() => context.registerWsMessageHandler('', () => {}), /Invalid WebSocket message handler/);
});

// A package's scripts loaded in page order, with the core entries recording what they register.
function loadPackageScripts(name, files, extra = {}) {
  const tabs = [];
  const handlers = new Map();
  const listeners = new Map();
  const context = {
    registerTab: (tab) => tabs.push(tab),
    registerWsMessageHandler: (type, handler) => handlers.set(type, handler),
    document: {
      addEventListener: (type, listener) => listeners.set(type, listener),
      getElementById: () => null,
    },
    ...extra,
  };
  context.globalThis = context;
  vm.createContext(context);
  for (const file of files) vm.runInContext(readPackageStatic('features', name, file), context, {filename: file});
  return {context, tabs, handlers, listeners};
}

function tabLayouts(tabs) {
  return tabs.map(({id, showsChat, fullScreenOnMobile, keepOnSessionSwitch}) => ({id, showsChat, fullScreenOnMobile, keepOnSessionSwitch}));
}

test('the latex package registers its tab and its proposed-edit event', () => {
  const {tabs, handlers} = loadPackageScripts('latex', ['latex-panel.js']);

  assert.deepEqual(tabLayouts(tabs), [{id: 'chat-tex', showsChat: true, fullScreenOnMobile: true, keepOnSessionSwitch: true}]);
  assert.deepEqual([...handlers.keys()], ['tex_edit_proposed']);
});

test('the backlog package registers its tab and reloads its list on the first show after a session switch', () => {
  const fetched = [];
  const {tabs, listeners} = loadPackageScripts('backlog', ['backlog-panel.js'], {
    fetch: (url) => {
      fetched.push(url);
      return new Promise(() => {});
    },
  });
  const [backlogTab] = tabs;

  backlogTab.onShow();
  backlogTab.onShow();
  listeners.get('charliebot:session-switched')();
  backlogTab.onShow();

  assert.deepEqual(tabLayouts(tabs), [{id: 'chat-backlog', showsChat: true, fullScreenOnMobile: true, keepOnSessionSwitch: true}]);
  assert.deepEqual(fetched, ['/api/backlog/repos', '/api/backlog/repos']);
});

test('the terminal package registers its tab', () => {
  const {tabs} = loadPackageScripts(
      'terminal', ['terminal_b64.js', 'terminal_clipboard.js', 'terminal_mount.js', 'terminal_panel.js']);

  assert.deepEqual(tabLayouts(tabs), [{id: 'terminal', showsChat: false, fullScreenOnMobile: false, keepOnSessionSwitch: true}]);
});

test('the usage package renders its strip from its event type', () => {
  const {context, handlers} = loadPackageScripts('usage', ['ext_usage.js']);

  assert.deepEqual([...handlers.keys()], ['ext_usage']);
  assert.equal(handlers.get('ext_usage'), context.renderExtUsage);
});
