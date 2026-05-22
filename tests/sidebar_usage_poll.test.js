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
    classList: createClassList(overrides.className || ''),
    appendChild(child) {
      this.children.push(child);
      if (child && child.tagName === 'OPTION') {
        this.options.push(child);
        if (child.selected || !this.value) this.value = child.value;
      }
      return child;
    },
    prepend(child) {
      this.children.unshift(child);
      return child;
    },
    remove() {
      this.removed = true;
    },
    focus() {},
    setAttribute(name, value) {
      this[name] = value;
    },
    getAttribute(name) {
      return this[name];
    },
  };

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

  const context = {
    SESSION_ID: 'session-a',
    THINKING_SINCE: null,
    DRAFT_KEY: null,
    ACTIVE_BACKEND_ID: overrides.ACTIVE_BACKEND_ID || 'claude-opus-4.6',
    masterThinking: false,
    usageTotalCost: 0,
    switching: false,
    reconnectTimer: null,
    workersPollInterval: null,
    streamBuf: '',
    streamTs: null,
    catchupDone: false,
    pendingUserMsg: false,
    uploadedFiles: [],
    localStorage: {
      getItem: () => null,
      setItem: () => {},
      removeItem: () => {},
    },
    location: {href: '', protocol: 'http:', host: 'localhost:8000'},
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
                context_limit: 258400,
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
  return {context, fetchCalls, fetchRequests, intervals, timeouts, clears, alerts, elements};
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
    context_limit: 258400,
    total_cost_usd: 1.25,
  });
  assert.equal(context.usageTotalCost, 1.25);
  assert.equal(context.THINKING_SINCE, '2026-03-31T20:42:52Z');
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
    active_backend: 'codex-o3',
    active_backend_type: 'codex',
    has_more: false,
  });
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
