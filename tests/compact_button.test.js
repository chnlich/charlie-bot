const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const FILE_UPLOAD_JS = readStatic('file-upload.js');
const DISPLAYED_SESSION_JS = readStatic('chat/displayed-session.js');
const { loadChatRenderingModules } = require('./chat_rendering_context_stub');
const { loadSidebarStatusContext } = require('./sidebar_status_context_stub');

const { FakeElement } = require('./fake_dom');
const { createElement } = require('./dom_element_stub');

function makeDocument(elements) {
  return {
    getElementById(id) {
      if (elements.has(id)) return elements.get(id);
      // Full renders write everything as one flat `innerHTML` string, and the
      // failure notice is just another node in that string -- so a lookup by
      // id also has to find it there, the same way a real DOM query would.
      const container = elements.get('messages');
      if (container && String(container.innerHTML).includes('id="' + id + '"')) {
        return {};
      }
      return null;
    },
    createElement(tag) {
      return new FakeElement(tag);
    },
    querySelector() {
      return null;
    },
  };
}

function fakeHeaderButton() {
  return {
    disabled: false,
    title: '',
    dataset: {},
    classList: { toggle() {}, contains() { return false; } },
  };
}

function loadChatContext(elements) {
  let requestSequence = 0;
  const context = {
    document: makeDocument(elements),
    console: { error: () => {}, log: () => {} },
    marked: { parse: (v) => String(v || '') },
    fixNestedFences: (v) => String(v || ''),
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    renderUserMessageBubble: () => '',
    restoreBottomPin: () => {},
    hideStreaming: () => {},
    observeWebStatusMessages: () => {},
    noteMessageForStatusView: () => {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'session-a',
    confirm: () => true,
    fetch: () => Promise.resolve({ok: true, status: 202, json: async () => ({})}),
    crypto: {randomUUID: () => `request-${++requestSequence}`},
    // No stored key: mirrors page-load order config.js → websocket.js on the no-key path.
    wsUrlWithToken: (path) => path,
    // index.html's host fact; the displayed-session module restates it in the tab title.
    PAGE_HOSTNAME: 'localhost:8000',
    // No stubs above run config.js: stand in for its shared header literal.
    JSON_HEADERS: {'Content-Type': 'application/json'},
  };
  vm.createContext(context);
  // The real file-upload.js rides the page before chat/input.js; its gate
  // (blockIfUploadsInFlight) is what compactContext and sendMessage share.
  vm.runInContext(FILE_UPLOAD_JS, context, { filename: 'file-upload.js' });
  loadChatRenderingModules(context);
  vm.runInContext(readStatic('websocket.js'), context, { filename: 'websocket.js' });
  vm.runInContext(DISPLAYED_SESSION_JS, context, { filename: 'chat/displayed-session.js' });
  vm.runInContext(readStatic('websocket-handlers.js'), context, { filename: 'websocket-handlers.js' });
  vm.runInContext(readStatic('chat/input.js'), context, { filename: 'chat/input.js' });
  return context;
}

function fireClick(btn, handler) {
  // Mirrors real browser semantics: a disabled control's activation behavior
  // (including .click()) never fires -- there is no separate JS-level guard.
  if (btn.disabled) return;
  handler();
}

// ---------------------------------------------------------------------------
// 1. Gate is exhaustive over the backend-type enumeration.
// ---------------------------------------------------------------------------
test('updateBackendHeaderControls reads #compact-btn state and title from BACKEND_COMPACT, unlisted types included', () => {
  const BACKEND_COMPACT = {
    'cc-claude': {manual: true, hint: null},
    codex: {manual: false, hint: 'codex only compacts automatically — tune model_auto_compact_token_limit'},
    opencode: {manual: false, hint: null},
  };
  const compactBtn = fakeHeaderButton();
  const elements = new Map([['compact-btn', compactBtn]]);
  const context = loadSidebarStatusContext({
    document: { getElementById: (id) => elements.get(id) || null },
    BACKEND_COMPACT,
  });

  const expected = [
    ['cc-claude', false, ''],
    ['codex', true, 'codex only compacts automatically — tune model_auto_compact_token_limit'],
    ['opencode', true, 'Manual compaction is not supported on this backend'],
    // A session whose type the configuration no longer lists, and one with no type.
    ['gemini', true, 'Manual compaction is not supported on this backend'],
    ['', true, 'Manual compaction is not supported on this backend'],
  ];
  for (const [type, disabled, title] of expected) {
    context.updateBackendHeaderControls(type);
    assert.equal(compactBtn.disabled, disabled, `type ${type}`);
    assert.equal(compactBtn.title, title, `title for type ${type}`);
  }
});

// ---------------------------------------------------------------------------
// 2. Disabled means no request.
// ---------------------------------------------------------------------------
test('a disabled compact-btn never reaches fetch no matter how many times it is clicked', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const calls = [];
  context.fetch = (...args) => { calls.push(args); return Promise.resolve({ ok: true }); };
  context.confirm = () => true;

  const compactBtn = { disabled: true };
  fireClick(compactBtn, () => { context.compactContext(); });
  fireClick(compactBtn, () => { context.compactContext(); });
  fireClick(compactBtn, () => { context.compactContext(); });

  assert.equal(calls.length, 0);
});

