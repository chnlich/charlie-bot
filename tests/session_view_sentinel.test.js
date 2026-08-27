const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const COMPAT_LOADER_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'compat-loader.js'),
  'utf8'
);
const CHAT_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat.js'),
  'utf8'
);
const SIDEBAR_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'sidebar.js'),
  'utf8'
);
const PAGE_TIMERS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'page-timers.js'),
  'utf8'
);

function createClassList(initial = '') {
  const names = new Set(String(initial).split(/\s+/).filter(Boolean));
  return {
    add: (...items) => items.forEach((item) => { if (item) names.add(item); }),
    remove: (...items) => items.forEach((item) => names.delete(item)),
    contains: (item) => names.has(item),
    toggle: (item, force) => {
      if (force === undefined) {
        if (names.has(item)) { names.delete(item); return false; }
        names.add(item); return true;
      }
      if (force) names.add(item); else names.delete(item);
      return !!force;
    },
    toString: () => Array.from(names).join(' '),
  };
}

function createElement(overrides = {}) {
  let innerHTML = overrides.innerHTML || '';
  const element = {
    tagName: overrides.tagName || 'DIV',
    value: overrides.value || '',
    textContent: overrides.textContent || '',
    checked: overrides.checked || false,
    disabled: overrides.disabled || false,
    readOnly: overrides.readOnly || false,
    dataset: overrides.dataset || {},
    style: overrides.style || {},
    id: overrides.id || '',
    children: [],
    options: [],
    parentNode: null,
    classList: createClassList(overrides.className || ''),
    appendChild(child) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      this.children.push(child);
      if (child && child.tagName === 'OPTION') {
        this.options.push(child);
        if (child.selected || !this.value) this.value = child.value;
      }
      return child;
    },
    prepend(child) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      this.children.unshift(child);
      return child;
    },
    insertBefore(child, referenceChild) {
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this;
      if (!referenceChild) { this.children.push(child); return child; }
      const index = this.children.indexOf(referenceChild);
      if (index === -1) throw new Error('reference child not found');
      this.children.splice(index, 0, child);
      return child;
    },
    removeChild(child) {
      const index = this.children.indexOf(child);
      if (index === -1) throw new Error('child not found');
      this.children.splice(index, 1);
      if (child) child.parentNode = null;
      return child;
    },
    after(child) {
      if (!this.parentNode) return;
      if (child && child.parentNode) child.parentNode.removeChild(child);
      if (child) child.parentNode = this.parentNode;
      const index = this.parentNode.children.indexOf(this);
      this.parentNode.children.splice(index + 1, 0, child);
    },
    remove() {
      if (this.parentNode) this.parentNode.removeChild(this);
      this.removed = true;
    },
    focus() {},
    addEventListener(type, handler) {
      if (!this._listeners) this._listeners = {};
      if (!this._listeners[type]) this._listeners[type] = [];
      this._listeners[type].push(handler);
    },
    setAttribute(name, value) { this[name] = value; },
    getAttribute(name) { return this[name]; },
    querySelectorAll: () => [],
    querySelector: () => null,
  };

  Object.defineProperty(element, 'firstElementChild', {
    get() { return element.children[0] || null; },
  });
  Object.defineProperty(element, 'lastChild', {
    get() { return element.children[element.children.length - 1] || null; },
  });
  Object.defineProperty(element, 'innerHTML', {
    get() { return innerHTML; },
    set(value) { innerHTML = value; element.children = []; element.options = []; },
  });

  return Object.assign(element, overrides);
}

