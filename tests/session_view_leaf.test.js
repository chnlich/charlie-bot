// ---------------------------------------------------------------------------
// The session view names the worker leaf (session-view.js): loading a session
// whose profile is worker sets the leaf for sidebar/workers.js, hands it the
// closing summary from the chat projection, and resets the leaf container;
// any other session clears the leaf and leaves the container alone. The tab
// switch that swaps the containers is tabs.js's (covered in leaf_view.test.js).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {baseSessionContext, bootstrapPayload, createChatSidebarContext, installSessionDocumentLookups,
  stubPageTimers} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');

function buildHarness(sessionId) {
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 100;
  messages.scrollTop = 0;
  const tabWorkers = createElement({id: 'tab-workers'});
  const elements = new Map([
    ['messages', messages],
    ['tab-workers', tabWorkers],
    ['header-session-name', createElement({id: 'header-session-name'})],
    ['backend-badge', createElement()],
    ['input-model-badge', createElement()],
  ]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = sessionId;
  installSessionDocumentLookups(context, elements, messages, []);
  stubPageTimers(context);
  createChatSidebarContext(context);
  const h = {context, tabWorkers, leafCalls: [], tabs: []};
  const realSetLeaf = context.setLeafSession;
  context.setLeafSession = (id, summary) => { h.leafCalls.push({id, summary}); realSetLeaf(id, summary); };
  context.switchTab = (tab) => h.tabs.push(tab);
  return h;
}

function payload(sessionId, overrides = {}, messages) {
  const data = bootstrapPayload(sessionId, 0, false);
  data.session = {...data.session, ...overrides};
  if (messages) data.messages = messages;
  return data;
}

test('a worker session becomes the leaf, with the closing summary and a reset container', () => {
  const h = buildHarness('leaf-1');
  h.context.renderSessionView(payload('leaf-1', {profile: 'worker', task_parent_id: 'root-1'}, [
    {role: 'assistant', content: 'working', event_index: 1},
    {role: 'system', content: 'Task completed: Parser lands with tests', event_index: 2},
  ]));
  assert.deepEqual([...h.leafCalls], [{id: 'leaf-1', summary: 'Parser lands with tests'}]);
  assert.equal(h.context.Sidebar.activeSessionIsLeaf(), true);
  assert.match(h.tabWorkers.innerHTML, /Loading/, 'the container resets for the fresh load');
  assert.deepEqual([...h.tabs], ['chat'], 'the tab switch swaps the containers');
});

test('a manager session clears the leaf and leaves the leaf container alone', () => {
  const h = buildHarness('root-1');
  h.tabWorkers.innerHTML = 'previous leaf';
  h.context.renderSessionView(payload('root-1', {profile: 'manager'}));
  assert.deepEqual([...h.leafCalls], [{id: null, summary: ''}]);
  assert.equal(h.context.Sidebar.activeSessionIsLeaf(), false);
  assert.equal(h.tabWorkers.innerHTML, 'previous leaf');

  // A legacy session without a profile is not a leaf either.
  h.context.renderSessionView(payload('root-1'));
  assert.equal(h.context.Sidebar.activeSessionIsLeaf(), false);
});

test('leafClosingSummary reads the newest closing line and tolerates its absence', () => {
  const {context} = buildHarness('leaf-1');
  const summary = context.Sidebar.leafClosingSummary;
  assert.equal(summary([
    {role: 'system', content: 'Task cancelled: superseded'},
    {role: 'system', content: 'Task completed: second try'},
    {role: 'assistant', content: 'Task completed: not a system line'},
  ]), 'second try');
  assert.equal(summary([{role: 'system', content: 'Task completed:'}]), '');
  assert.equal(summary([{role: 'system', content: 'Scheduled run skipped'}]), '');
  assert.equal(summary([]), '');
  assert.equal(summary(undefined), '');
});
