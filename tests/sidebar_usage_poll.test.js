const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const CHAT_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat.js'),
  'utf8'
);
const SIDEBAR_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'sidebar.js'),
  'utf8'
);
const WEBSOCKET_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'websocket.js'),
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
        if (names.has(item)) {
          names.delete(item);
          return false;
        }
        names.add(item);
        return true;
      }
      if (force) names.add(item);
      else names.delete(item);
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
      if (!referenceChild) {
        this.children.push(child);
        return child;
      }
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
    addEventListener() {},
    setAttribute(name, value) {
      this[name] = value;
    },
    getAttribute(name) {
      return this[name];
    },
    querySelectorAll: () => [],
  };

  Object.defineProperty(element, 'firstElementChild', {
    get() {
      return element.children[0] || null;
    },
  });
  Object.defineProperty(element, 'lastChild', {
    get() {
      return element.children[element.children.length - 1] || null;
    },
  });

  Object.defineProperty(element, 'innerHTML', {
    get() {
      return innerHTML;
    },
    set(value) {
      innerHTML = value;
      element.children = [];
      element.options = [];
    },
  });

  return Object.assign(element, overrides);
}

function buildContext(overrides = {}) {
  const fetchCalls = [];
  const fetchRequests = [];
  const intervals = [];
  const timeouts = [];
  const clears = [];
  const alerts = [];
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
    fetch: async (url, opts = {}) => {
      fetchCalls.push(url);
      fetchRequests.push({url, opts});
      if (url.endsWith('/usage')) {
        return {
          ok: true,
          async json() {
            return {
              session: {id: 'session-a', thinking_since: '2026-03-31T20:42:52Z'},
              usage: {
                context_tokens: 49179,
                context_full: 258400,
                context_compact_at: 180000,
                total_cost_usd: 1.25,
              },
              active_backend: context.ACTIVE_BACKEND_ID,
            };
          },
        };
      }
      return {
        ok: true,
        async json() {
          return {id: 'session-b', backend: context.ACTIVE_BACKEND_ID};
        },
      };
    },
    setInterval: (fn, ms) => {
      intervals.push({fn, ms});
      return intervals.length;
    },
    setTimeout: (fn, ms) => {
      timeouts.push({fn, ms});
      return timeouts.length;
    },
    clearInterval: (id) => {
      clears.push(id);
    },
    clearTimeout: (id) => {
      clears.push(id);
    },
    URLSearchParams,
    AbortController,
    document: {
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: overrides.querySelectorAll || (() => []),
      querySelector: overrides.querySelector || (() => null),
      createElement: (tagName) => {
        const el = createElement({tagName: String(tagName).toUpperCase()});
        // Support escapeHtml pattern: set textContent, read innerHTML as escaped
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
    renderSessionView: () => {},
    updateSidebarHighlight: () => {},
    pollSessionStatus: () => Promise.resolve(false),
    pollWorkers: () => {},
    autoResize: () => {},
    startThinking: () => {},
    stopThinking: () => {},
    setSessionSpinner: () => {},
    relativeTime: (txt) => txt,
    updateRelativeTimes: () => {},
    formatTokens: (n) => `${Math.round(n / 1000)}k`,
    formatNextRun: (txt) => txt,
    formatLastRun: (txt) => txt,
    escapeHtml: (v) => v,
    renderUserMessageBubble: (content, isVoice, timestamp, uploadedFiles) =>
      `<div data-content="${content || ''}" data-voice="${isVoice ? '1' : '0'}" data-ts="${timestamp || ''}" data-files="${(uploadedFiles || []).length}"></div>`,
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
    alert: (message) => {
      alerts.push(message);
    },
    confirm: overrides.confirm || (() => true),
  };
  context.window = {
    addEventListener: () => {},
    innerHeight: 800,
  };
  context.CSS = {
    escape: (value) => String(value),
  };

  vm.createContext(context);
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, fetchCalls, fetchRequests, intervals, timeouts, clears, alerts, elements, localStorageData};
}

function buildSessionActionElements() {
  return new Map([
    ['session-action-modal-overlay', createElement({className: 'hidden'})],
    ['session-action-modal-title', createElement()],
    ['session-action-modal-body', createElement()],
    ['session-action-backend', createElement({tagName: 'SELECT'})],
    ['session-action-modal-confirm', createElement()],
  ]);
}