function buildContext(overrides = {}) {
  const fetchCalls = [];
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
    fetch: overrides.fetch || (async (url) => {
      fetchCalls.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    }),
    setInterval: () => 1,
    setTimeout: (fn) => { if (overrides.autoTimeout) fn(); return 1; },
    clearInterval: () => {},
    clearTimeout: () => {},
    URLSearchParams,
    AbortController,
    document: {
      getElementById: (id) => {
        const fromMap = elements.get(id);
        if (fromMap) return fromMap;
        const container = elements.get('messages');
        if (container) {
          for (const child of container.children) {
            if (child.id === id) return child;
          }
        }
        return null;
      },
      querySelectorAll: () => [],
      querySelector: () => null,
      createElement: (tagName) => {
        const el = createElement({tagName: String(tagName).toUpperCase()});
        let rawText = '';
        Object.defineProperty(el, 'textContent', {
          get() { return rawText; },
          set(v) {
            rawText = String(v);
            el.innerHTML = String(v)
              .replaceAll('&', '&amp;')
              .replaceAll('<', '&lt;')
              .replaceAll('>', '&gt;')
              .replaceAll('"', '&quot;')
              .replaceAll("'", '&#39;');
          },
        });
        return el;
      },
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
    formatNextRun: (txt) => txt,
    formatLastRun: (txt) => txt,
    escapeHtml: (v) => v,
    renderUserMessageBubble: (content) => `<div>${content || ''}</div>`,
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
    alert: () => {},
    confirm: () => true,
  };
  context.window = {addEventListener: () => {}, innerHeight: 800};
  context.CSS = {escape: (value) => String(value)};

  vm.createContext(context);
  vm.runInContext(PAGE_TIMERS_JS, context, {filename: 'page-timers.js'});
  vm.runInContext(COMPAT_LOADER_JS, context, {filename: 'compat-loader.js'});
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, fetchCalls, elements};
}

function makeMessages(overrides = {}) {
  const messages = createElement({id: 'messages'});
  messages.scrollTop = overrides.scrollTop ?? 0;
  messages.clientHeight = overrides.clientHeight ?? 500;
  messages.scrollHeight = overrides.scrollHeight ?? 1000;
  return messages;
}

function renderSessionViewWith(context, elements, hasMore, opts = {}) {
  const messages = elements.get('messages');
  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: opts.existingMessages || [{role: 'assistant', content: 'hello', event_index: 5}],
    pending_draft: null,
    event_count: opts.eventCount || 6,
    oldest_message_ordinal: opts.oldestOrdinal ?? 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: hasMore,
  });
  // renderSessionView scrolls to bottom; reset for pagination tests.
  messages.scrollTop = 0;
}

function findSentinel(elements) {
  return elements.get('messages').children.find(c => c.id === 'load-more-sentinel');
}

// ---------------------------------------------------------------------------
// Acceptance test 6: Sentinel states, failure recovery, scroll suppress,
//                     bounded viewport fill
// ---------------------------------------------------------------------------

test('sentinel renders idle text on first paint when has_more is true', () => {
  const messages = makeMessages();
  const {context, elements} = buildContext({
    elements: new Map([
      ['messages', messages],
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
      ['input-model-badge', createElement()],
    ]),
  });

  renderSessionViewWith(context, elements, true);

  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must be prepended when has_more is true');
  assert.equal(sentinel.getAttribute('data-state'), 'idle');
  assert.equal(sentinel.innerHTML, 'Scroll up for older messages');
});

test('sentinel shows loading text while a request is in flight', async () => {
  const messages = makeMessages({scrollTop: 0});
  let resolveFetch;
  const fetchPromise = new Promise((resolve) => { resolveFetch = resolve; });
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => fetchPromise.then(() => ({
      ok: true,
      async json() { return {has_more: false, next_before: 0, messages: []}; },
    })),
  });

  renderSessionViewWith(context, elements, true);

  const loadPromise = context.loadOlderIfNeeded(messages);
  const sentinel = findSentinel(elements);
  assert.equal(sentinel.getAttribute('data-state'), 'loading');
  assert.match(sentinel.innerHTML, /Loading older messages/);

  resolveFetch();
  await loadPromise;
});

