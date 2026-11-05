const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {createElement, createEscapingElement} = require('./dom_element_stub');
const {createFakeWebSocketClass} = require('./fake_websocket');
const {readStatic} = require('./read_static');

// renderMessages loads the real chat/rendering.js, so each engine node holds the markup renderMessage builds.
function buildPage(initialMessages = [], sessionId = 'session-a', {renderMessages = false} = {}) {
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 0;
  messages.scrollHeight = 0;
  messages.scrollTop = 0;
  messages.getBoundingClientRect = () => ({top: 0});
  messages.removeEventListener = () => {};
  const renderedMessages = initialMessages.slice();
  const context = {
    SESSION_ID: sessionId,
    eventCursor: 0,
    // The page global index.html renders next to the ?v= script URLs; websocket.js
    // sends it as the handshake's asset_version.
    STATIC_ASSET_VERSION: 'stub-asset-version',
    document: {
      createElement: (tag) => createElement({tagName: tag}),
      createTreeWalker: () => ({}),
      getElementById: (id) => id === 'messages' ? messages : null,
      dispatchEvent: () => true,
    },
    CustomEvent,
    TextEncoder,
    location: {protocol: 'http:', host: 'localhost'},
    localStorage: {getItem: () => null},
    console: {log() {}, error() {}},
    performance: {now: () => 0},
    WebSocket: createFakeWebSocketClass(),
    wsUrlWithToken: (path) => path,
    setTimeout: () => 1,
    clearTimeout() {},
    hideStreaming() {},
    showStreaming() {},
    observeWebStatusMessages() {},
    appendMessageObject(msg, _sessionId, forceScroll) {
      const engine = context.Chat.activeTurnEngineFor(messages);
      if (engine) engine.appendMessage(msg, forceScroll);
      renderedMessages.push(msg);
    },
    appendMessage() {},
    refreshSessionStatusPanel() {},
    startThinking() {},
    stopThinking() {},
    pollActiveSessionView() {},
    refreshSessionStatusNow() {},
    applyRunningChanged() {},
    setSessionPendingTriggerIndicator() {},
    updateSidebarSessionName() {},
    handleSidebarSearch() {},
    switchSidebarFilter() {},
    currentFilter: 'all',
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readStatic('websocket.js'), context, {filename: 'websocket.js'});
  vm.runInContext(readStatic('chat/namespace.js'), context, {filename: 'chat/namespace.js'});
  vm.runInContext(readStatic('chat/shared.js'), context, {filename: 'chat/shared.js'});
  if (renderMessages) {
    // escapeHtml round-trips text through an element's innerHTML, so the rendered markup needs the escaping stub.
    context.document.createElement = (tag) => createEscapingElement(tag);
    // The stand-ins chat_rendering_context_stub.js uses for markdown and the file-link pass. They bind before
    // rendering.js loads, as its module-scope consts read them then.
    Object.assign(context, {
      marked: {parse: (value) => String(value || '')},
      fixNestedFences: (value) => String(value || ''),
      renderProseMarkdown: (value) => String(value || ''),
      renderChatMath: () => {},
      scheduleCodeHighlightFlush: () => {},
      wrapWideChars: (html) => html,
      renderUserMessageBubble: () => '',
    });
    context.Chat.renderRoundRatingButtons = () => '';
    context.Chat.markFileLinks = () => {};
    vm.runInContext(readStatic('chat/rendering.js'), context, {filename: 'chat/rendering.js'});
    // The harness parses no HTML, so the node holds no toggles for the engine to carry open state across.
    context.Chat.buildTurnEngineMessageNode = (msg, sid) => ({
      innerHTML: context.renderMessage(msg, sid),
      querySelectorAll: () => [],
    });
  } else {
    context.Chat.buildTurnEngineMessageNode = () => createElement();
  }
  vm.runInContext(readStatic('chat/turn-engine.js'), context, {filename: 'chat/turn-engine.js'});
  vm.runInContext(readStatic('websocket-handlers.js'), context, {filename: 'websocket-handlers.js'});
  const engine = context.Chat.TurnEngine.mountIfAvailable(messages, initialMessages, sessionId);
  const socket = connect(context);
  return {context, engine, messages, socket, renderedMessages};
}

function connect(context) {
  context.connectWS();
  const socket = context.WebSocket.instances.at(-1);
  socket.emitOpen();
  return socket;
}

function deliver(socket, message) {
  socket.emitMessage({type: 'message', message});
}

function deliverUpdate(socket, message) {
  socket.emitMessage({type: 'message_update', message});
}

// A committed assistant message whose Agent row started one subagent, as the store sends it.
function committedDraft(subagent) {
  return {
    role: 'assistant',
    id: 'draft-1',
    content: 'Two reads are running.',
    tools: [{
      id: 'toolu_1',
      name: 'Agent',
      input: {description: 'Read the log'},
      output: 'Report: the log is clean.',
      is_error: false,
      subagent,
    }],
  };
}

function subagentOf(status, toolUses) {
  return {
    type: 'general-purpose',
    description: 'Read the log',
    prompt: 'Read the log',
    status,
    tool_uses: toolUses,
    duration_ms: 1200,
    total_tokens: 900,
    tools: [],
  };
}

function screenMultiset(messages) {
  return Array.from(messages, (msg) => `${msg.role}\u0000${msg.content || ''}`).sort();
}

test('an identical replay or live frame leaves the on-screen multiset unchanged', () => {
  const page = buildPage([{role: 'assistant', id: 'first', content: 'already here'}]);
  const before = screenMultiset(page.renderedMessages);

  deliver(page.socket, {role: 'assistant', id: 'first', content: 'already here'});

  assert.deepEqual(screenMultiset(page.renderedMessages), before);
});

