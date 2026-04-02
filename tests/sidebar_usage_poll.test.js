const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const SIDEBAR_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'sidebar.js'),
  'utf8'
);

function buildContext() {
  const fetchCalls = [];
  const intervals = [];
  const clears = [];

  const context = {
    SESSION_ID: 'session-a',
    THINKING_SINCE: null,
    DRAFT_KEY: null,
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
    fetch: async (url) => {
      fetchCalls.push(url);
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
          };
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
      getElementById: () => null,
      querySelectorAll: () => [],
      querySelector: () => null,
      createElement: () => ({
        className: '',
        innerHTML: '',
        appendChild: () => {},
        remove: () => {},
        prepend: () => {},
        classList: {add: () => {}, remove: () => {}, toggle: () => {}},
      }),
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
    BACKEND_OPTIONS: {},
  };

  vm.createContext(context);
  vm.runInContext(SIDEBAR_JS, context, {filename: 'sidebar.js'});
  return {context, fetchCalls, intervals, clears};
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
