// ---------------------------------------------------------------------------
// vm-context loader for web/static/js/websocket.js, shared by the node
// --test websocket harnesses. It holds one base context: the page-load state
// globals, silent console, a fresh FakeWebSocket class per build, and the
// no-key wsUrlWithToken page-load order. Every hook websocket.js calls into the
// chat/sidebar modules is a no-op. The harness's own globals spread over that
// base (SESSION_ID, document, setTimeout, the recorders). Then comes one load
// sequence: createContext, websocket.js, websocket-handlers.js. The load order
// both suites depend on lives in one place. A harness with a context of its own
// (the sidebar indicator harness loading the real status.js) takes the same load
// sequence through loadWebsocketModules.
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
    document: {
      getElementById: () => null,
      // websocket.js announces socket closes, opens, replays and page-load
      // causes as page events; a harness asserts on these through
      // `dispatched` when it needs the announcements themselves.
      dispatchEvent: () => true,
    },
    CustomEvent,
    TextEncoder,
    sessionStorage: {getItem: () => null, setItem: () => {}, removeItem: () => {}},
    Chat: {activeTurnEngineFor: () => null},
    localStorage: {getItem: () => null},
    // No stored key: mirrors page-load order config.js → websocket.js on the no-key path.
    wsUrlWithToken: (path) => path,
    location: {protocol: 'http:', host: 'localhost:8000', reload: () => {}},
    console: {log: () => {}, error: () => {}},
    WebSocket: createFakeWebSocketClass(),
    clearTimeout: () => {},
    hideStreaming: () => {},
    showStreaming: () => {},
    appendMessage: () => {},
    appendMessageObject: () => {},
    observeWebStatusMessages: () => {},
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
  prepareWebsocketGlobals(context);
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  vm.runInContext(WEBSOCKET_HANDLERS_JS, context, {filename: 'websocket-handlers.js'});
}

// The page events websocket.js announces (socket closes, opens, replays and
// page-load causes) need a dispatching document, the event constructor and the
// replay byte counter. A harness that records the announcements asserts on
// them; the defaults drop them, like every other no-op page hook here.
function prepareWebsocketGlobals(context) {
  if (context.document && !context.document.dispatchEvent) context.document.dispatchEvent = () => true;
  if (!context.CustomEvent) context.CustomEvent = CustomEvent;
  if (!context.TextEncoder) context.TextEncoder = TextEncoder;
}

module.exports = { loadWebsocketContext, loadWebsocketModules, prepareWebsocketGlobals };
