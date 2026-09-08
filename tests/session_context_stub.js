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
    renderProseMarkdown: (txt) => txt,
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    formatBubbleTime: (txt) => txt,
    shouldAutoScroll: () => true,
    showScrollToBottom: () => {},
    showToast: () => {},
    loadedThreads: {clear: () => {}},
    _backlogLoaded: false,
    BACKEND_OPTIONS: overrides.BACKEND_OPTIONS || {},
    BACKEND_TYPES: overrides.BACKEND_TYPES || {},
    BACKEND_ALIASES: overrides.BACKEND_ALIASES || {},
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

// The wire path web/static/js/sidebar/session-view.js posts switch telemetry
// to (reportSwitchEvent); an endpoint move on either side breaks the pin.
const SWITCH_TELEMETRY_URL = '/api/diag/switch-events';

// One sidebar row as the harnesses mount it: querySelector answers only
// '.session-name' (the selector the row rendering resolves); sidebar callers
// resolving other selectors against a row (archived.js's group move)
// null-guard the miss.
function makeSidebarRow(sessionId, name) {
  const nameEl = createElement({textContent: name});
  return createElement({
    id: 'session-' + sessionId,
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
  });
}

// The bootstrap body switchSession renders. oldestMessageOrdinal and hasMore
// are the two fields the pagination tests vary; everything else is the fixed
// one-turn shape the switch flow reads.
function bootstrapPayload(sessionId, oldestMessageOrdinal, hasMore) {
  return {
    session: {id: sessionId, name: 'Session ' + sessionId, backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'hello from ' + sessionId, event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: oldestMessageOrdinal,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: hasMore,
    threads: [],
    triggers: [],
  };
}

// The document lookups the switch harnesses share: static ids answer from
// `elements`, and the nodes the loaded modules create at runtime under
// `messages` (placeholder rows, rendered bubbles) answer from its children —
// they never enter the static map. Call before createChatSidebarContext: the
// loaded modules bind the lookups at load time. `messages` is null when the
// harness mounts no chat container (usage-only harnesses).
function installSessionDocumentLookups(context, elements, messages, rows) {
  context.document.getElementById = (id) => {
    const fromMap = elements.get(id);
    if (fromMap) return fromMap;
    for (const child of messages ? messages.children : []) {
      if (child.id === id) return child;
    }
    return null;
  };
  context.document.querySelectorAll = (sel) => (sel === '[id^="session-"]' ? rows : []);
  context.document.querySelector = () => null;
}

// The page timers never fire under test: status polls and reconnects must not
// race the assertions.
function stubPageTimers(context) {
  context.setInterval = () => 1;
  context.setTimeout = () => 1;
  context.clearInterval = () => {};
  context.clearTimeout = () => {};
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

module.exports = {
  baseSessionContext,
  createChatSidebarContext,
  buildSidebarFilterElements,
  buildUsageElements,
  SWITCH_TELEMETRY_URL,
  makeSidebarRow,
  bootstrapPayload,
  installSessionDocumentLookups,
  stubPageTimers,
};