function makeSession(id, name, overrides = {}) {
  return {
    id,
    name,
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

function sessionAnchorOpenTag(html, id) {
  const match = html.match(new RegExp(`<a\\b[^>]*id="session-${id}"[^>]*>`));
  if (!match) throw new Error(`Missing rendered session anchor for ${id}`);
  return match[0];
}

test('pollActiveSessionView refreshes usage from the lazy usage endpoint', async () => {
  const {context, fetchCalls} = buildContext();
  let renderedUsage = null;

  context.masterThinking = true;
  context.renderUsageFromData = (usage) => {
    renderedUsage = usage;
  };
  context.ensureActiveSessionViewPolling = () => {};

  await context.pollActiveSessionView();

  assert.deepEqual(fetchCalls, ['/api/sessions/session-a/usage']);
  assert.deepEqual(renderedUsage, {
    context_tokens: 49179,
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
  });
  assert.equal(context.THINKING_SINCE, '2026-03-31T20:42:52Z');
});

// ---------------------------------------------------------------------------
// renderUsageFromData: compaction line + colour-relative-to-line
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

test('renderUsageFromData draws the compact line at the right percentage', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const line = elements.get('usage-compact-line');
  assert.doesNotMatch(line.className, /hidden/);
  assert.equal(line.style.left, ((180000 / 258400) * 100).toFixed(1) + '%');
  assert.equal(line.title, '180000');
});

test('renderUsageFromData draws no compact line when context_compact_at is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 258400,
    context_compact_at: null,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const line = elements.get('usage-compact-line');
  assert.match(line.className, /hidden/);
  assert.equal(line.style.left, '0%');
  // The bar is still drawn (no line, but context_full present).
  const bar = elements.get('usage-bar');
  assert.doesNotMatch(bar.className, /hidden/);
});

test('renderUsageFromData bar turns red past the compaction line', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 200000,  // past the line at 180000
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  const bar = elements.get('usage-bar');
  assert.match(bar.className, /bg-red-500/);
  assert.doesNotMatch(bar.className, /bg-blue-500/);
  assert.doesNotMatch(bar.className, /bg-yellow-500/);
});

test('renderUsageFromData bar is yellow at 50%-100% of the compaction line', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,  // 100000 / 180000 ~ 55% of the line
    context_full: 258400,
    context_compact_at: 180000,
    total_cost_usd: 1.25,
    model: 'codex-test',
  });

  assert.match(elements.get('usage-bar').className, /bg-yellow-500/);
});

test('a result WebSocket event forces a poll without writing the header directly', async () => {
  const elements = new Map([
    ['usage-indicator', createElement({className: 'hidden'})],
    ['usage-bar', createElement({className: 'h-full rounded-full bg-blue-500', style: {width: '0%'}})],
    ['usage-text', createElement({textContent: 'before'})],
    ['usage-cost', createElement({textContent: 'before'})],
  ]);
  const {context} = buildContext({elements});
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});

  let pollCall = null;
  context.pollActiveSessionView = (opts) => { pollCall = opts; };
  let renderCall = null;
  context.renderUsageFromData = (usage) => { renderCall = usage; };

  context.handleWSEvent({type: 'result', total_cost_usd: 5.0}, 'session-a', 0);

  // The WebSocket handler must not write the header; renderUsageFromData is the
  // only writer and it was not called by the result event.
  assert.equal(elements.get('usage-text').textContent, 'before');
  assert.equal(elements.get('usage-cost').textContent, 'before');
  assert.equal(elements.get('usage-bar').style.width, '0%');
  assert.equal(renderCall, null);
  // The forced poll is what updates the header.
  assert.ok(pollCall && pollCall.force === true, 'result event must force the poll');
});

test('ensureActiveSessionViewPolling only schedules while the active session is running', () => {
  const {context, intervals, clears} = buildContext();

  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 0);

  context.THINKING_SINCE = '2026-03-31T20:42:52Z';
  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 1);
  assert.equal(intervals[0].ms, 3000);

  context.ensureActiveSessionViewPolling();
  assert.equal(intervals.length, 1);

  context.THINKING_SINCE = null;
  context.stopActiveSessionViewPolling();
  assert.deepEqual(clears, [1]);
});

test('renderMessage preserves clone_start banners for SPA rebuilds', () => {
  const {context} = buildContext();
  context.escapeHtml = (value) => String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');

  const html = context.renderMessage({
    role: 'clone_start',
    content: 'Parent & Session',
    parent_session_id: 'parent/session?tab=chat',
  }, 'session-a');

  assert.match(html, /Cloned from/);
  assert.match(html, /href="\/\?session=parent%2Fsession%3Ftab%3Dchat"/);
  assert.match(html, /Parent &amp; Session/);
});

test('renderMessage passes uploaded_files through for user attachment bubbles', () => {
  const {context} = buildContext();

  const html = context.renderMessage({
    role: 'user',
    content: '',
    uploaded_files: [{filename: 'report.pdf', path: '/tmp/report.pdf'}],
  }, 'session-a');

  assert.match(html, /message-attachment/);
  assert.match(html, /report\.pdf/);
});