test('failed fetch keeps has_more and leaves a clickable failed sentinel', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => { fetchCount++; return {ok: false, status: 500}; },
  });

  renderSessionViewWith(context, elements, true);

  await context.loadOlderIfNeeded(messages);

  // Failure must NOT clear has_more — verify by checking that a second call
  // (or clicking the failed sentinel) triggers another fetch.
  assert.equal(fetchCount, 1, 'first fetch should have been attempted');

  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain after failure');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /Failed to load older messages/);
  assert.match(sentinel.innerHTML, /click to retry/);
  assert.equal(typeof sentinel.onclick, 'function', 'failed sentinel must be clickable');

  // Clicking the failed sentinel retries — proving has_more was NOT cleared.
  messages.scrollTop = 0;
  sentinel.onclick();
  // The onclick call is async; wait a tick for the fetch to register.
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(fetchCount, 2, 'clicking failed sentinel must retry (has_more was not cleared)');
});

test('programmatic scroll restore does not trigger another fetch', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => {
      fetchCount++;
      return {
        ok: true,
        async json() {
          return {
            has_more: false,
            next_before: 0,
            messages: [{id: 'msg-old', role: 'user', content: 'old', event_index: 1}],
          };
        },
      };
    },
  });

  renderSessionViewWith(context, elements, true);

  // Register the scroll handler (initScrollPagination) so we can test the
  // suppress flag.
  context.initScrollPagination();

  await context.loadOlderIfNeeded(messages);
  assert.equal(fetchCount, 1, 'exactly one fetch for the page');

  // Simulate the scroll event dispatched by the programmatic scrollTop
  // assignment inside loadOlderIfNeeded. The suppressScrollLoad flag should
  // absorb it — no additional fetch.
  const scrollHandlers = messages._listeners?.scroll || [];
  assert.ok(scrollHandlers.length > 0, 'scroll handler must be registered');
  for (const handler of scrollHandlers) {
    handler();
  }

  assert.equal(fetchCount, 1, 'programmatic scroll must not trigger another fetch');
});

test('viewport fill stops at 5 consecutive auto-loads', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 100, clientHeight: 100});
  let fetchCount = 0;
  const {context} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetchCount++;
      const match = url.match(/before=(\d+)/);
      const before = parseInt(match[1], 10);
      const nextBefore = Math.max(0, before - 1);
      return {
        ok: true,
        async json() {
          return {
            has_more: nextBefore > 0,
            next_before: nextBefore,
            messages: [{id: `msg-${before}`, role: 'user', content: `m${before}`, event_index: before}],
          };
        },
      };
    },
    autoTimeout: true,
  });

  renderSessionViewWith(context, new Map([['messages', messages]]), true, {
    oldestOrdinal: 100, eventCount: 101,
  });
  // re-grab elements from context is not needed; messages is the same object.
  // renderSessionViewWith resets scrollTop to 0.

  await context.loadOlderIfNeeded(messages);
  // 1 initial fetch + 5 viewport fills = 6 total.
  assert.equal(fetchCount, 6, `expected 6 fetches (1 initial + 5 fills), got ${fetchCount}`);
});

test('successful page with no more messages removes the sentinel', async () => {
  const messages = makeMessages({scrollTop: 0});
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => ({
      ok: true,
      async json() { return {has_more: false, next_before: 0, messages: []}; },
    }),
  });

  renderSessionViewWith(context, elements, true);

  await context.loadOlderIfNeeded(messages);

  const sentinel = findSentinel(elements);
  assert.ok(!sentinel || sentinel.removed, 'sentinel must be removed when has_more becomes false');
});

function renderStaleBootstrap(context, messages, extra = {}) {
  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'hello', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
    ...extra,
  });
}

