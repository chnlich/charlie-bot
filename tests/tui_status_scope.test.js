const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const BACKEND_TYPES = {
  'claude-opus-4.6': 'cc-claude',
  'claude-tui': 'tui-cli',
  'opencode-glm': 'opencode',
};

function loadStatusContext(overrides = {}) {
  const context = {
    SESSION_ID: overrides.SESSION_ID || 'session-a',
    ACTIVE_BACKEND_TYPE: overrides.ACTIVE_BACKEND_TYPE || 'cc-claude',
    BACKEND_TYPES,
    console: { error: () => {} },
    escapeHtmlAttr: (v) => String(v),
    fetch: overrides.fetch || (() => {
      throw new Error('unexpected fetch');
    }),
    document: {
      getElementById: () => null,
      querySelectorAll: overrides.querySelectorAll || (() => []),
    },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/status.js'), context, { filename: 'sidebar/status.js' });
  return context;
}

function sessionAnchors(ids) {
  return ids.map((id) => ({ id: 'session-' + id, tagName: 'A' }));
}

function recordingFetch(requested, payload = {}) {
  return async (url) => {
    requested.push(url);
    return { ok: true, json: async () => payload };
  };
}

test('fetchTuiStatus issues no request when no rendered row is tui-cli', async () => {
  const requested = [];
  const context = loadStatusContext({
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-a', 'session-b']) : [],
    fetch: recordingFetch(requested),
  });
  context.renderTuiStatusDot({ id: 'session-a', backend: 'claude-opus-4.6' });
  context.renderTuiStatusDot({ id: 'session-b', backend: 'opencode-glm' });
  context.TuiStatusMap = { 'session-a': { running: true, busy: true } };

  await context.fetchTuiStatus();

  assert.deepEqual(requested, []);
  assert.deepEqual(context.TuiStatusMap, {});
});

test('fetchTuiStatus requests only the rows rendered as tui-cli', async () => {
  const requested = [];
  const context = loadStatusContext({
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-a', 'session-b', 'session-c']) : [],
    fetch: recordingFetch(requested, { 'session-b': { running: true, busy: false } }),
  });
  context.renderTuiStatusDot({ id: 'session-a', backend: 'claude-opus-4.6' });
  context.renderTuiStatusDot({ id: 'session-b', backend: 'claude-tui' });
  context.renderTuiStatusDot({ id: 'session-c', backend: 'opencode-glm' });

  await context.fetchTuiStatus();

  assert.deepEqual(requested, ['/api/sessions/tui/status?ids=session-b']);
  assert.deepEqual(context.TuiStatusMap, { 'session-b': { running: true, busy: false } });
});

test('fetchTuiStatus keeps the active tui session polled when its row is not rendered', async () => {
  const requested = [];
  const context = loadStatusContext({
    SESSION_ID: 'session-active',
    ACTIVE_BACKEND_TYPE: 'tui-cli',
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-b']) : [],
    fetch: recordingFetch(requested),
  });
  context.renderTuiStatusDot({ id: 'session-b', backend: 'claude-tui' });

  await context.fetchTuiStatus();

  assert.deepEqual(requested, ['/api/sessions/tui/status?ids=session-active,session-b']);
});

test('fetchTuiStatus drops the active session alone when its backend type is not tui-cli', async () => {
  const requested = [];
  const context = loadStatusContext({
    SESSION_ID: 'session-a',
    ACTIVE_BACKEND_TYPE: 'opencode',
    fetch: recordingFetch(requested),
  });

  await context.fetchTuiStatus();

  assert.deepEqual(requested, []);
});

test('a row re-rendered with a new backend leaves the poll scope at the next render', async () => {
  const requested = [];
  const context = loadStatusContext({
    querySelectorAll: (selector) =>
      selector === 'a[id^="session-"]' ? sessionAnchors(['session-a', 'session-b']) : [],
    fetch: recordingFetch(requested),
  });
  context.renderTuiStatusDot({ id: 'session-a', backend: 'claude-tui' });
  context.renderTuiStatusDot({ id: 'session-b', backend: 'claude-opus-4.6' });
  await context.fetchTuiStatus();
  assert.deepEqual(requested, ['/api/sessions/tui/status?ids=session-a']);

  // Backend rotation re-renders the row: session-a runs opencode now.
  context.renderTuiStatusDot({ id: 'session-a', backend: 'opencode-glm' });
  requested.length = 0;

  await context.fetchTuiStatus();

  assert.deepEqual(requested, []);
});

test('a failed tui poll keeps the last status map', async () => {
  const context = loadStatusContext({
    fetch: async () => {
      throw new Error('network down');
    },
  });
  context.renderTuiStatusDot({ id: 'session-a', backend: 'claude-tui' });
  context.TuiStatusMap = { 'session-a': { running: true, busy: false } };

  await context.fetchTuiStatus();

  assert.deepEqual(context.TuiStatusMap, { 'session-a': { running: true, busy: false } });
});
