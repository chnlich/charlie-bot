// ---------------------------------------------------------------------------
// The pending-send store: an unsent chat message comes back into the composer
// ---------------------------------------------------------------------------
// The send path (chat/input.js) records each send's text and request id under
// charliebot-pending-send-<session> before the composer clears, and removes the
// entry on the first proof the server holds the message: the POST response, the
// socket echo of the request id, or a loaded history that carries it. A session
// render announces charliebot:session-rendered (sidebar/session-view.js), and
// the store's reconciliation puts the texts the proofs did not cover back into
// the composer, in click order, ahead of the draft.
//
// Driven through the real chat and sidebar modules on the session_context_stub
// harness, in the style of test_switch_session_telemetry.integration.test.js:
// chat POSTs resolve through manually answered promises, and a reload is a
// second harness over the first one's localStorage — a fresh page instance with
// an empty recorded-here set.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {baseSessionContext, bootstrapPayload, installSessionDocumentLookups, makeSidebarRow,
  stubPageTimers, createChatSidebarContext} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');
const {readStatic} = require('./read_static');

const tick = () => new Promise((resolve) => setImmediate(resolve));
const flush = async () => { await tick(); await tick(); };

const PENDING_SEND_KEY = 'charliebot-pending-send-session-a';
const DRAFT_KEY_A = 'charliebot-draft-session-a';

function buildHarness({localStorageItems = {}, bootstraps} = {}) {
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const headerName = createElement({id: 'header-session-name'});
  const rows = [makeSidebarRow('session-a', 'Alpha'), makeSidebarRow('session-b', 'Beta')];
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', headerName],
    ['msg-input', createElement({id: 'msg-input'})],
    ['backend-badge', createElement()],
    ['thinking', createElement({className: 'hidden'})],
    ['thinking-time', createElement()],
    ...rows.map((row) => [row.id, row]),
  ]);
  const {context, localStorageData} = baseSessionContext({elements, localStorageItems});
  // The chat control layer registers document listeners at load (the pending-send store's echo
  // listener, chat/input.js); the harness document calls each listener of an event's type in
  // registration order, as the page's document does.
  const listeners = new Map();
  context.document.addEventListener = (type, listener) => {
    if (!listeners.has(type)) listeners.set(type, []);
    listeners.get(type).push(listener);
  };
  context.document.dispatchEvent = (event) => {
    for (const listener of listeners.get(event.type) || []) listener(event);
    return true;
  };
  installSessionDocumentLookups(context, elements, messages, rows);
  const bootstraps_ = bootstraps || {
    'session-a': bootstrapPayload('session-a', 0, false),
    'session-b': bootstrapPayload('session-b', 0, false),
  };
  const chatPosts = [];
  // A chat POST the test has not answered yet; answer(held) settles it.
  const heldChatPosts = [];
  context.fetch = async (url, opts = {}) => {
    const chatMatch = url.match(/\/api\/chat\/([^/]+)\/message$/);
    if (chatMatch && opts.method === 'POST') {
      const body = JSON.parse(opts.body);
      chatPosts.push({sessionId: chatMatch[1], content: body.content, requestId: body.request_id});
      return new Promise((resolve) => heldChatPosts.push({body, resolve}));
    }
    const bootstrapMatch = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
    if (bootstrapMatch) return {ok: true, status: 200, json: async () => bootstraps_[bootstrapMatch[1]]};
    return {ok: true, status: 200, json: async () => ({})};
  };
  stubPageTimers(context);
  createChatSidebarContext(context);
  // config.js and file-upload.js ride the page ahead of the module list's runtime calls, so the
  // draft writer (saveDraftNow) and the upload gate are the real ones.
  vm.runInContext(readStatic('config.js'), context, {filename: 'config.js'});
  vm.runInContext(readStatic('file-upload.js'), context, {filename: 'file-upload.js'});
  // The optimistic bubble's one observable, the way the switch-telemetry harness records it.
  const appendedMessages = [];
  context.appendMessage = (role, content, inputMode, timestamp, files, requestId) => {
    appendedMessages.push({role, content, requestId});
    return {role, content, request_id: requestId, delivery_status: 'Sending…'};
  };
  // The delivery-status writes the send path and the echo listener make.
  const deliveryUpdates = [];
  context.setMessageDeliveryStatus = (msg, status) => deliveryUpdates.push({request_id: msg.request_id, status});
  return {context, input: elements.get('msg-input'), elements, bootstraps: bootstraps_, chatPosts,
    heldChatPosts, localStorageData, appendedMessages, deliveryUpdates};
}

// A reload: a fresh page instance over the first harness's localStorage, its recorded-here set empty.
function reloadHarness(previous) {
  return buildHarness({
    localStorageItems: Object.fromEntries(previous.localStorageData),
    bootstraps: previous.bootstraps,
  });
}

// One send the POST has not answered yet: the composer's text is taken and recorded. The send's
// promise stays pending, like the page's POST does; the record and the POST are synchronous at the click.
function sendUnanswered(h, text) {
  h.context.DRAFT_KEY = DRAFT_KEY_A;
  h.input.value = text;
  const sending = h.context.sendMessage();
  const last = h.chatPosts[h.chatPosts.length - 1];
  assert.deepEqual({sessionId: last.sessionId, content: last.content}, {sessionId: 'session-a', content: text});
  return sending;
}

function storedEntries(h) {
  return JSON.parse(h.localStorageData.get(PENDING_SEND_KEY));
}

