// ---------------------------------------------------------------------------
// A projected legacy worker-thread leaf's click (sidebar/workers.js
// openWorkerThread): it never switches the session to the thread id and never
// sets a leaf session; it lands on the owning session and paints that one
// thread's ordinary Workers-tab card, expanded. Harness follows
// sidebar_tree_nesting.test.js (session_context_stub + createChatSidebarContext).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { baseSessionContext, buildSidebarFilterElements, createChatSidebarContext, inlinePageTimers } =
  require('./session_context_stub');

const THREAD_ID = 'thread-legacy-1';
const PARENT_ID = 'legacy-root';

function buildContext() {
  const workersPane = createElement({id: 'tab-workers', className: 'hidden'});
  const elements = new Map([
    ['session-list', createElement()],
    ['tab-workers', workersPane],
    ...buildSidebarFilterElements(),
  ]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = PARENT_ID;
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  // The thread detail endpoints openWorkerThread and the expanded card read.
  const fetches = [];
  context.fetch = async (url) => {
    fetches.push(url);
    if (url === `/api/threads/${PARENT_ID}/threads/${THREAD_ID}`) {
      return {ok: true, json: async () => ({
        id: THREAD_ID, description: 'old delegation', status: 'completed',
        created_at: '2026-04-01T00:00:00Z', backend: 'claude-opus-4.6',
      })};
    }
    return {ok: true, json: async () => ({})};
  };
  createChatSidebarContext(context);
  return {context, workersPane, fetches};
}

test('an already-active parent paints the projected leaf card without switching', async () => {
  const {context, workersPane, fetches} = buildContext();
  const switches = [];
  const realSwitch = context.switchSession;
  context.switchSession = async (id) => { switches.push(id); await realSwitch(id); };
  const details = [];
  context.toggleThreadDetail = async (threadId, sessionId) => { details.push([threadId, sessionId]); };

  await context.openWorkerThread(PARENT_ID, THREAD_ID);

  assert.deepEqual(switches, [], 'the parent session is already active: no switch');
  assert.deepEqual(fetches, [`/api/threads/${PARENT_ID}/threads/${THREAD_ID}`]);
  assert.equal(workersPane.classList.contains('hidden'), false);
  assert.match(workersPane.innerHTML, new RegExp(`id="thread-dot-${THREAD_ID}"`));
  assert.match(workersPane.innerHTML, /old delegation/);
  assert.deepEqual(details, [[THREAD_ID, PARENT_ID]]);
  // The pane is not a leaf session: the thread id never became SESSION_ID.
  assert.equal(context.SESSION_ID, PARENT_ID);
  assert.equal(context.Sidebar.activeSessionIsLeaf(), false);
});

test('a leaf click on another active session switches to the parent first', async () => {
  const {context, workersPane} = buildContext();
  context.SESSION_ID = 'other-session';
  const switches = [];
  context.switchSession = async (id) => {
    switches.push(id);
    context.SESSION_ID = id;  // the real switch lands before openWorkerThread continues
  };
  context.toggleThreadDetail = async () => {};

  await context.openWorkerThread(PARENT_ID, THREAD_ID);

  assert.deepEqual(switches, [PARENT_ID]);
  assert.equal(workersPane.classList.contains('hidden'), false);
  assert.match(workersPane.innerHTML, new RegExp(`id="thread-dot-${THREAD_ID}"`));
  assert.equal(context.SESSION_ID, PARENT_ID);
  assert.equal(context.Sidebar.activeSessionIsLeaf(), false);
});

test('a failed thread fetch leaves the pane alone', async () => {
  const {context, workersPane} = buildContext();
  context.fetch = async () => ({ok: false, status: 404});
  context.toggleThreadDetail = async () => {};

  await context.openWorkerThread(PARENT_ID, THREAD_ID);

  assert.equal(workersPane.classList.contains('hidden'), true);
  assert.equal(workersPane.innerHTML, '');
});
