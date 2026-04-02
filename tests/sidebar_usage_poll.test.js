const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

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
      if (url.endsWith('/view')) {
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
    clearInterval: (id) => {
      clears.push(id);
    },
    document: {
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: () => [],
      querySelector: () => null,
      createElement: (tagName) => createElement({tagName: String(tagName).toUpperCase()}),
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
    renderSingleMessage: () => '',
    renderWorkersTab: () => {},
    switchTab: () => {},
    marked: {parse: (txt) => txt},
    fixNestedFences: (txt) => txt,
    formatBubbleTime: (txt) => txt,
    shouldAutoScroll: () => true,
    showScrollToBottom: () => {},
    loadedThreads: {clear: () => {}},
    _backlogLoaded: false,
    BACKEND_OPTIONS: overrides.BACKEND_OPTIONS || {},
    alert: (message) => {
      alerts.push(message);
    },
  };

  vm.createContext(context);
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, fetchCalls, fetchRequests, intervals, clears, alerts, elements};
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

test('pollActiveSessionView refreshes usage from the session view endpoint', async () => {
  const {context, fetchCalls} = buildContext();
  let renderedUsage = null;

  context.masterThinking = true;
  context.renderUsageFromData = (usage) => {
    renderedUsage = usage;
  };
  context.ensureActiveSessionViewPolling = () => {};

  await context.pollActiveSessionView();

  assert.deepEqual(fetchCalls, ['/api/sessions/session-a/view']);
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

test('renderSingleMessage preserves clone_start banners for SPA rebuilds', () => {
  const {context} = buildContext();
  context.escapeHtml = (value) => String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');

  const html = context.renderSingleMessage({
    role: 'clone_start',
    content: 'Parent & Session',
    parent_session_id: 'parent/session?tab=chat',
  }, 'session-a');

  assert.match(html, /Cloned from/);
  assert.match(html, /href="\/\?session=parent%2Fsession%3Ftab%3Dchat"/);
  assert.match(html, /Parent &amp; Session/);
});

test('renderSingleMessage passes uploaded_files through for user attachment bubbles', () => {
  const {context} = buildContext();

  const html = context.renderSingleMessage({
    role: 'user',
    content: '',
    uploaded_files: [{filename: 'report.pdf', path: '/tmp/report.pdf'}],
  }, 'session-a');

  assert.match(html, /data-files="1"/);
});

test('pollSessionStatus updates pending trigger indicators from the status endpoint', async () => {
  const {context} = buildContext();
  const spinnerUpdates = [];
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
  context.setSessionSpinner = (sid, visible) => {
    spinnerUpdates.push({sid, visible});
  };
  context.setSessionPendingTriggerIndicator = (sid, status) => {
    pendingUpdates.push({sid, status});
  };

  const anyRunning = await context.pollSessionStatus();

  assert.equal(anyRunning, true);
  assert.deepEqual(spinnerUpdates, [
    {sid: 'session-a', visible: false},
    {sid: 'session-b', visible: true},
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
  const {context} = buildContext();

  const html = context.renderSessionItem({
    id: 'session-a',
    name: 'Wake later',
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
});

test('renderScheduledSessionItem keeps delayed-trigger and cron indicators distinct', () => {
  const {context} = buildContext();

  const html = context.renderScheduledSessionItem({
    id: 'session-a',
    name: 'Wake later',
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