test('missing pagination cursor fails loudly instead of silently doing nothing', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => {
      fetchCount++;
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });
  const errors = [];
  context.console.error = (...args) => { errors.push(args); };

  // A backend older than the cursor rename answers with oldest_event_index, so
  // oldest_message_ordinal is absent and the cursor is unusable.
  renderStaleBootstrap(context, messages, {oldest_event_index: 4});
  messages.scrollTop = 0;

  await context.loadOlderIfNeeded(messages);

  assert.equal(fetchCount, 0, 'an unusable cursor must not reach the network');
  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /Failed to load older messages/);
  assert.ok(errors.length >= 1, 'the unusable cursor must be logged');
});

test('first paint fills the viewport when the tail page is shorter than the container', async () => {
  // scrollHeight <= clientHeight: the container cannot scroll, so no scroll event
  // will ever fire and an idle sentinel would wait forever.
  const messages = makeMessages({scrollTop: 0, scrollHeight: 40, clientHeight: 500});
  const fetched = [];
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });

  renderStaleBootstrap(context, messages, {oldest_message_ordinal: 4});

  await new Promise((r) => setTimeout(r, 10));

  assert.equal(fetched.length, 1, 'first paint must fetch one older page');
  assert.match(fetched[0], /before=4/);
  const sentinel = findSentinel(elements);
  assert.ok(!sentinel || sentinel.removed, 'sentinel goes away once has_more is false');
});

test('first paint does not fetch when the tail page already fills the container', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 1000, clientHeight: 500});
  const fetched = [];
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });

  renderStaleBootstrap(context, messages, {oldest_message_ordinal: 4});

  await new Promise((r) => setTimeout(r, 10));

  assert.equal(fetched.length, 0, 'a scrollable container must not auto-fetch on first paint');
  const sentinel = findSentinel(elements);
  assert.equal(sentinel.getAttribute('data-state'), 'idle');
});

// ---------------------------------------------------------------------------
// renderUsageFromData: unknown context + cost cell rendering
// ---------------------------------------------------------------------------

function buildUsageElements() {
  return new Map([
    ['usage-indicator', createElement({className: 'hidden'})],
    ['usage-bar', createElement({className: 'h-full rounded-full bg-blue-500', style: {width: '0%'}})],
    ['usage-compact-line', createElement({className: 'absolute top-0 h-full w-0.5 bg-white hidden', style: {left: '0%'}})],
    ['usage-text', createElement({textContent: ''})],
    ['usage-cost', createElement({textContent: ''})],
  ]);
}

test('renderUsageFromData shows unknown and hides the bar when context_tokens is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: null,
    context_full: null,
    context_compact_at: null,
    total_cost_usd: 0.5,
    model: '',
  });

  assert.equal(elements.get('usage-text').textContent, 'unknown');
  assert.match(elements.get('usage-bar').className, /hidden/);
  assert.equal(elements.get('usage-bar').style.width, '0%');
  // The cost cell still renders.
  assert.equal(elements.get('usage-cost').textContent, '$0.50');
  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), false);
});

test('renderUsageFromData shows unknown when only context_full is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 50000,
    context_full: null,
    context_compact_at: null,
    total_cost_usd: null,
    model: '',
  });

  assert.equal(elements.get('usage-text').textContent, 'unknown');
  assert.match(elements.get('usage-bar').className, /hidden/);
  // N/A cost still renders in the cell.
  assert.equal(elements.get('usage-cost').textContent, 'N/A');
});

test('renderUsageFromData draws the bar when both context fields are present', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 200000,
    context_compact_at: 167000,
    total_cost_usd: 1.25,
    model: 'claude-opus-4-6',
  });

  assert.equal(elements.get('usage-text').textContent, '100k / 200k');
  assert.doesNotMatch(elements.get('usage-bar').className, /hidden/);
  assert.equal(elements.get('usage-bar').style.width, '50.0%');
  assert.equal(elements.get('usage-cost').textContent, '$1.25');
});

test('renderUsageFromData hides the indicator when usage is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData(null);

  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), true);
});