test('pollSessionStatus updates pending trigger indicators from the status endpoint', async () => {
  const {context} = buildContext();
  const indicatorUpdates = [];
  const pendingUpdates = [];

  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/status');
    return {
      ok: true,
      async json() {
        return {
          'session-a': {
            has_unread: false,
            has_running_tasks: false,
            has_pending_trigger: true,
            pending_trigger_count: 2,
            next_trigger_at: '2026-04-02T06:00:00Z',
          },
          'session-b': {
            has_unread: true,
            has_running_tasks: true,
            has_pending_trigger: false,
            pending_trigger_count: 0,
            next_trigger_at: null,
          },
        };
      },
    };
  };
  context.setSessionIndicator = (sid, state) => {
    indicatorUpdates.push({sid, state});
  };
  context.setSessionPendingTriggerIndicator = (sid, status) => {
    pendingUpdates.push({sid, status});
  };

  const anyRunning = await context.pollSessionStatus();

  assert.equal(anyRunning, true);
  assert.deepEqual(indicatorUpdates, [
    {sid: 'session-a', state: 'idle'},
    {sid: 'session-b', state: 'worker_only'},
  ]);
  assert.deepEqual(pendingUpdates, [
    {
      sid: 'session-a',
      status: {
        has_unread: false,
        has_running_tasks: false,
        has_pending_trigger: true,
        pending_trigger_count: 2,
        next_trigger_at: '2026-04-02T06:00:00Z',
      },
    },
    {
      sid: 'session-b',
      status: {
        has_unread: true,
        has_running_tasks: true,
        has_pending_trigger: false,
        pending_trigger_count: 0,
        next_trigger_at: null,
      },
    },
  ]);
});

test('restoreSidebarFromUrl renders initial all sessions through grouped renderer without fetching sessions', () => {
  const nav = createElement();
  const filterAll = createElement({className: 'filter-pill'});
  const filterStarred = createElement({className: 'filter-pill'});
  const filterArchived = createElement({className: 'filter-pill'});
  const filterScheduled = createElement({className: 'filter-pill'});
  const {context, fetchRequests} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['filter-all', filterAll],
      ['filter-starred', filterStarred],
      ['filter-archived', filterArchived],
      ['filter-scheduled', filterScheduled],
      ['cron-add-btn', createElement()],
    ]),
    querySelectorAll: (selector) => selector === '.filter-pill'
      ? [filterAll, filterStarred, filterArchived, filterScheduled] : [],
  });

  context.INITIAL_SESSIONS = [
    {
      id: 'session-a',
      name: 'Grouped session',
      group: 'Work',
      updated_at: '2026-04-02T04:00:00Z',
      has_unread: true,
      has_running_tasks: false,
      has_pending_trigger: true,
      pending_trigger_count: 1,
      next_trigger_at: '2026-04-02T06:00:00Z',
      starred: true,
      backend: 'claude-opus-4.6',
    },
    {
      id: 'session-b',
      name: 'Ungrouped session',
      group: null,
      updated_at: '2026-04-01T04:00:00Z',
      has_unread: false,
      has_running_tasks: false,
      has_pending_trigger: false,
      pending_trigger_count: 0,
      next_trigger_at: null,
      starred: false,
      backend: 'claude-opus-4.6',
    },
  ];
  context.INITIAL_LOAD_ERRORS = [];
  context.location.search = '';

  context.restoreSidebarFromUrl();

  assert.equal(fetchRequests.length, 0);
  assert.match(nav.innerHTML, /class="session-group group"/);
  assert.match(nav.innerHTML, /Work/);
  assert.match(nav.innerHTML, /\(No group\)/);
  assert.match(nav.innerHTML, /id="pending-trigger-session-a"/);
  assert.match(nav.innerHTML, /id="star-session-a"/);
  assert.equal(filterAll.classList.contains('bg-blue-600/20'), true);
});

test('renderSessionList limits each grouped session section to five visible sessions', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  const workSessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));
  const personalSessions = Array.from({length: 6}, (_, idx) =>
    makeSession(`personal-${idx + 1}`, `Personal ${idx + 1}`, {group: 'Personal'}));

  context.renderSessionList([...workSessions, ...personalSessions], 'all');

  assert.match(nav.innerHTML, /Work/);
  assert.match(nav.innerHTML, /Personal/);
  assert.equal((nav.innerHTML.match(/session-group-limit-toggle/g) || []).length, 2);
  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-5').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'personal-6').includes('session-group-limit-extra hidden'), true);
  assert.match(nav.innerHTML, /renameGroup\(this\.dataset\.groupName\)/);
  assert.match(nav.innerHTML, /deleteGroup\(this\.dataset\.groupName\)/);
});

