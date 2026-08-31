// Core of the vm context a chat/sidebar harness test builds: the session
// globals, storage and element stubs, and the no-op chat globals those
// harnesses share. Each harness layers its own fetch/timer/document-lookup
// variants onto the returned context, then wraps it in
// createChatSidebarContext(context) — fork mutations must land before that
// call, because the loaded chat/sidebar modules bind or shadow globals at
// load time.
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { createElement, createEscapingElement } = require('./dom_element_stub');

const COMPAT_LOADER_JS = readStatic('compat-loader.js');
const CHAT_JS = readStatic('chat.js');
const SIDEBAR_JS = readStatic('sidebar.js');
const PAGE_TIMERS_JS = readStatic('page-timers.js');

function baseSessionContext(overrides = {}) {
  const elements = overrides.elements || new Map();
  const localStorageData = new Map(Object.entries(overrides.localStorageItems || {}));

  const context = {
    SESSION_ID: 'session-a',
    THINKING_SINCE: null,
    DRAFT_KEY: null,
    ACTIVE_BACKEND_ID: overrides.ACTIVE_BACKEND_ID || 'claude-opus-4.6',
    masterThinking: false,
    switching: false,
    reconnectTimer: null,
    workersPollInterval: null,
    streamBuf: '',
    streamTs: null,
    catchupDone: false,
    pendingUserMsg: false,
    uploadedFiles: [],
    localStorage: {
      getItem: (key) => localStorageData.has(key) ? localStorageData.get(key) : null,
      setItem: (key, value) => { localStorageData.set(key, String(value)); },
      removeItem: (key) => { localStorageData.delete(key); },
    },
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: ''},
    history: {pushState: () => {}},
    console: {error: () => {}, log: () => {}},
    URLSearchParams,
    AbortController,
    document: {
      // document lookups (getElementById/querySelector*) differ per harness and
      // are assigned by each fork; createElement is shared.
      createElement: createEscapingElement,
      body: createElement({tagName: 'BODY'}),
      addEventListener: () => {},
      removeEventListener: () => {},
    },
    disconnectWS: () => {},
    connectWS: () => {},
    resetVoiceState: () => {},
    renderFileChips: () => {},
    hideSlashPopup: () => {},
    hideStreaming: () => {},
    showStreaming: () => {},
    updateSidebarHighlight: () => {},
    pollSessionStatus: () => Promise.resolve(false),
    pollWorkers: () => {},
    autoResize: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    relativeTime: (txt) => txt,
    updateRelativeTimes: () => {},
    formatTokens: (n) => `${Math.round(n / 1000)}k`,
    formatUsageCostValue: (cost) => cost == null ? 'N/A' : '$' + cost.toFixed(2),
    formatLastRun: (txt) => txt,
    escapeHtml: (v) => v,
    renderWorkersTab: () => {},
    switchTab: () => {},
    marked: {parse: (txt) => txt},
    fixNestedFences: (txt) => txt,
    renderChatMath: () => {},
    formatBubbleTime: (txt) => txt,
    shouldAutoScroll: () => true,
    showScrollToBottom: () => {},
    showToast: () => {},
    loadedThreads: {clear: () => {}},
    _backlogLoaded: false,
    BACKEND_OPTIONS: overrides.BACKEND_OPTIONS || {},
    BACKEND_TYPES: overrides.BACKEND_TYPES || {},
    // config.js's shared header literal; index.html loads config.js before the
    // chat/sidebar modules createChatSidebarContext fans out to.
    JSON_HEADERS: {'Content-Type': 'application/json'},
  };
  context.window = {addEventListener: () => {}, innerHeight: 800};
  context.CSS = {escape: (value) => String(value)};

  return {context, elements, localStorageData};
}

// page-timers before chat/sidebar, matching the script order in
// web/templates/index.html; compat-loader before sidebar.js, which fans out to
// sidebar/*.js through it (see the header of web/static/js/sidebar.js).
function createChatSidebarContext(context) {
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(COMPAT_LOADER_JS, context, {filename: 'compat-loader.js'});
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
}

// Map keys are the element ids renderUsageFromData looks up in
// web/static/js/sidebar/session-view.js; an id rename on either side breaks the lookup.
function buildUsageElements() {
  return new Map([
    ['usage-indicator', createElement({className: 'hidden'})],
    ['usage-bar', createElement({className: 'h-full rounded-full bg-blue-500', style: {width: '0%'}})],
    ['usage-compact-line', createElement({className: 'absolute top-0 h-full w-0.5 bg-white hidden', style: {left: '0%'}})],
    ['usage-text', createElement({textContent: ''})],
    ['usage-cost', createElement({textContent: ''})],
  ]);
}

module.exports = {baseSessionContext, createChatSidebarContext, buildUsageElements};