// ---------------------------------------------------------------------------
// 3. Indistinguishable from sending a message.
// ---------------------------------------------------------------------------
test('compactContext posts the exact same request shape as the shared message-send path', async () => {
  const elements = new Map([
    ['messages', new FakeElement('DIV')],
    ['usage-text', Object.assign(new FakeElement('SPAN'), { textContent: '50k / 200k' })],
  ]);
  const context = loadChatContext(elements);
  context.confirm = () => true;
  const calls = [];
  context.fetch = (url, opts) => {
    calls.push({url, opts});
    return Promise.resolve({ok: true, status: 202, json: async () => ({})});
  };

  await context.compactContext();
  assert.equal(calls.length, 1);

  await context.Chat.postChatMessage('session-a', '/compact');
  assert.equal(calls.length, 2);

  assert.equal(calls[0].url, calls[1].url);
  assert.equal(calls[0].opts.method, calls[1].opts.method);
  const firstBody = JSON.parse(calls[0].opts.body);
  const secondBody = JSON.parse(calls[1].opts.body);
  assert.equal(firstBody.content, '/compact');
  assert.equal(secondBody.content, '/compact');
  assert.equal(firstBody.request_id, 'request-1');
  assert.equal(secondBody.request_id, 'request-2');
});

test('a /compact the server refuses fails its bubble without ending a thinking round it never started', async () => {
  const context = loadChatContext(new Map([
    ['messages', new FakeElement('DIV')],
    ['usage-text', Object.assign(new FakeElement('SPAN'), { textContent: '50k / 200k' })],
  ]));
  const systemLines = [];
  let stopped = 0;
  let attempts = 0;
  context.appendMessage = (role, content) => {
    if (role === 'system') systemLines.push(content);
    return {};
  };
  context.setMessageDeliveryStatus = () => {};
  context.stopThinking = () => {
    stopped += 1;
  };
  context.fetch = async () => {
    attempts += 1;
    return {ok: false, status: 409, json: async () => ({})};
  };

  await context.compactContext();

  assert.equal(attempts, 1, 'a 4xx is never retried');
  assert.equal(stopped, 0);
  assert.deepEqual(systemLines, ['Failed to send message']);
});

// ---------------------------------------------------------------------------
// 3b. A typed /compact rides the same send path (no client-side interception).
// ---------------------------------------------------------------------------
test('a typed /compact posts through the shared message-send path like the Compact button', async () => {
  const input = createElement({id: 'msg-input'});
  input.value = '/compact';
  input.style = {};
  const elements = new Map([
    ['messages', new FakeElement('DIV')],
    ['msg-input', input],
    ['usage-text', Object.assign(new FakeElement('SPAN'), { textContent: '50k / 200k' })],
  ]);
  const context = loadChatContext(elements);
  // The page wires thinking state and drafts from config.js and chat/rendering;
  // this harness only needs their presence.
  context.DRAFT_KEY = null;
  context.startThinking = () => {};
  context.stopThinking = () => {};
  const calls = [];
  context.fetch = (url, opts) => {
    calls.push({url, opts});
    return Promise.resolve({ok: true, status: 202, json: async () => ({})});
  };

  await context.sendMessage();

  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/chat/session-a/message');
  assert.equal(calls[0].opts.method, 'POST');
  // The send path's full payload shape; the typed /compact rides it as content.
  assert.deepEqual(JSON.parse(calls[0].opts.body), {
    content: '/compact',
    uploaded_files: [],
    input_mode: null,
    request_id: 'request-1',
  });

  // The button path lands on the same URL and body.
  context.confirm = () => true;
  await context.compactContext();
  assert.equal(calls.length, 2);
  assert.equal(calls[1].url, calls[0].url);
  assert.deepEqual(JSON.parse(calls[1].opts.body), {
    content: '/compact',
    request_id: 'request-2',
  });
});

