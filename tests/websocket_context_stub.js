// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/websocket.js, shared by the node
// --test websocket harnesses: one base context (the page-load state globals,
// silent console, a fresh FakeWebSocket class per build, the no-key
// wsUrlWithToken page-load order, and a no-op for every hook websocket.js
// calls into the chat/sidebar modules), the harness's own globals spread
// over that base (SESSION_ID, document, setTimeout, the recorders), then
// one load sequence — createContext, websocket.js — so the load order both
// suites depend on lives in one place.
// ---------------------------------------------------------------------------
const vm = require('node:vm');

const { createFakeWebSocketClass } = require('./fake_websocket');
const { readStatic } = require('./read_static');

const WEBSOCKET_JS = readStatic('websocket.js');

function loadWebsocketContext(extraContext) {
  const context = {
    eventCursor: 0,
    reconnectDelay: 1000,
    thinkingStart: null,
    sessionUnread: {},
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
    startThinking: () => {},
    stopThinking: () => {},
    pollActiveSessionView: () => {},
    renderExtUsage: () => {},
    refreshSessionStatusNow: () => {},
    setSessionIndicator: () => {},
    getSessionIndicatorState: () => ({}),
    setSessionPendingTriggerIndicator: () => {},
    updateSidebarSessionName: () => {},
    showDiffModal: () => {},
    handleSidebarSearch: () => {},
    switchSidebarFilter: () => {},
    currentFilter: 'all',
    ...extraContext,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  return {context, FakeWebSocket: context.WebSocket};
}

module.exports = { loadWebsocketContext };