// ---------------------------------------------------------------------------
// Reloads: the recorded texts come back
// ---------------------------------------------------------------------------
test('a send the POST has not answered comes back into the composer after a reload', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');
  assert.deepEqual(storedEntries(h1), [{request_id: h1.chatPosts[0].requestId, content: 'hello'}],
      'the record is in the store while the POST is unresolved');

  const h2 = reloadHarness(h1);
  h2.context.renderSessionView(h2.bootstraps['session-a']);

  assert.equal(h2.input.value, 'hello', 'the recorded text is back in the composer');
  assert.equal(h2.localStorageData.has(PENDING_SEND_KEY), false,
      'the put-back consumed the entry: the text is the draft now');
});

test('two pending sends come back in click order after a reload', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'first');
  sendUnanswered(h1, 'second');
  assert.deepEqual(storedEntries(h1).map((entry) => entry.content), ['first', 'second']);

  const h2 = reloadHarness(h1);
  h2.context.renderSessionView(h2.bootstraps['session-a']);

  assert.equal(h2.input.value, 'first\nsecond');
});

test('a send refused 4xx keeps its entry and comes back after a reload', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');
  h1.heldChatPosts[0].resolve({ok: false, status: 400, json: async () => ({})});
  await flush();
  assert.deepEqual(storedEntries(h1), [{request_id: h1.chatPosts[0].requestId, content: 'hello'}],
      'a refused send keeps its entry');

  const h2 = reloadHarness(h1);
  h2.context.renderSessionView(h2.bootstraps['session-a']);

  assert.equal(h2.input.value, 'hello');
});

test('a put-back lands ahead of the saved draft and becomes the draft', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');

  const h2 = reloadHarness(h1);
  h2.context.DRAFT_KEY = DRAFT_KEY_A;
  h2.localStorageData.set(DRAFT_KEY_A, 'typed later');
  h2.context.renderSessionView(h2.bootstraps['session-a']);

  assert.equal(h2.input.value, 'hello\ntyped later', 'the put-back is ahead of the saved draft');
  assert.equal(h2.localStorageData.get(DRAFT_KEY_A), 'hello\ntyped later',
      'the combined text is the ordinary composer draft');
});

// ---------------------------------------------------------------------------
// Proofs of receipt: the entry leaves the store
// ---------------------------------------------------------------------------
test('a send answered 202 records at the click and leaves no entry to put back', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');
  assert.equal(h1.localStorageData.has(PENDING_SEND_KEY), true);

  h1.heldChatPosts[0].resolve({ok: true, status: 202, json: async () => ({})});
  await flush();
  assert.equal(h1.localStorageData.has(PENDING_SEND_KEY), false, 'the 202 removed the entry');

  const h2 = reloadHarness(h1);
  h2.context.renderSessionView(h2.bootstraps['session-a']);
  assert.equal(h2.input.value, '', 'the composer carries no put-back');
});

test('a 202 the page never saw is proved by the loaded history', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');
  const requestId = h1.chatPosts[0].requestId;
  assert.equal(h1.localStorageData.has(PENDING_SEND_KEY), true,
      'the entry is recorded while the POST response is lost');

  const h2 = reloadHarness(h1);
  h2.bootstraps['session-a'] = Object.assign(h2.bootstraps['session-a'], {
    messages: [{role: 'user', content: 'hello', request_id: requestId, event_index: 6}],
  });
  h2.context.renderSessionView(h2.bootstraps['session-a']);

  assert.equal(h2.input.value, '', 'the loaded history holds the request id: no put-back');
  assert.equal(h2.localStorageData.has(PENDING_SEND_KEY), false, 'the proof dropped the entry');
});

test('the echo marks the bubble Sent and clears the entry before the 202', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');
  const requestId = h1.chatPosts[0].requestId;

  // websocket-handlers.js announces the echo of this tab's own send this way once the committed user
  // message arrives on the socket.
  h1.context.document.dispatchEvent(new CustomEvent('charliebot:client-send-echo', {detail: {request_id: requestId}}));

  assert.deepEqual(h1.deliveryUpdates, [{request_id: requestId, status: 'Sent'}],
      'the echo ends the bubble’s Sending… before the POST response does');
  assert.equal(h1.localStorageData.has(PENDING_SEND_KEY), false, 'the echo removed the entry');

  h1.heldChatPosts[0].resolve({ok: true, status: 202, json: async () => ({})});
  await flush();
  assert.equal(h1.localStorageData.has(PENDING_SEND_KEY), false);
});

// ---------------------------------------------------------------------------
// The live tab: its own sends never come back
// ---------------------------------------------------------------------------
test('a switch away and back in a live tab never puts an in-flight send back', async () => {
  const h1 = buildHarness();
  sendUnanswered(h1, 'hello');

  await h1.context.switchSession('session-b');
  await h1.context.switchSession('session-a');
  await flush();

  assert.equal(h1.input.value, '', 'the send is still in flight in this tab: no put-back');
  assert.deepEqual(storedEntries(h1), [{request_id: h1.chatPosts[0].requestId, content: 'hello'}],
      'the entry stays recorded until a proof of receipt');
  assert.deepEqual(h1.chatPosts.map((post) => post.content), ['hello'], 'no duplicate POST');
});

// ---------------------------------------------------------------------------
// A malformed store entry
// ---------------------------------------------------------------------------
test('a malformed store entry is a console error naming the key, not a silent reset', async () => {
  const errors = [];
  const h1 = buildHarness({localStorageItems: {[PENDING_SEND_KEY]: '[[[not a list'}});
  h1.context.console = {error: (msg) => errors.push(String(msg)), log: () => {}};

  h1.context.renderSessionView(h1.bootstraps['session-a']);

  assert.ok(errors.some((msg) => msg.includes(PENDING_SEND_KEY) && msg.includes('malformed')),
      'the error names the store key: ' + JSON.stringify(errors));
  assert.equal(h1.localStorageData.get(PENDING_SEND_KEY), '[[[not a list',
      'the raw entry is left standing for the operator to see');
  assert.equal(h1.input.value, '');
});