test('the turn engine records paginated messages before WebSocket draws', () => {
  const page = buildPage();
  const older = {role: 'user', id: 'older', content: 'page message'};
  page.engine.prependMessages([older, {...older}]);
  page.renderedMessages.unshift(older);
  assert.equal(page.engine.entries.length, 1);
  const before = screenMultiset(page.renderedMessages);

  deliver(page.socket, {role: 'user', id: 'older', content: 'page message'});

  assert.deepEqual(screenMultiset(page.renderedMessages), before);
});

test('an event repeated by replay and live delivery appears once', () => {
  for (const replayFirst of [true, false]) {
    const page = buildPage();
    const replay = {role: 'assistant', id: 'overlap', content: 'one event'};
    const live = {...replay};
    deliver(page.socket, replayFirst ? replay : live);
    deliver(page.socket, replayFirst ? live : replay);

    assert.deepEqual(screenMultiset(page.renderedMessages), ['assistant\u0000one event']);
  }
});

test('a claimed user echo re-keys its local record before its duplicate arrives', () => {
  const requestId = 'request-one';
  const echo = {role: 'user', id: 'event-one', request_id: requestId, content: 'sent once'};

  for (const replayFirst of [true, false]) {
    const page = buildPage();
    const localMessage = {role: 'user', id: requestId, request_id: requestId, content: 'sent once'};
    page.engine.appendMessage(localMessage, true);
    page.renderedMessages.push(localMessage);
    vm.runInContext('pendingUserRequestIds.add("request-one")', page.context);

    const replay = {...echo};
    const live = {...echo};
    deliver(page.socket, replayFirst ? replay : live);
    deliver(page.socket, replayFirst ? live : replay);

    assert.deepEqual(screenMultiset(page.renderedMessages), ['user\u0000sent once']);
    assert.equal(page.engine.hasMessage({role: 'user', id: 'event-one', content: 'sent once'}), true);
    assert.equal(page.engine.hasMessage({role: 'user', id: requestId, content: 'sent once'}), false);
  }
});

test('same-id messages with another role or content remain visible', () => {
  const page = buildPage([{role: 'plan', id: 'shared', content: 'first plan'}]);
  const olderPlan = {role: 'plan', id: 'shared', content: 'older plan'};
  page.engine.prependMessages([olderPlan]);
  page.renderedMessages.unshift(olderPlan);
  deliver(page.socket, {role: 'assistant', id: 'shared', content: 'assistant bubble'});
  deliver(page.socket, {role: 'plan', id: 'shared', content: 'third plan'});

  assert.deepEqual(screenMultiset(page.renderedMessages), [
    'assistant\u0000assistant bubble',
    'plan\u0000first plan',
    'plan\u0000older plan',
    'plan\u0000third plan',
  ]);
});

test('a frame without an id still draws', () => {
  const page = buildPage();

  deliver(page.socket, {role: 'assistant', content: 'without an id'});
  deliver(page.socket, {role: 'assistant', content: 'without an id'});

  assert.deepEqual(screenMultiset(page.renderedMessages), [
    'assistant\u0000without an id',
    'assistant\u0000without an id',
  ]);
});

test('disposing a session engine clears its message records', () => {
  const page = buildPage([{role: 'assistant', id: 'same', content: 'same text'}]);
  const previousEngine = page.engine;
  page.context.SESSION_ID = 'session-b';
  page.renderedMessages.length = 0;
  const nextEngine = page.context.Chat.TurnEngine.mountIfAvailable(page.messages, [], 'session-b');
  const socket = connect(page.context);

  deliver(socket, {role: 'assistant', id: 'same', content: 'same text'});

  assert.equal(previousEngine.messageRecords.size, 0);
  assert.deepEqual(screenMultiset(page.renderedMessages), ['assistant\u0000same text']);
});

test('a page without a mounted turn engine keeps the legacy draw path', () => {
  const page = buildPage();
  page.engine.dispose();
  page.context.Chat.turnEngines.delete(page.messages);

  deliver(page.socket, {role: 'assistant', id: 'same', content: 'same text'});
  deliver(page.socket, {role: 'assistant', id: 'same', content: 'same text'});

  assert.equal(page.renderedMessages.length, 2);
});

test('a message_update replaces the committed message where it stands and its card shows the new state', () => {
  const page = buildPage([], 'session-a', {renderMessages: true});
  deliver(page.socket, committedDraft(subagentOf('running', 1)));
  const [entry] = page.engine.entries;
  assert.match(entry.node.innerHTML, />running</);

  deliverUpdate(page.socket, committedDraft(subagentOf('completed', 3)));

  assert.equal(page.engine.entries.length, 1);
  assert.equal(page.engine.entries[0], entry, 'the entry keeps its place in the list');
  const html = entry.node.innerHTML;
  assert.match(html, />completed</);
  assert.doesNotMatch(html, />running</);
  assert.match(html, />3 tool/, 'the card counts the subagent\'s own tool uses');
  assert.match(html, />1 tool/, 'the parent counts only its own rows');
  assert.ok(html.indexOf('>completed<') < html.indexOf('Report: the log is clean.'),
      'the card sits under the Agent row, above that row\'s output');
});

test('a message_update for a message the list does not hold throws and leaves the list as it was', () => {
  const page = buildPage([], 'session-a', {renderMessages: true});
  deliver(page.socket, committedDraft(subagentOf('running', 1)));
  const before = page.engine.entries.map((entry) => entry.msg);

  assert.throws(
      () => deliverUpdate(page.socket, {...committedDraft(subagentOf('completed', 3)), id: 'draft-unknown'}),
      /message_update names no message in this list: draft-unknown/);
  assert.deepEqual(page.engine.entries.map((entry) => entry.msg), before);
});
