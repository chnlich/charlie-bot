// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/websocket.js, shared by the node
// --test websocket harnesses: one base context (the page-load state globals,
// silent console, a fresh FakeWebSocket class per build, the no-key
// wsUrlWithToken page-load order, and a no-op for every hook websocket.js
// calls into the chat/sidebar modules), the harness's own globals spread
// over that base (SESSION_ID, document, setTimeout, the recorders), then
// one load sequence — createContext, websocket.js, websocket-handlers.js — so
// the load order both suites depend on lives in one place. A harness with a
// context of its own (the sidebar indicator harness loading the real
// status.js) takes the same load sequence through loadWebsocketModules.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { createFakeWebSocketClass } = require('./fake_websocket');
const { readStatic } = require('./read_static');

const WEBSOCKET_JS = readStatic('websocket.js');
const WEBSOCKET_HANDLERS_JS = readStatic('websocket-handlers.js');

function loadWebsocketContext(extraContext) {
  const context = {
    eventCursor: 0,
    reconnectDelay: 1000,
    thinkingStart: null,
    sessionUnread: {},
    // The page global index.html renders next to the ?v= script URLs; websocket.js
    // sends it as the handshake's asset_version.
    STATIC_ASSET_VERSION: 'stub-asset-version',
    document: {getElementById: () => null},
    Chat: {activeTurnEngineFor: () => null},
    localStorage: {getItem: () => null},
    // No stored key: mirrors page-load order config.js → websocket.js on the no-key path.
    wsUrlWithToken: (path) => path,
    location: {protocol: 'http:', host: 'localhost:8000'},
    console: {log: () => {}, error: () => {}},
    WebSocket: createFakeWebSocketClass(),
    clearTimeout: () => {},
    hideStreaming: () => {},
    showStreaming: () => {},
    appendMessage: () => {},
    appendMessageObject: () => {},
    observeWebStatusMessages: () => {},
    noteMessageForStatusView: () => {},
    refreshSessionStatusPanel: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    pollActiveSessionView: () => {},
    refreshSessionStatusNow: () => {},
    applyRunningChanged: () => {},
    setSessionPendingTriggerIndicator: () => {},
    updateSidebarSessionName: () => {},
    handleSidebarSearch: () => {},
    switchSidebarFilter: () => {},
    currentFilter: 'all',
    ...extraContext,
  };
  context.globalThis = context;
  vm.createContext(context);
  loadWebsocketModules(context);
  return {context, FakeWebSocket: context.WebSocket};
}

// The two-file load sequence on its own, for a harness whose vm context
// already exists: websocket.js's transport, then the core handlers whose
// registrations it accepts. The caller's context supplies the page globals
// the files read (SESSION_ID, WebSocket, wsUrlWithToken, the sidebar hooks).
function loadWebsocketModules(context) {
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  vm.runInContext(WEBSOCKET_HANDLERS_JS, context, {filename: 'websocket-handlers.js'});
}

module.exports = { loadWebsocketContext, loadWebsocketModules };
