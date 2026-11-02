// ---------------------------------------------------------------------------
// The core registration entries a package page part uses: registerTab
// (tabs.js), registerWsMessageHandler (websocket.js),
// Chat.registerMessagePostProcessor (chat/rendering.js) and
// Chat.registerFileLinkFamily (chat/file-links.js). Each package registers its
// tab, its event types and its chat rendering through them, so deleting a
// package leaves no name of it in the core files.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, readPackageStatic } = require('./read_static');
const { loadChatRendering } = require('./chat_rendering_context_stub');
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
      ['tab-chat', 'btn-chat', 'btn-pkg'].map((id) => [id, fakeElement(id)]));
  const calls = [];
  const context = {
    document: {getElementById: (id) => elements.get(id) || null},
    platform: {isMobile},
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

test('a session switch keeps a tab that keeps, and returns to chat from one that does not', () => {
  const {context, calls} = loadTabsContext();
  context.registerTab(packageTab(calls));
  context.registerTab(packageTab(calls, {id: 'full-area', keepOnSessionSwitch: false}));

  context.switchTab('pkg');
  const fromKeepingTab = context.tabAfterSessionSwitch();
  context.switchTab('full-area');
  const fromFullAreaTab = context.tabAfterSessionSwitch();

  assert.equal(fromKeepingTab, 'pkg');
  assert.equal(fromFullAreaTab, 'chat');
});

test('a registered message handler runs for its event type after the event cursor advances', () => {
  const {context} = loadWebsocketContext({SESSION_ID: 'session-a'});
  const seen = [];
  context.registerWsMessageHandler(
    'pkg_event',
    (ev) => seen.push([ev.payload, context.eventCursor]),
    {advancesCursor: true},
  );

  context.handleWSEvent({type: 'pkg_event', payload: 'p', event_index: 7}, 'session-a', 0);

  assert.deepEqual(seen, [['p', 8]]);
  assert.throws(
    () => context.registerWsMessageHandler('pkg_event', () => {}, {advancesCursor: true}),
    /Duplicate WebSocket message handler/,
  );
  assert.throws(
    () => context.registerWsMessageHandler('', () => {}, {advancesCursor: true}),
    /Invalid WebSocket message handler/,
  );
});

test('registered connect handlers run after opening without sending a cursor frame', () => {
  const {context, FakeWebSocket} = loadWebsocketContext({SESSION_ID: 'session-a', eventCursor: 12});
  const sentBeforeHandler = [];
  const handler = () => sentBeforeHandler.push(FakeWebSocket.instances.at(-1).sent.length);
  context.registerWsConnectHandler(handler);

  context.connectWS();
  const socket = FakeWebSocket.instances.at(-1);
  assert.equal(socket.url, '/ws/sessions/session-a?cursor=12&asset_version=stub-asset-version');
  socket.emitOpen();

  assert.deepEqual(sentBeforeHandler, [0]);
  assert.deepEqual(socket.sent, []);
  assert.throws(() => context.registerWsConnectHandler(handler), /Duplicate WebSocket connect handler/);
  assert.throws(() => context.registerWsConnectHandler(null), /Invalid WebSocket connect handler/);
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
      readyState: 'loading',
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

test('the terminal package registers its tab', () => {
  const {tabs} = loadPackageScripts(
      'terminal', ['terminal_b64.js', 'terminal_clipboard.js', 'terminal_mount.js', 'terminal_panel.js']);

  assert.deepEqual(tabLayouts(tabs), [{id: 'terminal', showsChat: false, fullScreenOnMobile: false, keepOnSessionSwitch: true}]);
});

test('the artifacts package registers the Plans tab, its plan events and its session listener', () => {
  const connectHandlers = [];
  const {context, tabs, handlers, listeners} = loadPackageScripts('artifacts', ['plan-panel.js'], {
    registerWsConnectHandler: (handler) => connectHandlers.push(handler),
  });
  const panel = vm.runInContext('planPanel', context);
  const calls = [];
  for (const name of ['invalidate', 'onReconnect', 'onPlanUpdated', 'onActiveSessionChanged']) {
    panel[name] = (...args) => calls.push([name, ...args]);
  }

  connectHandlers.forEach((handler) => handler());
  handlers.get('plan_updated')({plan_id: 7});
  listeners.get('charliebot:active-session-changed')();

  assert.deepEqual(tabLayouts(tabs), [{id: 'chat-plans', showsChat: false, fullScreenOnMobile: false, keepOnSessionSwitch: false}]);
  assert.deepEqual(calls, [['invalidate'], ['onReconnect'], ['onPlanUpdated', 7], ['onActiveSessionChanged']]);
});

test('the usage package renders its strip from its event type', () => {
  const {context, handlers} = loadPackageScripts('usage', ['ext_usage.js']);

  assert.deepEqual([...handlers.keys()], ['ext_usage']);
  assert.equal(handlers.get('ext_usage'), context.renderExtUsage);
});

test('a registered message post-processor runs over each rendered root', () => {
  const context = loadChatRendering();
  const seen = [];
  const processor = (root) => seen.push(root.id);
  context.Chat.registerMessagePostProcessor(processor);

  context.postProcessRenderedMessages({id: 'root-1', querySelectorAll: () => []});

  assert.deepEqual(seen, ['root-1']);
  assert.throws(() => context.Chat.registerMessagePostProcessor(processor), /Duplicate message post-processor/);
  assert.throws(() => context.Chat.registerMessagePostProcessor(null), /Invalid message post-processor/);
});

test('the artifacts package registers its cards and its file link family', () => {
  const processors = [];
  const context = {
    console,
    URL: globalThis.URL,
    window: {addEventListener: () => {}},
    Chat: {registerMessagePostProcessor: (processor) => processors.push(processor)},
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, {filename: 'chat/namespace.js'});
  vm.runInContext(readStatic('chat/file-links.js'), context, {filename: 'chat/file-links.js'});
  vm.runInContext(readPackageStatic('features', 'artifacts', 'artifact-cards.js'), context, {filename: 'artifact-cards.js'});
  const family = {name: 'artifact-pages', textSource: 'x', answersFor: () => false, recognizes: () => false};

  assert.deepEqual(processors, [context.Chat.embedLinkedHtmlArtifacts]);
  assert.throws(() => context.Chat.registerFileLinkFamily(family), /Duplicate file link family: artifact-pages/);
  assert.throws(() => context.Chat.registerFileLinkFamily({...family, name: ''}), /Invalid file link family/);
});
