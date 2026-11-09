// Core of the vm context a chat/sidebar harness test builds: the session
// globals, storage and element stubs, and the no-op chat globals those
// harnesses share. Each harness layers its own fetch/timer/document-lookup
// variants onto the returned context, then wraps it in
// createChatSidebarContext(context). Fork mutations must land before that call,
// because the loaded chat/sidebar modules bind or shadow globals at load time.
const vm = require('node:vm');

const {readStatic, readPackageStatic, chatModules, sidebarModules, runStaticModules} = require('./read_static');
const {createElement, createEscapingElement} = require('./dom_element_stub');

const PAGE_TIMERS_JS = readStatic('page-timers.js');
const CRON_JS = readPackageStatic('features', 'cron', 'cron.js');
// The connection layer's request-abort registry; the page loads it before every chat and
// sidebar module, so the harnesses run it before those too.
const VIEW_REQUESTS_JS = readStatic('view-requests.js');

function baseSessionContext(overrides = {}) {
  const elements = overrides.elements || new Map();
  const localStorageData = new Map(Object.entries(overrides.localStorageItems || {}));

  const sessionStorageData = new Map();
  const context = {
    SESSION_ID: 'session-a',
    // The page global index.html renders next to the ?v= script URLs; websocket.js
    // sends it as the handshake's asset_version.
    STATIC_ASSET_VERSION: 'stub-asset-version',
    // index.html's host fact; the displayed-session module restates it in the tab title.
    PAGE_HOSTNAME: 'localhost:8000',
    // index.html's active-session name; the displayed-session module starts from it.
    SESSION_NAME: 'Session session-a',
    // The server-embedded initial render payload (index.html); the sidebar's
    // restore path reads it to detect a v2 task-node deep link.
    SESSION_BOOTSTRAP: overrides.SESSION_BOOTSTRAP !== undefined ? overrides.SESSION_BOOTSTRAP : null,
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
    pendingUserRequestIds: new Set(),
    uploadedFiles: [],
    localStorage: {
      getItem: (key) => localStorageData.has(key) ? localStorageData.get(key) : null,
      setItem: (key, value) => { localStorageData.set(key, String(value)); },
      removeItem: (key) => { localStorageData.delete(key); },
    },
    sessionStorage: {
      getItem: (key) => sessionStorageData.has(key) ? sessionStorageData.get(key) : null,
      setItem: (key, value) => { sessionStorageData.set(key, String(value)); },
      removeItem: (key) => { sessionStorageData.delete(key); },
    },
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: ''},
    history: {pushState: () => {}},
    console: {error: () => {}, log: () => {}},
    URLSearchParams,
    AbortController,
    AbortSignal,
    document: {
      // document lookups (getElementById/querySelector*) differ per harness and
      // are assigned by each fork; createElement is shared.
      createElement: createEscapingElement,
      body: createElement({tagName: 'BODY'}),
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => true,
    },
    // session-view.js announces the end of a session switch as a page event.
    CustomEvent,
    // websocket.js's switch seam, recorded so a harness asserts the frames hold
    // and the subscription requests a switch makes. The subscription request
    // answers success by default; a harness overrides to refuse it.
    holdSessionFrames: (sessionId) => {
      context.heldSwitchTargets.push(sessionId);
      context.switchPending = true;
    },
    requestSessionSubscription: (sessionId, cursor, report) => {
      context.subscribeRequests.push({session_id: sessionId, cursor, report});
      context.switchPending = false;
    },
    resumeViewSubscription: () => {
      context.resumedSubscriptions++;
      context.switchPending = false;
    },
    canRetrySessionSubscription: () => !context.switchPending,
    closeSessionSocket: () => { context.closedSockets++; },
    closedSockets: 0,
    heldSwitchTargets: [],
    subscribeRequests: [],
    resumedSubscriptions: 0,
    switchPending: false,
    renderFileChips: () => {},
    hideStreaming: () => {},
    showStreaming: () => {},
    updateSidebarHighlight: () => {},
    pollSessionStatus: () => Promise.resolve(false),
    autoResize: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    // platform.js's desktop face: the chat status panel reads isMobile and registers its layout callback at load.
    platform: {isMobile: false, onChange() {}},
    relativeTime: (txt) => txt,
    // utils.js's trigger clock form; harnesses needing the real one load utils.js
    // (pending_triggers_tray.test.js does) and its top-level functions overwrite these.
    clockTimeHM: () => 'HH:mm',
    dateClockMDHM: () => 'MM/DD HH:mm',
    relativeFireIn: () => '(in 0m)',
    updateRelativeTimes: () => {},
    formatTokens: (n) => `${Math.round(n / 1000)}k`,
    formatUsageCostValue: (cost) => cost == null ? 'N/A' : '$' + cost.toFixed(2),
    escapeHtml: (v) => v,
    switchTab: () => {},
    tabAfterSessionSwitch: () => 'chat',
    marked: {parse: (txt) => txt},
    fixNestedFences: (txt) => txt,
    renderProseMarkdown: (txt) => txt,
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    formatBubbleTime: (txt) => txt,
    shouldAutoScroll: () => true,
    showScrollToBottom: () => {},
    showToast: () => {},
    BACKEND_OPTIONS: overrides.BACKEND_OPTIONS || {},
    BACKEND_TYPES: overrides.BACKEND_TYPES || {},
    BACKEND_ALIASES: overrides.BACKEND_ALIASES || {},
    // index.html's picker list: the config's visible options and pools, options first. The backend
    // pickers read it; BACKEND_OPTIONS stays the full label map.
    BACKEND_PICKER_ENTRIES: overrides.BACKEND_PICKER_ENTRIES || [],
    // index.html's server facts: the compact button's facts by backend type and
    // the configured default backend id.
    BACKEND_COMPACT: overrides.BACKEND_COMPACT || {},
    DEFAULT_BACKEND_ID: overrides.DEFAULT_BACKEND_ID || 'claude-opus-4.6',
    // config.js's shared literals and the draft-save seam; index.html loads
    // config.js before the chat/sidebar modules createChatSidebarContext fans
    // out to.
    JSON_HEADERS: {'Content-Type': 'application/json'},
    // config.js's freshness labels (sidebar/last-state.js and the panels paint them).
    NOT_CURRENT_LABEL: '不是最新',
    NO_LOCAL_COPY_LABEL: '没有本机副本',
    OFFLINE_PANEL_LABEL: '断网，联网后加载',
    crypto: {randomUUID: () => 'archive-request-id'},
    PROGRESS_BAR_FILL_CLASS: 'h-full rounded-full transition-all duration-300',
    saveDraftNow: () => {},
  };
  context.window = {addEventListener: () => {}, innerHeight: 800};
  // A voice run's startup record stamps its click on the page's monotonic clock,
  // and its report carries the browser's user agent.
  context.performance = {now: () => 0};
  context.navigator = {userAgent: 'TestAgent/1.0'};
  context.CSS = {escape: (value) => String(value)};

  return {context, elements, localStorageData, sessionStorageData};
}

