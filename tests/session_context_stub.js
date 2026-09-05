// Core of the vm context a chat/sidebar harness test builds: the session
// globals, storage and element stubs, and the no-op chat globals those
// harnesses share. Each harness layers its own fetch/timer/document-lookup
// variants onto the returned context, then wraps it in
// createChatSidebarContext(context) — fork mutations must land before that
// call, because the loaded chat/sidebar modules bind or shadow globals at
// load time.
const vm = require('node:vm');

const {readStatic, chatModules, sidebarModules, runStaticModules} = require('./read_static');
const {createElement, createEscapingElement} = require('./dom_element_stub');

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
    // config.js's shared literal pair; index.html loads config.js before the
    // chat/sidebar modules createChatSidebarContext fans out to.
    JSON_HEADERS: {'Content-Type': 'application/json'},
    PROGRESS_BAR_FILL_CLASS: 'h-full rounded-full transition-all duration-300',
  };
  context.window = {addEventListener: () => {}, innerHeight: 800};
  context.CSS = {escape: (value) => String(value)};

  return {context, elements, localStorageData};
}

// page-timers before the chat and sidebar modules, matching the script order
// in web/templates/index.html; the chat and sidebar lists are that page's
// /static/js/chat/ and /static/js/sidebar/ script tags in document order.
function createChatSidebarContext(context) {
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  runStaticModules(context, chatModules());
  runStaticModules(context, sidebarModules());
}

// Map keys are the element ids web/static/js/sidebar/filters.js reaches:
// getElementById('filter-' + name) over the registered filter names plus
// getElementById('cron-add-btn'), and the 'filter-pill' class filterPillClass
// stamps. An id or class rename on either side breaks the lookup.
function buildSidebarFilterElements() {
  return new Map([
    ['filter-all', createElement({className: 'filter-pill'})],
    ['filter-starred', createElement({className: 'filter-pill'})],
    ['filter-archived', createElement({className: 'filter-pill'})],
    ['filter-scheduled', createElement({className: 'filter-pill'})],
    ['cron-add-btn', createElement()],
  ]);
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

module.exports = {baseSessionContext, createChatSidebarContext, buildSidebarFilterElements, buildUsageElements};
