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
    formatTokens: (n) => `${Math.round(n / 1000)}k`,
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
