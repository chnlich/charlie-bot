const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ROOT = path.join(__dirname, '..');

function readStatic(relativePath) {
  return fs.readFileSync(path.join(ROOT, 'web', 'static', 'js', relativePath), 'utf8');
}

function escapeHtml(value) {
  return String(value || '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

function fakeElement() {
  let text = '';
  return {
    children: [],
    dataset: {},
    style: {},
    classList: {toggle() {}, contains() { return false; }},
    appendChild(child) {
      this.children.push(child);
      return child;
    },
    prepend(child) {
      this.children.unshift(child);
      return child;
    },
    querySelectorAll() {
      return [];
    },
    remove() {},
    get textContent() {
      return text;
    },
    set textContent(value) {
      text = String(value || '');
      this.innerHTML = escapeHtml(text);
    },
    innerHTML: '',
  };
}

function loadChatRendering() {
  const context = {
    CSS: {escape: (value) => String(value)},
    document: {
      createElement: () => fakeElement(),
      getElementById: () => null,
      querySelector: () => null,
    },
    marked: {parse: (value) => String(value || '')},
    fixNestedFences: (value) => String(value || ''),
    renderChatMath: () => {},
    renderUserMessageBubble: () => '',
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, {filename: 'chat/namespace.js'});
  vm.runInContext(readStatic('chat/shared.js'), context, {filename: 'chat/shared.js'});
  context.Chat.renderRoundRatingButtons = () => '';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(readStatic('chat/rendering.js'), context, {filename: 'chat/rendering.js'});
  return context;
}

function loadSidebarWorkers(elements) {
  const context = {
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: {},
    document: {
      createElement: () => fakeElement(),
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: () => [],
    },
    escapeHtml,
    fetch: () => Promise.resolve({ok: true}),
    loadedThreads: {delete() {}},
    stopThreadPoll: () => {},
    fetchAndRenderEvents: () => Promise.resolve(),
    console: {warn: () => {}, error: () => {}},
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, {filename: 'sidebar/namespace.js'});
  vm.runInContext(readStatic('sidebar/workers.js'), context, {filename: 'sidebar/workers.js'});
  return context;
}

test('task_delegated renders CLI-style metadata without full task spec', () => {
  const context = loadChatRendering();
  const fullTaskSpec = '## Goal\nSecret implementation details that belong in Workers.';

  const html = context.renderMessage({
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'thread-123',
    description: fullTaskSpec,
    backend: 'codex-o3',
    model: 'o3',
    delegate_invocation: {
      task_type: 'implement',
      repo_path: '/tmp/repo',
      base_branch: 'main',
      task_spec_file: '/tmp/task.md',
      reviewer_context_file: '/tmp/reviewer.md',
      keep_worktree: true,
      backend: 'codex-o3',
    },
  }, 'session-a');

  assert.match(html, /Delegated/);
  assert.match(html, /task type/);
  assert.match(html, /implement/);
  assert.match(html, /thread-123/);
  assert.match(html, /\/tmp\/repo/);
  assert.match(html, /\/tmp\/task\.md/);
  assert.match(html, /codex-o3 \/ o3/);
  assert.match(html, /keep worktree/);
  assert.doesNotMatch(html, /Secret implementation details/);
});

test('task_delegated verify metadata renders explicit none for repo and base', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'task_delegated',
    thread_id: 'verify-thread',
    delegate_invocation: {
      task_type: 'verify',
      repo_path: null,
      base_branch: null,
      task_spec_file: '/tmp/verify.md',
      reviewer_context_file: null,
      keep_worktree: false,
      backend: null,
    },
  }, 'session-a');

  assert.match(html, /verify/);
  assert.match(html, /repo[\s\S]*\(none\)/);
  assert.match(html, /base[\s\S]*\(none\)/);
});

test('historical task_delegated fallback omits full description', () => {
  const context = loadChatRendering();
  const fullTaskSpec = '## Goal\nOld full task spec should not render.';

  const html = context.renderMessage({
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'old-thread',
    description: fullTaskSpec,
    backend: 'codex-o3',
    model: 'o3',
  }, 'session-a');

  assert.match(html, /old-thread/);
  assert.match(html, /codex-o3 \/ o3/);
  assert.match(html, /Workers panel/);
  assert.doesNotMatch(html, /Old full task spec/);
});

test('worker_summary renders non-clickable locator without worker result content', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'worker_summary',
    content: 'Worker `12345678` | thread `12345678-full` | status: completed | time: 2026-07-01 12:34 PDT | find in Workers panel by thread ID',
    full_content: 'Large worker result body',
  }, 'session-a');

  assert.match(html, /12345678-full/);
  assert.doesNotMatch(html, /Worker Result/);
  assert.doesNotMatch(html, /showTextModal/);
  assert.doesNotMatch(html, /data-full/);
  assert.doesNotMatch(html, /Large worker result body/);
});

test('workers sidebar escapes full descriptions in initial and live cards', () => {
  const container = fakeElement();
  const elements = new Map([['tab-workers', container]]);
  const context = loadSidebarWorkers(elements);
  const description = 'Quote "double" and \'single\' <tag>';

  context.renderWorkersTab([{
    id: 'thread-a',
    status: 'running',
    description,
    created_at: '2026-07-01T12:00:00Z',
  }], 'session-a', []);

  assert.match(container.innerHTML, /data-full="Quote &quot;double&quot; and &#39;single&#39; &lt;tag&gt;"/);

  context.addWorkerCard('thread-b', description, '2026-07-01T12:00:00Z', '');

  assert.match(container.children[0].innerHTML, /data-full="Quote &quot;double&quot; and &#39;single&#39; &lt;tag&gt;"/);
});