// page-timers before the chat and sidebar modules, matching the script order
// in web/templates/index.html. The chat and sidebar lists are that page's
// /static/js/chat/ and /static/js/sidebar/ script tags in document order.
function createChatSidebarContext(context) {
  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(VIEW_REQUESTS_JS, context, {filename: 'view-requests.js'});
  runStaticModules(context, chatModules());
  standInForUsageJs(context);
  runStaticModules(context, sidebarModules());
}

// usage.js loads between the chat and the sidebar modules on the page. These
// harnesses stub its paint globals (showStreaming, hideStreaming), and this
// stands in for the registration session-view.js makes at load.
function standInForUsageJs(context) {
  context.Chat.setStreamDraftModeSupplier = () => {};
}

// The cron script: a harness whose rows, menus or Workspace badge show the
// cron parts runs it after createChatSidebarContext. The page runs a package's
// script after the core modules the same way.
function loadCronPackage(context) {
  // A harness runs the page's scripts in include order, as the document parses: the module's page part fetches it
  // after the document on a real page, and the late-load path has its own tests (cron_broken_ui.test.js).
  const readyState = context.document.readyState;
  context.document.readyState = 'loading';
  try {
    vm.runInContext(CRON_JS, context, {filename: 'cron.js'});
  } finally {
    if (readyState === undefined) delete context.document.readyState;
    else context.document.readyState = readyState;
  }
}

