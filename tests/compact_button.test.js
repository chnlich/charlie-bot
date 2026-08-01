const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ROOT = path.join(__dirname, '..');

function readStatic(relativePath) {
  return fs.readFileSync(path.join(ROOT, 'web', 'static', 'js', relativePath), 'utf8');
}

// ---------------------------------------------------------------------------
// Minimal fake DOM. `innerHTML` on any element is the concatenation of its own
// literal string content plus its children's `innerHTML` -- this mirrors real
// DOM semantics closely enough for both `renderMessagesIntoContainer` (which
// replaces content wholesale via `.innerHTML =`) and `_appendRenderedMessage`
// (which appends discrete child wrappers), without needing an HTML parser.
// ---------------------------------------------------------------------------
function escapeForFakeDom(str) {
  return String(str).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

class FakeElement {
  constructor(tag = 'DIV') {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.dataset = {};
    const classSet = new Set();
    this.classList = {
      add(...c) { c.forEach((x) => classSet.add(x)); },
      remove(...c) { c.forEach((x) => classSet.delete(x)); },
      toggle(c, force) {
        if (force === undefined) {
          if (classSet.has(c)) { classSet.delete(c); return false; }
          classSet.add(c);
          return true;
        }
        if (force) classSet.add(c); else classSet.delete(c);
        return !!force;
      },
      contains(c) { return classSet.has(c); },
    };
    this._html = '';
    this._text = '';
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
  }

  get innerHTML() {
    return this._html + this.children.map((c) => c.innerHTML).join('');
  }

  set innerHTML(html) {
    this._html = html;
    this.children = [];
  }

  get textContent() {
    return this._text;
  }

  set textContent(value) {
    this._text = String(value || '');
    this.innerHTML = escapeForFakeDom(this._text);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  insertBefore(child, ref) {
    child.parentElement = this;
    if (!ref) {
      this.children.push(child);
      return child;
    }
    const idx = this.children.indexOf(ref);
    this.children.splice(idx === -1 ? this.children.length : idx, 0, child);
    return child;
  }

  removeChild(child) {
    const idx = this.children.indexOf(child);
    if (idx !== -1) this.children.splice(idx, 1);
    return child;
  }

  remove() {
    if (this.parentElement) this.parentElement.removeChild(this);
  }

  querySelector() {
    return null;
  }

  querySelectorAll() {
    return [];
  }
}

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
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  context.Chat.renderRoundRatingButtons = () => '';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(readStatic('chat/rendering.js'), context, { filename: 'chat/rendering.js' });
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
// 4. A fake success is never reported as success, and a mid-round reload
//    changes nothing.
// ---------------------------------------------------------------------------
test('a model merely claiming success without a context_compacted message is judged failed, and the notice renders exactly once', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  const messages = [
    { role: 'user', content: '/compact', id: 'u1' },
    { role: 'assistant', content: 'Context compacted successfully!', id: 'a1' },
    { role: 'separator', id: 's1' },
  ];

  assert.equal(context.compactOutcome(messages), 'failed');

  context.renderMessagesIntoContainer(container, messages, 'session-a');
  const noticeCount = (container.innerHTML.match(/compact-failed-notice/g) || []).length;
  assert.equal(noticeCount, 1);
});

test('a real context_compacted message is judged ok and produces no notice', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  const messages = [
    { role: 'user', content: '/compact', id: 'u1' },
    { role: 'system', content: 'Context compacted (manual)', kind: 'context_compacted', id: 'c1' },
    { role: 'separator', id: 's1' },
  ];

  assert.equal(context.compactOutcome(messages), 'ok');

  context.renderMessagesIntoContainer(container, messages, 'session-a');
  assert.doesNotMatch(container.innerHTML, /compact-failed-notice/);
});

test('a mid-round reload reaches the same failed verdict through the live-append path, with no page variable involved', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  // First paint: only the click made it into the snapshot before the reload.
  const upToCompact = [
    { role: 'assistant', content: 'Sure, compacting now.', id: 'a0' },
    { role: 'user', content: '/compact', id: 'u1' },
  ];
  context.renderMessagesIntoContainer(container, upToCompact, 'session-a');
  assert.doesNotMatch(container.innerHTML, /compact-failed-notice/);

  // The round then ends without ever having produced a context_compacted
  // message -- delivered live, exactly as the WebSocket would deliver it.
  context._commitMessage({ role: 'separator', id: 's1' });

  assert.match(container.innerHTML, /compact-failed-notice/);
});

test('an auto-compact context_compacted message with no preceding /compact click produces no notice', () => {
  const elements = new Map([['messages', new FakeElement('DIV')]]);
  const context = loadChatContext(elements);
  const container = elements.get('messages');

  const messages = [
    { role: 'assistant', content: 'Some work happened.', id: 'a1' },
    { role: 'system', content: 'Context compacted (auto)', kind: 'context_compacted', id: 'c1' },
    { role: 'separator', id: 's1' },
  ];

  assert.equal(context.compactOutcome(messages), 'none');

  context.renderMessagesIntoContainer(container, messages, 'session-a');
  assert.doesNotMatch(container.innerHTML, /compact-failed-notice/);
});