test('toggleSessionGroupLimit persists and updates only the selected group', () => {
  const workExtra = createElement({
    className: 'session-group-limit-extra hidden',
    dataset: {sessionGroupLimitExtra: 'Work'},
  });
  const personalExtra = createElement({
    className: 'session-group-limit-extra hidden',
    dataset: {sessionGroupLimitExtra: 'Personal'},
  });
  const workToggle = createElement({
    className: 'session-group-limit-toggle',
    dataset: {sgroupLimitToggleKey: 'Work'},
    textContent: 'Show all',
  });
  const personalToggle = createElement({
    className: 'session-group-limit-toggle',
    dataset: {sgroupLimitToggleKey: 'Personal'},
    textContent: 'Show all',
  });
  const {context, localStorageData} = buildContext({
    localStorageItems: {
      'session-group-list-expanded': JSON.stringify({Personal: false}),
    },
    querySelectorAll: (selector) => {
      if (selector === '.session-group-limit-extra') return [workExtra, personalExtra];
      if (selector === '.session-group-limit-toggle') return [workToggle, personalToggle];
      return [];
    },
  });

  context.toggleSessionGroupLimit('Work');

  assert.deepEqual(JSON.parse(localStorageData.get('session-group-list-expanded')), {
    Personal: false,
    Work: true,
  });
  assert.equal(workExtra.classList.contains('hidden'), false);
  assert.equal(personalExtra.classList.contains('hidden'), true);
  assert.equal(workToggle.textContent, 'Show less');
  assert.equal(workToggle.getAttribute('aria-expanded'), 'true');
  assert.equal(personalToggle.textContent, 'Show all');

  context.toggleSessionGroupLimit('Work');

  assert.deepEqual(JSON.parse(localStorageData.get('session-group-list-expanded')), {
    Personal: false,
    Work: false,
  });
  assert.equal(workExtra.classList.contains('hidden'), true);
  assert.equal(workToggle.textContent, 'Show all');
  assert.equal(workToggle.getAttribute('aria-expanded'), 'false');
});