// One sidebar row as the harnesses mount it: querySelector answers only
// '.session-name' (the selector the row rendering resolves). Sidebar callers
// resolving other selectors against a row (archived.js's group move) null-guard
// the miss.
function makeSidebarRow(sessionId, name) {
  const nameEl = createElement({textContent: name});
  nameEl.getBoundingClientRect = () => ({top: 0, left: 0, right: 0, bottom: 20, width: 200, height: 20});
  return createElement({
    id: 'session-' + sessionId,
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
  });
}

// One session-metadata fixture as the sidebar wire format carries it. Only the
// fields every sidebar harness shares live here. A harness whose fixture must
// leave a field undefined (usage_poll reads no status) or default it
// differently (delete_backfill's Work group) passes that field in overrides.
// It is never added to this base.
function makeSessionMeta(id, overrides = {}) {
  return {
    id,
    name: `Session ${id}`,
    group: null,
    updated_at: '2026-04-02T04:00:00Z',
    has_unread: false,
    has_running_tasks: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    starred: false,
    backend: 'claude-opus-4.6',
    ...overrides,
  };
}

// The page's sidebar rows as it carries them: one array per field, in row
// order, with null where a row leaves the field out. A harness that serves
// rows through INITIAL_SESSIONS builds them with this, as index.html's
// server-rendered page does.
function sessionColumns(rows) {
  const columns = {};
  rows.forEach((row, index) => {
    Object.keys(row).forEach((field) => {
      if (!columns[field]) columns[field] = rows.map(() => null);
      columns[field][index] = row[field];
    });
  });
  return columns;
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
// `elements`. The nodes the loaded modules create at runtime under `messages`
// (placeholder rows, rendered bubbles) answer from its children. They never
// enter the static map. Call before createChatSidebarContext: the
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

// The switch-flow harness skeleton the sidebar switch suites share: a chat
// container, the five static chrome ids, one row per session, the document
// lookups, and the stubbed page timers. scrollHeight is per-suite geometry.
// The switch tail's auto-continue fills only while the painted content is short
// (scrollHeight <= clientHeight + 80), and the pagination trigger fires only
// within 80px of the top. So each suite picks the height that parks its
// scenario. Every fork (fields, fetch, contextTweaks) lands before
// createChatSidebarContext, whose modules bind or shadow globals at load time.
// fetch receives the finished harness record, so its closures reach the
// recorded fields.
function buildSwitchFlowHarness({rows, scrollHeight, fields, fetch, contextTweaks}) {
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = scrollHeight;
  messages.scrollTop = 0;
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', createElement({id: 'header-session-name'})],
    ['backend-badge', createElement()],
    ['msg-input', createElement()],
    ['freshness-mark', createElement({id: 'freshness-mark'})],
    ...rows.map((row) => [row.id, row]),
  ]);
  const h = {messages, elements, ...fields};
  const {context} = baseSessionContext({elements});
  context.eventCursor = 0;
  if (contextTweaks) contextTweaks(context, h);
  installSessionDocumentLookups(context, elements, messages, rows);
  // async so the page code may chain .catch on the result even when the
  // suite's fetch answers a plain object.
  context.fetch = async (url, opts = {}) => fetch(h, url, opts);
  stubPageTimers(context);
  createChatSidebarContext(context);
  h.context = context;
  return h;
}

// The inline-fire twin: every timer callback runs inside the call that
// schedules it. So a flow's own timer-driven work (a refresh scheduled during a
// paint) is observed before the assertions run, and a leaked fetch cannot hide
// behind a pending timer. Call before createChatSidebarContext.
function inlinePageTimers(context) {
  context.setInterval = () => 0;
  context.setTimeout = (fn) => { fn(); return 0; };
  context.clearInterval = () => {};
  context.clearTimeout = () => {};
}