// ---------------------------------------------------------------------------
// 3c. A dictated message names its input mode on the wire, once.
// ---------------------------------------------------------------------------
test('a dictated message posts input_mode voice and the next typed message posts none', async () => {
  const input = createElement({id: 'msg-input'});
  input.style = {};
  const elements = new Map([
    ['messages', new FakeElement('DIV')],
    ['msg-input', input],
  ]);
  const context = loadChatContext(elements);
  context.DRAFT_KEY = null;
  context.startThinking = () => {};
  context.stopThinking = () => {};
  const calls = [];
  context.fetch = (url, opts) => {
    calls.push({url, opts});
    return Promise.resolve({ok: true, status: 202, json: async () => ({})});
  };

  context.Chat.setInputMode('voice');
  input.value = 'dictated words';
  await context.sendMessage();
  input.value = 'typed words';
  await context.sendMessage();

  assert.deepEqual(
    calls.map((call) => JSON.parse(call.opts.body)),
    [
      {content: 'dictated words', uploaded_files: [], input_mode: 'voice', request_id: 'request-1'},
      {content: 'typed words', uploaded_files: [], input_mode: null, request_id: 'request-2'},
    ]);
});

// ---------------------------------------------------------------------------
// 3d. The composer note shows on demand and dies with the input mode's clears.
// ---------------------------------------------------------------------------
function loadNoteContext() {
  const input = createElement({id: 'msg-input'});
  input.style = {};
  const note = createElement({id: 'input-note', className: 'hidden text-xs text-amber-400 mb-2'});
  const elements = new Map([
    ['messages', new FakeElement('DIV')],
    ['msg-input', input],
    ['input-note', note],
  ]);
  const context = loadChatContext(elements);
  context.DRAFT_KEY = null;
  context.startThinking = () => {};
  context.stopThinking = () => {};
  context.fetch = () => Promise.resolve({ok: true, status: 202, json: async () => ({})});
  return { context, input, note };
}

const noteHidden = (note) => note.classList.contains('hidden');

test('setInputNote shows the note line with its text', () => {
  const { context, note } = loadNoteContext();
  assert.ok(noteHidden(note)); // the note line starts hidden

  context.Chat.setInputNote('note words');
  assert.equal(note.textContent, 'note words');
  assert.ok(!noteHidden(note));
});

test('a send clears the note with the composer', async () => {
  const { context, input, note } = loadNoteContext();
  input.value = 'words';
  context.Chat.setInputNote('note words');
  assert.ok(!noteHidden(note));

  await context.sendMessage();
  assert.equal(note.textContent, '');
  assert.ok(noteHidden(note));
});

test('emptying the composer clears the note', () => {
  const { context, input, note } = loadNoteContext();
  input.value = 'words';
  context.Chat.setInputNote('note words');

  input.value = '';
  for (const handler of input._listeners.input) handler();
  assert.equal(note.textContent, '');
  assert.ok(noteHidden(note));
});

test('setInputMode(null) clears the note', () => {
  const { context, note } = loadNoteContext();
  context.Chat.setInputNote('note words');
  assert.ok(!noteHidden(note));

  context.Chat.setInputMode(null);
  assert.equal(note.textContent, '');
  assert.ok(noteHidden(note));
});

test("setInputMode('voice') keeps the note", () => {
  const { context, note } = loadNoteContext();
  context.Chat.setInputNote('note words');

  context.Chat.setInputMode('voice');
  assert.equal(note.textContent, 'note words');
  assert.ok(!noteHidden(note));
});

// ---------------------------------------------------------------------------
// 4. The compaction verdict comes from the backend event, not client inference.
// ---------------------------------------------------------------------------
test('a context_compact_failed system message renders as an ordinary chat bubble through the normal render path', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  const messages = [
    { role: 'user', content: '/compact', id: 'u1' },
    { role: 'system', content: 'Compaction failed — context too large', kind: 'context_compact_failed', id: 'c1' },
    { role: 'separator', id: 's1' },
  ];

  context.renderMessagesIntoContainer(container, messages, 'session-a');

  assert.match(container.innerHTML, /Compaction failed.*context too large/);
});

test('a context_compact_failed message delivered live renders through the same bubble path as any other message', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  context._commitMessage({ role: 'system', content: 'Compaction failed', kind: 'context_compact_failed', id: 'c1' });

  assert.match(container.innerHTML, /Compaction failed/);
});

test('no failure notice element is ever synthesized by the client, for success, failure, or silence', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  const messages = [
    { role: 'user', content: '/compact', id: 'u1' },
    { role: 'system', content: 'Context compacted (manual)', kind: 'context_compacted', id: 'c1' },
    { role: 'separator', id: 's1' },
    { role: 'user', content: '/compact', id: 'u2' },
    { role: 'system', content: 'Compaction failed', kind: 'context_compact_failed', id: 'c2' },
    { role: 'separator', id: 's2' },
    { role: 'user', content: '/compact', id: 'u3' },
    { role: 'separator', id: 's3' },
  ];

  context.renderMessagesIntoContainer(container, messages, 'session-a');
  context._commitMessage({ role: 'separator', id: 's4' });

  assert.doesNotMatch(container.innerHTML, /compact-failed-notice/);
});
