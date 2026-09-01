const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { escapeHtml } = require('./escape_html_stub');
const {loadChatRenderingModules} = require('./chat_rendering_context_stub');

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
  loadChatRenderingModules(context);
  return context;
}

function assertWellFormedMarkup(html, label = 'html') {
  assert.doesNotMatch(html, /<[^>]*</, `${label} contains a nested tag opener`);

  const tags = html.match(/<[^<>]*>/g) || [];
  assert.equal(tags.length, (html.match(/</g) || []).length, `${label} contains an unterminated tag`);

  const counts = new Map();
  for (const tagText of tags) {
    const match = /^<\/?\s*([A-Za-z][A-Za-z0-9:-]*)\b/.exec(tagText);
    assert.ok(match, `${label} contains an unparseable tag: ${tagText}`);

    const tagName = match[1].toLowerCase();
    if (/\/\s*>$/.test(tagText)) continue;

    const count = counts.get(tagName) || {opening: 0, closing: 0};
    if (tagText.startsWith('</')) count.closing += 1;
    else count.opening += 1;
    counts.set(tagName, count);
  }

  for (const [tagName, count] of counts) {
    assert.equal(count.opening, count.closing, `${label} has unbalanced <${tagName}> tags`);
  }
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
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
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

test('assistant OpenCode protocol renders complete collapsed literal output without Markdown parsing', () => {
  const context = loadChatRendering();
  const parseCalls = [];
  context.marked.parse = (value) => {
    parseCalls.push(value);
    return '<p>unexpected Markdown</p>';
  };
  const prefix = '<tool_call>read</tool_call>\n<function_results>';
  const suffix = '</function_results>';
  const content = prefix + 'x'.repeat(100373 - prefix.length - suffix.length) + suffix;

  const html = context.renderMessage({role: 'assistant', content}, 'session-a');

  assert.equal(content.length, 100373);
  assert.deepEqual(parseCalls, []);
  const detailsTag = html.match(/<details[^>]*>/);
  assert.ok(detailsTag);
  assert.doesNotMatch(detailsTag[0], /\sopen(?:\s|=|>)/);
  assert.match(html, /Raw backend output/);
  assert.match(html, /100373 characters/);
  assert.match(html, /<pre style="max-height:24rem;overflow:auto">/);
  assert.match(html, /<code data-embedded="1">/);
  assert.match(html, /<button class="copy-btn" onclick="copyCode\(this\)">Copy<\/button>/);
  assert.match(html, /&lt;tool_call&gt;/);
  assert.match(html, /&lt;function_results&gt;/);
  assert.doesNotMatch(html, /<tool_call>/);
  assert.doesNotMatch(html, /<function_results>/);

  const literal = html.match(/<pre[^>]*><code[^>]*>([\s\S]*)<\/code><\/pre>/);
  assert.ok(literal);
  assert.equal(literal[1], escapeHtml(content));
  assertWellFormedMarkup(html);
});

test('assistant OpenCode protocol detection accepts leading whitespace', () => {
  const context = loadChatRendering();
  const parseCalls = [];
  context.marked.parse = (value) => {
    parseCalls.push(value);
    return '<p>unexpected Markdown</p>';
  };
  const content = '\n \t<tool_call>read</tool_call>';

  const html = context.renderMessage({role: 'assistant', content}, 'session-a');

  assert.deepEqual(parseCalls, []);
  assert.match(html, /Raw backend output/);
  const literal = html.match(/<pre[^>]*><code[^>]*>([\s\S]*)<\/code><\/pre>/);
  assert.ok(literal);
  assert.equal(literal[1], escapeHtml(content));
});

test('ordinary assistant Markdown keeps the existing marked.parse path', () => {
  const context = loadChatRendering();
  const parseCalls = [];
  context.marked.parse = (value) => {
    parseCalls.push(value);
    return '<p><strong>ordinary</strong></p>';
  };
  const content = '**ordinary**';

  const html = context.renderMessage({role: 'assistant', content}, 'session-a');

  assert.deepEqual(parseCalls, [content]);
  assert.match(html, /<p><strong>ordinary<\/strong><\/p>/);
  assert.doesNotMatch(html, /Raw backend output/);
});

test('raw assistant output retains thinking and representative structured tools', () => {
  const context = loadChatRendering();
  const html = context.renderMessage({
    role: 'assistant',
    content: '<tool_call>read</tool_call>',
    thinking: 'Inspecting <protocol>',
    tools: [
      {name: 'Bash', input: {command: 'pwd'}},
      {name: 'Read', input: {file_path: '/tmp/input.txt'}},
      {name: 'Edit', input: {file_path: '/tmp/output.txt'}},
      {name: 'Glob', input: {pattern: '*.js'}},
      {name: 'Grep', input: {pattern: 'needle', path: '/tmp'}},
    ],
  }, 'session-a');

  assert.match(html, /Thinking…/);
  assert.match(html, /Inspecting &lt;protocol&gt;/);
  assert.match(html, /5 tool calls/);
  for (const toolName of ['Bash', 'Read', 'Edit', 'Glob', 'Grep']) {
    assert.match(html, new RegExp('>' + toolName + '<'));
  }
  assert.match(html, /Raw backend output/);
});

test('renderMessage returns well-formed markup for every role branch', () => {
  const context = loadChatRendering();

  const messages = [
    {
      role: 'user',
      content: 'Please inspect the report.',
      timestamp: '2026-07-01T12:30:00Z',
      uploaded_files: [{filename: 'report.pdf', path: '/tmp/report.pdf'}],
    },
    {
      role: 'assistant',
      content: 'Done.',
      timestamp: '2026-07-01T12:31:00Z',
    },
    {
      role: 'system',
      content: 'Session resumed',
      timestamp: '2026-07-01T12:32:00Z',
    },
    {
      role: 'task_delegated',
      content: 'Task delegated',
      thread_id: 'thread-123',
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
      timestamp: '2026-07-01T12:33:00Z',
    },
    {
      role: 'worker_summary',
      content: 'Worker `12345678` | thread `12345678-full` | status: completed',
      timestamp: '2026-07-01T12:34:00Z',
    },
    {
      role: 'plan',
      content: '1. Inspect\n2. Fix',
      timestamp: '2026-07-01T12:35:00Z',
    },
    {
      role: 'clone_start',
      content: 'Parent & Session',
      parent_session_id: 'parent/session?tab=chat',
    },
    {
      role: 'scheduled_trigger',
      content: '[Scheduled trigger fired | timeout] Check PID 12345 (finished: 12345 (gone at start), host:6789, slurm:91038: COMPLETED 0:0; still alive: slurm:91039: RUNNING)',
      timestamp: '2026-07-01T12:36:00Z',
    },
    {
      role: 'separator',
      thinking_seconds: 12,
      event_index: 4,
    },
  ];

  for (const msg of messages) {
    assertWellFormedMarkup(context.renderMessage(msg, 'session-a'), msg.role);
  }
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
