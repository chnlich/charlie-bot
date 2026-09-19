// ---------------------------------------------------------------------------
// The two creation entry points (session-view.js): New Session and a row's
// hover "+" both POST the task creation body (client request key, parent id,
// manager profile, empty goal, the sidebar's backend choice). A child create
// opens its parent row before the list repaints; the welcome screen keeps its
// full-page load. Harness follows the switch tests (session_context_stub).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {baseSessionContext, bootstrapPayload, createChatSidebarContext, installSessionDocumentLookups,
  stubPageTimers, SWITCH_TELEMETRY_URL} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');

const CREATED = (n) => ({ok: true, status: 200, json: async () => (
  {id: 'task-new-' + n, name: 'Session 7', profile: 'manager', schema_version: 2, backend: 'claude-opus-4.6'})});

function buildHarness(overrides = {}) {
  const input = createElement({id: 'msg-input', value: ''});
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 100;
  messages.scrollTop = 0;
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', createElement({id: 'header-session-name'})],
    ['backend-badge', createElement()],
    ['input-model-badge', createElement()],
    ['msg-input', input],
  ]);
  if (overrides.backendSelectValue !== undefined) {
    elements.set('new-session-backend',
      createElement({tagName: 'SELECT', id: 'new-session-backend', value: overrides.backendSelectValue}));
  }
  const h = {posts: [], renders: [], expanded: [], consoleErrors: [], pushes: []};
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = overrides.sessionId !== undefined ? overrides.sessionId : 'session-a';
  context.DRAFT_KEY = context.SESSION_ID ? 'charliebot-draft-' + context.SESSION_ID : null;
  context.currentFilter = 'all';
  context.crypto = {randomUUID: () => 'req-' + (h.posts.length + 1)};
  context.console.error = (...args) => h.consoleErrors.push(args);
  context.history = {pushState: (state, title, url) => h.pushes.push(url)};
  installSessionDocumentLookups(context, elements, messages, []);
  const createResponse = overrides.createResponse || CREATED;
  context.fetch = async (url, opts = {}) => {
    if (url === '/api/sessions/' && opts.method === 'POST') {
      h.posts.push({url, body: JSON.parse(opts.body)});
      return createResponse(h.posts.length);
    }
    if (url === SWITCH_TELEMETRY_URL) return {ok: true, status: 200, json: async () => ({ok: true})};
    const boot = url.match(/\/api\/sessions\/([^/]+)\/bootstrap$/);
    if (boot) return {ok: true, status: 200, json: async () => bootstrapPayload(boot[1], 0, false)};
    return {ok: true, status: 200, json: async () => ({})};
  };
  stubPageTimers(context);
  createChatSidebarContext(context);
  // Page modules outside the chat/ and sidebar/ sets that the create flow
  // reaches at call time (websocket.js, utils.js, app.js wiring).
  for (const name of ['connectWS', 'disconnectWS', 'cancelReconnect', 'hideStreaming', 'autoResize',
    'scheduleLazySessionDataLoad', 'saveDraftNow', 'setSidebarFilterPill', 'switchSidebarFilter']) {
    if (typeof context[name] !== 'function') context[name] = () => {};
  }
  context.renderSessionView = (data) => h.renders.push(data);
  context.Sidebar.expandTreeNode = (id) => h.expanded.push(id);
  h.context = context;
  return h;
}

async function settle(rounds = 10) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}

test('New Session posts the root task body with the dropdown backend and opens the node', async () => {
  const h = buildHarness({backendSelectValue: 'codex-o3'});

  h.context.createSession();
  await settle();

  assert.equal(h.posts.length, 1);
  assert.deepEqual(h.posts[0].body, {
    request_id: 'req-1', task_parent_id: null, profile: 'manager', task: {goal: ''}, backend: 'codex-o3',
  });
  assert.equal(h.context.SESSION_ID, 'task-new-1', 'the new node is the active session');
  assert.equal(h.renders.length, 1);
  assert.deepEqual(h.pushes, ['/?session=task-new-1']);
  assert.deepEqual(h.expanded, [], 'a root has no parent to open');
  assert.deepEqual(h.consoleErrors, []);
});

test('the hover "+" posts the same body under its parent and opens the parent row', async () => {
  const h = buildHarness({});

  h.context.createChildSession('parent-1');
  await settle();

  assert.equal(h.posts.length, 1);
  const body = h.posts[0].body;
  assert.equal(body.task_parent_id, 'parent-1');
  assert.equal(body.profile, 'manager');
  assert.deepEqual(body.task, {goal: ''});
  assert.equal(body.request_id, 'req-1');
  assert.equal('backend' in body, false, 'no dropdown, no backend override');
  assert.deepEqual(h.expanded, ['parent-1']);
  assert.equal(h.context.SESSION_ID, 'task-new-1');
});

test('from the welcome screen a create lands through a full page load', async () => {
  const h = buildHarness({sessionId: null});

  h.context.createChildSession('parent-1');
  await settle();

  assert.equal(h.posts.length, 1);
  assert.equal(h.context.location.href, '/?session=task-new-1');
  assert.deepEqual(h.expanded, ['parent-1']);
  assert.equal(h.renders.length, 0);
});

test('a failed create logs and leaves the current session in place', async () => {
  const h = buildHarness({createResponse: () => ({ok: false, status: 409, json: async () => ({})})});

  h.context.createSession();
  await settle();

  assert.equal(h.context.SESSION_ID, 'session-a');
  assert.equal(h.consoleErrors.length, 1);
  assert.deepEqual(h.expanded, []);
});
