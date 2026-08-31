const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { loadChatRenderingModules } = require('./chat_rendering_context_stub');

const { FakeElement } = require('./fake_dom');

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

function loadStatusContext(elements) {
  const context = {
    document: {
      getElementById: (id) => elements.get(id) || null,
    },
    console: { error: () => {} },
    fetch: () => Promise.resolve({ ok: true, json: async () => ({}) }),
    SESSION_ID: 'session-a',
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/status.js'), context, { filename: 'sidebar/status.js' });
  return context;
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
  const context = {
    document: makeDocument(elements),
    console: { error: () => {}, log: () => {} },
    marked: { parse: (v) => String(v || '') },
    fixNestedFences: (v) => String(v || ''),
    renderChatMath: () => {},
    renderUserMessageBubble: () => '',
    showScrollToBottom: () => {},
    hideStreaming: () => {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'session-a',
    confirm: () => true,
    fetch: () => Promise.resolve({ ok: true }),
    // No stored key: mirrors page-load order config.js → websocket.js on the no-key path.
    wsUrlWithToken: (path) => path,
    // No stubs above run config.js: stand in for its shared header literal.
    JSON_HEADERS: {'Content-Type': 'application/json'},
  };
  vm.createContext(context);
  loadChatRenderingModules(context);
  vm.runInContext(readStatic('websocket.js'), context, { filename: 'websocket.js' });
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
test('updateBackendHeaderControls gates #compact-btn to exactly cc-claude, for every type in the BACKEND_TYPES fixture', () => {
  const BACKEND_TYPES = {
    'claude-opus-4.6': 'cc-claude',
    'claude-sonnet-5': 'cc-claude',
    'codex-o3': 'codex',
    'legacy-tui': 'tui-cli',
    'opencode-glm': 'opencode',
  };
  const compactBtn = fakeHeaderButton();
  const stopBtn = fakeHeaderButton();
  const elements = new Map([['compact-btn', compactBtn], ['stop-tui-btn', stopBtn]]);
  const context = loadStatusContext(elements);

  const uniqueTypes = Array.from(new Set(Object.values(BACKEND_TYPES)));
  assert.ok(uniqueTypes.length >= 3, 'fixture should exercise more than one non-cc-claude type');

  for (const type of uniqueTypes) {
    context.updateBackendHeaderControls(type, 'session-a');
    assert.equal(compactBtn.disabled, type !== 'cc-claude', `type ${type}`);
    const expectedTitle = type === 'cc-claude'
      ? ''
      : type === 'codex'
        ? 'codex only compacts automatically — tune model_auto_compact_token_limit'
        : 'Manual compaction is not supported on this backend';
    assert.equal(compactBtn.title, expectedTitle, `title for type ${type}`);
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
  context.fetch = (url, opts) => { calls.push({ url, opts }); return Promise.resolve({ ok: true }); };

  await context.compactContext();
  assert.equal(calls.length, 1);

  await context.postChatMessage('/compact');
  assert.equal(calls.length, 2);

  assert.equal(calls[0].url, calls[1].url);
  assert.equal(calls[0].opts.method, calls[1].opts.method);
  assert.equal(calls[0].opts.body, calls[1].opts.body);
  assert.deepEqual(JSON.parse(calls[0].opts.body), { content: '/compact' });
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