// Map keys are the element ids web/static/js/sidebar/filters.js reaches:
// getElementById('filter-' + name) over the registered filter names plus
// getElementById('cron-add-btn'). They also carry the 'filter-pill' class
// filterPillClass stamps. An id or class rename on either side breaks the
// lookup.
function buildSidebarFilterElements() {
  return new Map([
    ['filter-all', createElement({className: 'filter-pill'})],
    ['filter-threads', createElement({className: 'filter-pill'})],
    ['filter-starred', createElement({className: 'filter-pill'})],
    ['filter-archived', createElement({className: 'filter-pill'})],
    ['filter-scheduled', createElement({className: 'filter-pill'})],
    ['cron-add-btn', createElement()],
  ]);
}

// The five indicator element kinds web/static/js/sidebar paints per row, one
// hidden element each as `<kind>-<id>`. An id or kind rename on either side
// breaks the shown() lookup below.
const SIDEBAR_ICON_KINDS = ['spinner', 'worker-indicator', 'waiting-indicator', 'unread', 'subtree-unread'];

function buildSidebarIndicatorElements(ids) {
  const els = new Map();
  ids.forEach((id) => {
    SIDEBAR_ICON_KINDS.forEach((kind) => {
      els.set(kind + '-' + id, createElement({className: 'hidden'}));
    });
  });
  return els;
}

// A sidebar context whose document resolves only the session list, the filter
// pills, and ids' indicator elements; shown(id) reports which indicator
// elements are visible on that row.
function buildSidebarIndicatorContext(ids) {
  const nav = createElement();
  const elements = new Map([['session-list', nav], ...buildSidebarFilterElements(), ...buildSidebarIndicatorElements(ids)]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = 'none';
  context.INITIAL_SESSIONS = {};
  context.INITIAL_LOAD_ERRORS = [];
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  context.fetch = async (url) => { throw new Error('unexpected fetch ' + url); };
  createChatSidebarContext(context);
  const shown = (id) => ({
    spinner: !elements.get('spinner-' + id).classList.contains('hidden'),
    gear: !elements.get('worker-indicator-' + id).classList.contains('hidden'),
    clock: !elements.get('waiting-indicator-' + id).classList.contains('hidden'),
    dot: !elements.get('unread-' + id).classList.contains('hidden'),
    subtreeMark: !elements.get('subtree-unread-' + id).classList.contains('hidden'),
  });
  return {context, nav, shown};
}

// A fetched row list landing on the page: its unread facts apply through the
// reply gate at the request's sequence, then the list paints from the facts.
// The page's fetch paths (filters.js listingRows) do the same two steps.
function paintListReply(context, rows, filter) {
  context.applyUnreadReply(rows, context.factSeqAtRequest());
  context.renderSessionList(rows, filter);
}

// Map keys are the element ids renderUsageFromData looks up in
// web/static/js/sidebar/session-view.js; an id rename on either side breaks the lookup.
function buildUsageElements() {
  // The bar and the compact line sit inside the meter track, as they do in
  // index.html: renderUsageFromData hides the track through the bar's parent.
  const meter = createElement();
  const bar = createElement({className: 'h-full rounded-full bg-blue-500', style: {width: '0%'}});
  const compactLine = createElement({className: 'absolute top-0 h-full w-0.5 bg-white hidden', style: {left: '0%'}});
  meter.appendChild(bar);
  meter.appendChild(compactLine);
  return new Map([
    ['usage-indicator', createElement({className: 'hidden'})],
    ['usage-meter', meter],
    ['usage-bar', bar],
    ['usage-compact-line', compactLine],
    ['usage-text', createElement({textContent: ''})],
    ['usage-cost', createElement({textContent: ''})],
  ]);
}

module.exports = {
  baseSessionContext,
  buildSidebarIndicatorContext,
  paintListReply,
  createChatSidebarContext,
  standInForUsageJs,
  loadCronPackage,
  buildSidebarFilterElements,
  buildUsageElements,
  buildSwitchFlowHarness,
  makeSidebarRow,
  makeSessionMeta,
  sessionColumns,
  bootstrapPayload,
  installSessionDocumentLookups,
  stubPageTimers,
  inlinePageTimers,
};
