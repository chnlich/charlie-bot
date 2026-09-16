const assert = require('node:assert/strict');
const test = require('node:test');

const {baseSessionContext, bootstrapPayload, createChatSidebarContext, installSessionDocumentLookups,
  stubPageTimers, SWITCH_TELEMETRY_URL} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');
const {runStaticModules} = require('./read_static');

// The create endpoint's success payload, per attempt.
const CREATED = (n) => ({ok: true, status: 200, json: async () => (
  {id: 'task-new-' + n, name: 'New manager task', profile: 'manager', schema_version: 2, backend: 'claude-opus-4.6'})});

// The harness for the primary New Task action (sidebar / welcome): the real
// session-view module in the page's module set, the real switchSession, and a
// recorded renderSessionView (the switch harness pattern).
function buildHarness(overrides = {}) {
  const input = createElement({id: 'msg-input', value: overrides.inputValue || ''});
  const realInputFocus = input.focus.bind(input);
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
  const h = {
    input,
    messages,
    elements,
    posts: [],
    renders: [],
    tabCalls: [],
    toasts: [],
    focused: null,
    consoleErrors: [],
  };
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = overrides.sessionId !== undefined ? overrides.sessionId : 'session-a';
  context.DRAFT_KEY = context.SESSION_ID ? 'charliebot-draft-' + context.SESSION_ID : null;
  context.currentFilter = overrides.currentFilter || 'tasks';
  context.__CHARLIEBOT_PREVIEW__ = overrides.preview !== undefined ? overrides.preview : true;
  context.crypto = {randomUUID: () => 'req-' + Math.random().toString(16).slice(2)};
  context.console.error = (...args) => h.consoleErrors.push(args);
  context.switchTab = (tab) => h.tabCalls.push(tab);
  context.showToast = (msg, isError) => h.toasts.push({msg, isError});
  input.focus = () => { h.focused = input; realInputFocus(); };
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
  // task-panel.js sits outside the chat/ and sidebar/ script prefixes; the page
  // loads it right after the sidebar modules and the router resolves
  // globalThis.TaskPanel at call time.
  runStaticModules(context, ['task-panel.js']);
  // After the module load (Sidebar.wire re-exposed the real renderSessionView):
  // the switch harness records what the switch would render.
  context.renderSessionView = (data) => h.renders.push(data);
  h.context = context;
  return h;
}

async function settle(rounds = 10) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}


test('New Task in the tasks filter creates one root manager and opens Chat with a focused composer', async () => {
  const h = buildHarness({});
  h.context.createSessionOrTask();
  await settle();
  assert.equal(h.posts.length, 1, 'one create request');
  const body = h.posts[0].body;
  assert.equal(body.task_parent_id, null);
  assert.equal(body.profile, 'manager');
  assert.deepEqual(body.task, {goal: '', acceptance: [], context_refs: []});
  assert.ok(body.request_id);
  assert.equal(h.context.SESSION_ID, 'task-new-1', 'the new task is the active session');
  assert.equal(h.renders.length, 1);
  assert.equal(h.tabCalls[h.tabCalls.length - 1], 'chat', 'Chat is the tab the new task opens in');
  assert.equal(h.focused, h.input, 'the composer holds the cursor');
  assert.equal(h.input.value, '', 'the empty composer is ready to type');
});

test('a new task opens Chat even when the previous task displayed a non-Chat tab', async () => {
  const h = buildHarness({});
  h.context.switchTab('task');
  h.context.createSessionOrTask();
  await settle();
  assert.equal(h.context.SESSION_ID, 'task-new-1');
  assert.equal(h.tabCalls[h.tabCalls.length - 1], 'chat', 'the create moved the view to Chat');
  assert.equal(h.focused, h.input);
});

test('rapid double clicks fire exactly one create', async () => {
  const h = buildHarness({});
  h.context.createSessionOrTask();
  h.context.createSessionOrTask(); // lands while the first create-and-open is in flight
  h.context.createSessionOrTask();
  await settle();
  assert.equal(h.posts.length, 1, 'one pending create action absorbs the rapid clicks');
  assert.equal(h.context.SESSION_ID, 'task-new-1');
});

test('a failed create is visible, retryable, keeps the view and drafts, and replays one request id', async () => {
  let attempts = 0;
  const h = buildHarness({
    createResponse: (n) => {
      attempts = n;
      if (n === 1) {
        return {ok: false, status: 409, json: async () => ({detail: {message: 'refused', blockers: ['x']}})};
      }
      return CREATED(n - 1);
    },
  });
  h.input.value = 'half-written message draft';
  h.context.createSessionOrTask();
  await settle();
  assert.equal(attempts, 1);
  assert.equal(h.toasts.length, 1, 'the failure surfaced');
  assert.equal(h.toasts[0].isError, true);
  assert.ok(/Create task failed/.test(h.toasts[0].msg));
  assert.equal(h.context.SESSION_ID, 'session-a', 'the user\u2019s current view stayed');
  assert.equal(h.input.value, 'half-written message draft', 'the draft stayed');
  assert.equal(h.focused, null, 'no navigation happened');
  assert.ok(h.consoleErrors.length >= 1, 'the failure is logged');

  h.context.createSessionOrTask(); // the retry
  await settle();
  assert.equal(h.posts.length, 2);
  assert.equal(h.posts[1].body.request_id, h.posts[0].body.request_id,
    'the retry replays the failed attempt\u2019s request id (one node, never two)');
  assert.equal(h.context.SESSION_ID, 'task-new-1', 'the retry landed in the new task');
  assert.equal(h.focused, h.input);
});

test('a create from the welcome screen navigates to the new task and leaves the composer-focus flag', async () => {
  const h = buildHarness({sessionId: null, preview: true});
  h.context.createSessionOrTask();
  await settle();
  assert.equal(h.posts.length, 1);
  assert.equal(h.context.location.href, '/?session=task-new-1',
    'the welcome screen has no composer DOM: the landing is the existing full-page load');
  assert.equal(h.context.sessionStorage.getItem('charliebot-focus-composer'), '1');
  assert.equal(h.context.SESSION_ID, null, 'no SPA switch ran');
});

test('outside the tasks filter and preview, the primary button keeps the legacy session create', async () => {
  const h = buildHarness({currentFilter: 'all', preview: false});
  h.context.createSessionOrTask();
  await settle();
  const posts = h.posts.filter((p) => p.url === '/api/sessions/');
  assert.equal(posts.length, 1);
  assert.equal(posts[0].body.request_id, undefined, 'no task-create fields on the legacy path');
  assert.equal(posts[0].body.profile, undefined);
  assert.equal(posts[0].body.task, undefined);
  assert.equal(h.context.SESSION_ID, 'task-new-1', 'the legacy create switched to the new session');
  assert.equal(h.focused, h.input, 'the legacy path keeps its composer focus');
});