test('renderSessionList keeps active grouped session visible outside the first five', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  context.SESSION_ID = 'work-7';
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`work-${idx + 1}`, `Work ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'all');

  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-6').includes('session-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('session-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'work-7').includes('bg-blue-600/20 text-blue-300'), true);
});

test('renderSessionList leaves search results flat and untrimmed', () => {
  const nav = createElement();
  const {context} = buildContext({
    elements: new Map([['session-list', nav]]),
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`search-${idx + 1}`, `Search ${idx + 1}`, {group: 'Work'}));

  context.renderSessionList(sessions, 'search');

  assert.doesNotMatch(nav.innerHTML, /class="session-group group"/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-toggle/);
  assert.doesNotMatch(nav.innerHTML, /session-group-limit-extra/);
  assert.match(nav.innerHTML, /id="session-search-7"/);
});

test('renderGroupedScheduledList limits project groups to five visible sessions', () => {
  const nav = createElement();
  const {context} = buildContext({
    localStorageItems: {
      'cron-group-collapsed': JSON.stringify({Nightly: false}),
    },
    elements: new Map([['session-list', nav]]),
  });
  const sessions = Array.from({length: 7}, (_, idx) =>
    makeSession(`scheduled-${idx + 1}`, `Scheduled ${idx + 1}`, {
      schedule_project: 'Nightly',
      scheduled_task: 'nightly',
      schedule_enabled: true,
      schedule_cron: '0 2 * * *',
      schedule_timezone: 'UTC',
    }));

  context.renderSessionList(sessions, 'scheduled');

  assert.match(nav.innerHTML, /Nightly/);
  assert.match(nav.innerHTML, /cron-group-limit-toggle/);
  assert.match(nav.innerHTML, />Show all<\/button>/);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-5').includes('cron-group-limit-extra'), false);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-6').includes('cron-group-limit-extra hidden'), true);
  assert.equal(sessionAnchorOpenTag(nav.innerHTML, 'scheduled-7').includes('cron-group-limit-extra hidden'), true);
});

test('switchSession preserves worker icon until authoritative status returns', async () => {
  const workerIcon = createElement();
  const tabWorkers = createElement();
  const messages = createElement();
  const input = createElement();
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-opus-4.6': 'claude-code'},
    elements: new Map([
      ['msg-input', input],
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
      ['input-model-badge', createElement()],
      ['messages', messages],
      ['tab-workers', tabWorkers],
      ['worker-indicator-session-b', workerIcon],
      ['spinner-session-b', createElement({className: 'hidden'})],
      ['unread-session-b', createElement({className: 'hidden'})],
    ]),
  });
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-b/bootstrap');
    return {
      ok: true,
      async json() {
        return {
          session: makeSession('session-b', 'Session B'),
          messages: [],
          pending_draft: null,
          event_count: 0,
          active_backend: 'claude-opus-4.6',
          active_backend_type: 'claude-code',
          has_more: false,
        };
      },
    };
  };
  context.pollSessionStatus = () => new Promise(() => {});

  await context.switchSession('session-b');

  assert.equal(workerIcon.classList.contains('hidden'), false);
  assert.match(tabWorkers.innerHTML, /Loading worker threads/);
  assert.equal(context.location.href, '');
});

test('missing bootstrap worker data and empty worker tab do not imply idle', () => {
  const workerIcon = createElement();
  const tabWorkers = createElement();
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-opus-4.6': 'claude-code'},
    elements: new Map([
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
      ['input-model-badge', createElement()],
      ['messages', createElement()],
      ['tab-workers', tabWorkers],
      ['worker-indicator-session-a', workerIcon],
      ['spinner-session-a', createElement({className: 'hidden'})],
      ['unread-session-a', createElement({className: 'hidden'})],
    ]),
  });
  let statusPolls = 0;
  context.pollSessionStatus = () => {
    statusPolls += 1;
    return Promise.resolve(false);
  };

  context.renderSessionView({
    session: makeSession('session-a', 'Session A'),
    messages: [],
    pending_draft: null,
    event_count: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: 'claude-code',
    has_more: false,
  });
  context.updateSpinner();

  assert.match(tabWorkers.innerHTML, /Loading worker threads/);
  assert.equal(workerIcon.classList.contains('hidden'), false);
  assert.equal(statusPolls, 1);
});

test('loadOlderIfNeeded post-processes prepended messages through the shared helper', async () => {
  const messages = createElement({scrollTop: 0});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const {context} = buildContext({
    elements: new Map([
      ['messages', messages],
    ]),
  });
  const postProcessedHtml = [];
  context.postProcessRenderedMessages = (root) => {
    postProcessedHtml.push(root.innerHTML);
  };
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-a/events?before=4&limit=40');
    return {
      ok: true,
      async json() {
        return {
          has_more: false,
          next_before: 2,
          messages: [{
            role: 'assistant',
            content: 'older $x$',
            event_index: 3,
            timestamp: '2026-04-02T04:00:00Z',
          }],
        };
      },
    };
  };

  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'newer', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
  });
  postProcessedHtml.length = 0;
  messages.scrollTop = 0;

  await context.loadOlderIfNeeded(messages);

  assert.equal(postProcessedHtml.length, 1);
  assert.match(postProcessedHtml[0], /older \$x\$/);
});

test('loadOlderIfNeeded skips messages whose rendered id is already in the DOM', async () => {
  const messages = createElement({scrollTop: 0});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const {context} = buildContext({
    elements: new Map([
      ['messages', messages],
    ]),
  });
  let renderedMessages = null;
  context.isRenderedMessage = (msg) => msg.id === 'assistant-event-1';
  context.renderMessagesToDetachedContainer = (msgs) => {
    renderedMessages = msgs;
    return createElement();
  };
  context.fetch = async (url) => {
    assert.equal(url, '/api/sessions/session-a/events?before=4&limit=40');
    return {
      ok: true,
      async json() {
        return {
          has_more: false,
          next_before: 2,
          messages: [
            {
              id: 'assistant-event-1',
              role: 'assistant',
              content: 'already visible',
              event_index: 3,
            },
            {
              id: 'user-event-0',
              role: 'user',
              content: 'older ask',
              event_index: 1,
            },
          ],
        };
      },
    };
  };

  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{id: 'assistant-event-1', role: 'assistant', content: 'already visible', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
  });
  messages.scrollTop = 0;

  await context.loadOlderIfNeeded(messages);

  assert.deepEqual(renderedMessages, [{
    id: 'user-event-0',
    role: 'user',
    content: 'older ask',
    event_index: 1,
  }]);
});

test('renderMessage stamps committed messages with stable message ids', () => {
  const {context} = buildContext();

  const html = context.renderMessage({
    id: 'assistant-event-1',
    role: 'assistant',
    content: 'hello',
    event_index: 5,
  }, 'session-a');

  assert.match(html, /data-message-id="assistant-event-1"/);
});

test('renderSessionItem shows separate delayed-trigger and scheduled indicators', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  const html = context.renderSessionItem({
    id: 'session-a',
    name: 'Wake later',
    backend: 'claude-tui',
    updated_at: '2026-04-02T04:00:00Z',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: true,
    pending_trigger_count: 1,
    next_trigger_at: '2026-04-02T06:00:00Z',
    scheduled_task: 'nightly',
    schedule_enabled: true,
    starred: false,
  }, 'search');

  assert.match(html, /id="pending-trigger-session-a"/);
  assert.match(html, /1 pending delayed trigger/);
  assert.match(html, /Scheduled: nightly/);
  assert.match(html, /<\/svg>\s*<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0"/);
  assert.match(html, /<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="session-a" title="Claude stopped"><\/span>\s*<span class="flex-1 min-w-0">/);
  assert.match(html, /<span class="truncate block session-name">Wake later<\/span>/);
  assert.doesNotMatch(html, /session-name">[^<]*<span class="tui-status-dot/);
});

test('renderScheduledSessionItem keeps delayed-trigger and cron indicators distinct', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  const html = context.renderScheduledSessionItem({
    id: 'session-a',
    name: 'Wake later',
    backend: 'claude-tui',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: true,
    pending_trigger_count: 2,
    next_trigger_at: '2026-04-02T06:00:00Z',
    scheduled_task: 'nightly',
    schedule_enabled: true,
    schedule_cron: '0 2 * * *',
    schedule_timezone: 'UTC',
    starred: false,
  });

  assert.match(html, /id="pending-trigger-session-a"/);
  assert.match(html, /2 pending delayed triggers/);
  assert.match(html, /Scheduled: nightly/);
  assert.match(html, /<\/svg>\s*<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0"/);
  assert.match(html, /<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="session-a" title="Claude stopped"><\/span>\s*<span class="flex-1 min-w-0">/);
  assert.match(html, /<span class="truncate block session-name">Wake later<\/span>/);
  assert.doesNotMatch(html, /session-name">[^<]*<span class="tui-status-dot/);
});

test('renderSessionItem omits tui dot for non-tui backend', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'codex-o3': 'codex-cli'},
  });

  const html = context.renderSessionItem({
    id: 'session-a',
    name: 'SDK session',
    backend: 'codex-o3',
    updated_at: '2026-04-02T04:00:00Z',
    has_running_tasks: false,
    has_unread: false,
    has_pending_trigger: false,
    pending_trigger_count: 0,
    next_trigger_at: null,
    scheduled_task: '',
    starred: false,
  }, 'search');

  assert.doesNotMatch(html, /tui-status-dot/);
});

test('renderTuiStatusDot reflects stopped, idle, and busy states', () => {
  const {context} = buildContext({
    BACKEND_TYPES: {'claude-tui': 'tui-cli'},
  });

  context.TuiStatusMap = {
    stopped: {running: false, busy: false},
    idle: {running: true, busy: false},
    busy: {running: true, busy: true},
  };

  assert.equal(
    context.renderTuiStatusDot({id: 'stopped', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0" data-session-id="stopped" title="Claude stopped"></span>'
  );
  assert.equal(
    context.renderTuiStatusDot({id: 'idle', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0 running" data-session-id="idle" title="Claude idle"></span>'
  );
  assert.equal(
    context.renderTuiStatusDot({id: 'busy', backend: 'claude-tui'}),
    '<span class="tui-status-dot w-2 h-2 rounded-full flex-shrink-0 running busy" data-session-id="busy" title="Claude busy"></span>'
  );
});

test('refreshTuiDots updates dot classes, titles, and Stop button visibility', () => {
  const busyDot = createElement({className: 'tui-status-dot', dataset: {sessionId: 'busy'}});
  const idleDot = createElement({className: 'tui-status-dot busy', dataset: {sessionId: 'idle'}});
  const stoppedDot = createElement({className: 'tui-status-dot running busy', dataset: {sessionId: 'stopped'}});
  const stopBtn = createElement();
  const {context} = buildContext({
    elements: new Map([['stop-tui-btn', stopBtn]]),
    querySelectorAll: (selector) => {
      assert.equal(selector, '.tui-status-dot[data-session-id]');
      return [busyDot, idleDot, stoppedDot];
    },
  });

  context.ACTIVE_BACKEND_TYPE = 'tui-cli';
  context.SESSION_ID = 'stopped';
  context.TuiStatusMap = {
    busy: {running: true, busy: true},
    idle: {running: true, busy: false},
    stopped: {running: false, busy: false},
  };

  context.refreshTuiDots();

  assert.equal(busyDot.classList.contains('running'), true);
  assert.equal(busyDot.classList.contains('busy'), true);
  assert.equal(busyDot.title, 'Claude busy');
  assert.equal(idleDot.classList.contains('running'), true);
  assert.equal(idleDot.classList.contains('busy'), false);
  assert.equal(idleDot.title, 'Claude idle');
  assert.equal(stoppedDot.classList.contains('running'), false);
  assert.equal(stoppedDot.classList.contains('busy'), false);
  assert.equal(stoppedDot.title, 'Claude stopped');
  assert.equal(stopBtn.classList.contains('hidden'), true);

  context.SESSION_ID = 'idle';
  context.refreshTuiDots();
  assert.equal(stopBtn.classList.contains('hidden'), false);
});

test('createSession switches open chat through SPA state without full reload', async () => {
  const input = createElement({value: 'old draft'});
  const backendSelect = createElement({tagName: 'SELECT', value: 'codex-o3'});
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_TYPES: {'codex-o3': 'codex'},
    elements: new Map([
      ['msg-input', input],
      ['new-session-backend', backendSelect],
    ]),
  });
  let rendered = null;
  let pushedUrl = null;
  let connected = false;

  context.renderSessionView = (data) => { rendered = data; };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };
  context.connectWS = () => { connected = true; };

  await context.createSession();

  assert.equal(fetchRequests[0].url, '/api/sessions/');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {backend: 'codex-o3'});
  assert.equal(context.location.href, '');
  assert.equal(context.SESSION_ID, 'session-b');
  assert.equal(context.DRAFT_KEY, 'charliebot-draft-session-b');
  assert.equal(context.eventCursor, 0);
  assert.equal(pushedUrl, '/?session=session-b');
  assert.equal(connected, true);
  assert.equal(input.value, '');
  assert.deepEqual(JSON.parse(JSON.stringify(rendered)), {
    session: {id: 'session-b', backend: 'codex-o3'},
    messages: [],
    pending_draft: null,
    event_count: 0,
    oldest_message_ordinal: 0,
    active_backend: 'codex-o3',
    active_backend_type: 'codex',
    has_more: false,
  });
});

test('archiveSession refreshes backend sidebar and switches active session without full reload', async () => {
  const nav = createElement();
  const input = createElement();
  const {context} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['msg-input', input],
      ['filter-all', createElement({className: 'filter-pill'})],
      ['filter-starred', createElement({className: 'filter-pill'})],
      ['filter-archived', createElement({className: 'filter-pill'})],
      ['filter-scheduled', createElement({className: 'filter-pill'})],
      ['cron-add-btn', createElement()],
    ]),
  });
  const requests = [];
  let rendered = null;
  let pushedUrl = null;
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    if (url === '/api/sessions/session-a') {
      assert.equal(opts.method, 'DELETE');
      return {ok: true, async json() { return {}; }};
    }
    if (url === '/api/sessions/') {
      return {
        ok: true,
        async json() {
          return [makeSession('session-b', 'Backend Session B')];
        },
      };
    }
    if (url === '/api/sessions/session-b/bootstrap') {
      return {
        ok: true,
        async json() {
          return {
            session: makeSession('session-b', 'Backend Session B'),
            messages: [],
            pending_draft: null,
            event_count: 0,
            active_backend: 'claude-opus-4.6',
            active_backend_type: '',
            has_more: false,
          };
        },
      };
    }
    throw new Error('unexpected fetch ' + url);
  };
  context.renderSessionView = (data) => { rendered = data; };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };
  context.pollSessionStatus = () => Promise.resolve(false);

  await context.archiveSession('session-a');

  assert.deepEqual(requests.map((req) => req.url), [
    '/api/sessions/session-a',
    '/api/sessions/',
    '/api/sessions/session-b/bootstrap',
  ]);
  assert.equal(context.location.href, '');
  assert.equal(pushedUrl, '/?session=session-b');
  assert.equal(context.SESSION_ID, 'session-b');
  assert.match(nav.innerHTML, /Backend Session B/);
  assert.equal(rendered.session.id, 'session-b');
});

test('deleteSessionPermanently renders welcome state when backend returns no sessions', async () => {
  const nav = createElement();
  const main = createElement({tagName: 'MAIN'});
  const {context} = buildContext({
    elements: new Map([
      ['session-list', nav],
      ['filter-all', createElement({className: 'filter-pill'})],
      ['filter-starred', createElement({className: 'filter-pill'})],
      ['filter-archived', createElement({className: 'filter-pill'})],
      ['filter-scheduled', createElement({className: 'filter-pill'})],
      ['cron-add-btn', createElement()],
    ]),
    querySelector: (selector) => {
      if (selector === 'main') return main;
      return null;
    },
  });
  const requests = [];
  let pushedUrl = null;
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    if (url === '/api/sessions/session-a/permanent') {
      assert.equal(opts.method, 'DELETE');
      return {ok: true, async json() { return {}; }};
    }
    if (url === '/api/sessions/') {
      return {ok: true, async json() { return []; }};
    }
    throw new Error('unexpected fetch ' + url);
  };
  context.history.pushState = (_state, _title, url) => { pushedUrl = url; };

  await context.deleteSessionPermanently('session-a');

  assert.deepEqual(requests.map((req) => req.url), [
    '/api/sessions/session-a/permanent',
    '/api/sessions/',
  ]);
  assert.equal(context.location.href, '');
  assert.equal(pushedUrl, '/');
  assert.equal(context.SESSION_ID, null);
  assert.match(nav.innerHTML, /No sessions yet/);
  assert.match(main.innerHTML, /Welcome to CharlieBot/);
});

test('saveCronTask sends backend selector value and null inherit value', async () => {
  const requests = [];
  const cronModal = createElement({className: 'hidden'});
  const elements = new Map([
    ['cron-modal-title', createElement()],
    ['cron-name', createElement()],
    ['cron-expr', createElement()],
    ['cron-prompt', createElement()],
    ['cron-repo', createElement()],
    ['cron-backend', createElement({tagName: 'SELECT'})],
    ['cron-project', createElement()],
    ['cron-timezone', createElement()],
    ['cron-enabled', createElement({checked: true})],
    ['cron-delete-btn', createElement({className: 'hidden'})],
    ['cron-modal', cronModal],
  ]);
  const {context} = buildContext({elements});
  context.fetch = async (url, opts = {}) => {
    requests.push({url, opts});
    return {ok: true, async json() { return {}; }, async text() { return ''; }};
  };
  context.switchSidebarFilter = () => {};

  context.openCronAdder();
  elements.get('cron-name').value = 'nightly';
  elements.get('cron-expr').value = '0 2 * * *';
  elements.get('cron-prompt').value = 'run nightly';
  elements.get('cron-backend').value = '';

  await context.saveCronTask();

  let body = JSON.parse(requests[0].opts.body);
  assert.equal(requests[0].url, '/api/cron/tasks');
  assert.equal(body.backend, null);

  context.openCronAdder();
  elements.get('cron-name').value = 'nightly-codex';
  elements.get('cron-expr').value = '0 3 * * *';
  elements.get('cron-prompt').value = 'run codex nightly';
  elements.get('cron-backend').value = 'codex-o3';

  await context.saveCronTask();

  body = JSON.parse(requests[1].opts.body);
  assert.equal(requests[1].url, '/api/cron/tasks');
  assert.equal(body.backend, 'codex-o3');
});

test('startTuiStatusPolling polls TUI status every three seconds', () => {
  const {context, intervals} = buildContext();

  context.startTuiStatusPolling();

  assert.equal(intervals.length, 1);
  assert.equal(intervals[0].ms, 3000);
});

test('switchSession reconnects when clicking the active stopped TUI session', async () => {
  const {context, timeouts} = buildContext();
  let disconnected = false;
  let connected = false;
  context.TuiStatusMap = {'session-a': {running: false, busy: false}};
  context.disconnectWS = () => { disconnected = true; };
  context.connectWS = () => { connected = true; };

  await context.switchSession('session-a');

  assert.equal(disconnected, true);
  assert.equal(connected, true);
  assert.equal(timeouts.length, 1);
  assert.equal(timeouts[0].ms, 1500);
});

test('stopActiveTui writes stopped object status before refreshing dots', async () => {
  const {context} = buildContext();
  let stoppedUrl = null;
  let refreshed = false;
  let bannerShown = false;
  context.fetch = async (url, opts = {}) => {
    stoppedUrl = url;
    assert.equal(opts.method, 'POST');
    return {
      ok: true,
      async json() {
        return {stopped: true};
      },
    };
  };
  context.refreshTuiDots = () => { refreshed = true; };
  context.TuiSession = {showStoppedBanner: () => { bannerShown = true; }};

  await context.stopActiveTui();

  assert.equal(stoppedUrl, '/api/sessions/session-a/tui/stop');
  const stoppedStatus = vm.runInContext('globalThis.TuiStatusMap["session-a"]', context);
  assert.equal(stoppedStatus.running, false);
  assert.equal(stoppedStatus.busy, false);
  assert.equal(refreshed, true);
  assert.equal(bannerShown, true);
});

test('updateSidebarSessionName leaves sibling tui dot untouched', () => {
  const nameEl = {textContent: 'Old name'};
  const tuiDot = {className: 'tui-status-dot w-2 h-2 rounded-full flex-shrink-0'};
  const link = {
    children: [tuiDot, nameEl],
    querySelector(selector) {
      assert.equal(selector, '.session-name');
      return nameEl;
    },
  };
  const {context} = buildContext({
    elements: new Map([['session-session-a', link]]),
  });

  context.updateSidebarSessionName('session-a', 'New name');

  assert.equal(nameEl.textContent, 'New name');
  assert.deepEqual(link.children, [tuiDot, nameEl]);
});

test('forkSession opens the reusable modal with the active backend selected by default', () => {
  const elements = buildSessionActionElements();
  const {context} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a');

  assert.equal(elements.get('session-action-modal-overlay').classList.contains('hidden'), false);
  assert.equal(elements.get('session-action-backend').value, 'codex-o3');
  assert.deepEqual(
    elements.get('session-action-backend').options.map((option) => option.value),
    ['claude-opus-4.6', 'codex-o3']
  );
  assert.equal(elements.get('session-action-modal-confirm').textContent, 'Clone');
});

test('closeSessionActionModal hides the modal without sending a request', () => {
  const elements = buildSessionActionElements();
  const {context, fetchCalls} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {'codex-o3': 'Codex'},
    elements,
  });

  context.forkSession('session-a');
  context.closeSessionActionModal();

  assert.equal(elements.get('session-action-modal-overlay').classList.contains('hidden'), true);
  assert.deepEqual(fetchCalls, []);
});

test('submitSessionActionModal sends backend and null event_index for header clone', async () => {
  const elements = buildSessionActionElements();
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'codex-o3',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a');
  await context.submitSessionActionModal();

  assert.equal(fetchRequests[0].url, '/api/sessions/session-a/fork');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {
    event_index: null,
    backend: 'codex-o3',
  });
});

test('submitSessionActionModal sends backend and event_index for message-level clone and Elon-e', async () => {
  const elements = buildSessionActionElements();
  const {context, fetchRequests} = buildContext({
    ACTIVE_BACKEND_ID: 'claude-opus-4.6',
    BACKEND_OPTIONS: {
      'claude-opus-4.6': 'Opus',
      'codex-o3': 'Codex',
    },
    elements,
  });

  context.forkSession('session-a', 12);
  elements.get('session-action-backend').value = 'codex-o3';
  await context.submitSessionActionModal();

  context.eloneSession('session-a', 18);
  await context.submitSessionActionModal();

  assert.equal(fetchRequests[0].url, '/api/sessions/session-a/fork');
  assert.deepEqual(JSON.parse(fetchRequests[0].opts.body), {
    event_index: 12,
    backend: 'codex-o3',
  });
  assert.equal(fetchRequests[1].url, '/api/sessions/session-a/elone');
  assert.deepEqual(JSON.parse(fetchRequests[1].opts.body), {
    event_index: 18,
    backend: 'claude-opus-4.6',
  });
});
