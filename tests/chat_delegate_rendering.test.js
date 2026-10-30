const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { escapeHtml } = require('./escape_html_stub');
const {createEscapingElement} = require('./dom_element_stub');
const {loadChatRendering} = require('./chat_rendering_context_stub');
const {readPackageStatic} = require('./read_static');

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
  assert.match(html, /reviewer context file/);
  assert.match(html, /\/tmp\/reviewer\.md/);
  assert.match(html, /codex-o3 \/ o3/);
  assert.match(html, /keep worktree/);
  assert.doesNotMatch(html, /Secret implementation details/);
});

test('a routed delegation renders its backend list and reason under the backend/model row', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'thread-9',
    backend: 'fake2',
    model: 'fake-model',
    route: {backends: ['fake2', 'fake'], reason: 'design p=0.9'},
  }, 'session-a');

  assert.match(html, /route/);
  assert.match(html, /fake2, fake/);
  assert.match(html, /design p=0\.9/);
});

test('an empty route list renders pool and the router reason', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'thread-9',
    backend: 'fake',
    model: 'fake-model',
    route: {backends: [], reason: 'routine p=0.12'},
  }, 'session-a');

  assert.match(html, /route<\/span><span[^>]*>pool &middot; routine p=0\.12<\/span>/);
});

test('a delegation without a route renders no route row', () => {
  const context = loadChatRendering();

  const message = {
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'thread-9',
    backend: 'fake',
    model: 'fake-model',
  };

  assert.doesNotMatch(context.renderMessage(message, 'session-a'), /route/);
  assert.doesNotMatch(context.renderMessage({...message, route: null}, 'session-a'), /route/);
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

test('task_delegated cold-read metadata renders explicit none for repo and base', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'task_delegated',
    thread_id: 'cold-read-thread',
    delegate_invocation: {
      task_type: 'cold-read',
      repo_path: null,
      base_branch: null,
      task_spec_file: null,
      reviewer_context_file: null,
      keep_worktree: false,
      backend: null,
    },
  }, 'session-a');

  assert.match(html, /cold-read/);
  assert.match(html, /repo[\s\S]*\(none\)/);
  assert.match(html, /base[\s\S]*\(none\)/);
});

test('a new-style delegation links its child session and shows the live state line', () => {
  const context = loadChatRendering();

  const html = context.renderMessage({
    role: 'task_delegated',
    content: 'Task delegated',
    thread_id: 'run-9',
    child_session_id: 'child-session-9',
    backend: 'claude-sonnet-5',
    delegate_invocation: {
      task_type: 'implement',
      repo_path: '/repo',
      base_branch: 'main',
      task_spec_file: '/tmp/spec.md',
      reviewer_context_file: null,
      keep_worktree: false,
      backend: 'claude-sonnet-5',
    },
  }, 'session-a');

  // The child link rides the status poll's scope; the dead panel pointer is gone.
  assert.match(html, /href="\/\?session=child-session-9"/);
  assert.match(html, /data-delegate-session="child-session-9"/);
  assert.doesNotMatch(html, /reviewer context file/);
  assert.doesNotMatch(html, /Workers panel/);
});

test('the delegated card live state paints from the status poll', () => {
  const stateEl = createEscapingElement('span');
  stateEl.dataset = {delegateSession: 'child-1', delegateBackend: 'Sonnet 5'};
  const document = {
    getElementById: () => null,
    querySelectorAll: (sel) => (sel.includes('child-1') ? [stateEl] : []),
  };
  const vm = require('node:vm');
  const {readStatic} = require('./read_static');
  const context = {
    document,
    console: {error() {}, warn() {}, log() {}},
    fetch: () => Promise.resolve({ok: true, json: async () => ({}), text: async () => ''}),
    localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
    location: {href: '', protocol: 'http:', host: 'localhost:8000', search: ''},
    history: {pushState() {}},
    URLSearchParams,
    SESSION_ID: 'sess-1',
    BACKEND_OPTIONS: {},
    BACKEND_TYPES: {},
    setInterval: () => 0,
    clearInterval: () => {},
    setTimeout: () => 0,
    clearTimeout: () => {},
  };
  context.globalThis = context;
  context.window = {addEventListener() {}};
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, {filename: 'sidebar/namespace.js'});
  vm.runInContext(readStatic('sidebar/status.js'), context, {filename: 'sidebar/status.js'});
  context.document = {
    getElementById: () => null,
    querySelectorAll: (sel) => (sel.includes('child-1') ? [stateEl] : []),
  };

  context.Sidebar.paintDelegateCardState('child-1', {has_running_tasks: true});
  assert.equal(stateEl.textContent, 'running \u00b7 Sonnet 5');
  context.Sidebar.paintDelegateCardState('child-1', {has_running_tasks: false});
  assert.equal(stateEl.textContent, 'idle \u00b7 Sonnet 5');
  // A settled task-tree child reads its work verdict: waiting reads 'queued',
  // anything else (a failed or finished Run reads idle) reads bare 'idle' —
  // no 'failed' verdict exists.
  context.Sidebar.paintDelegateCardState('child-1', {has_running_tasks: false, work_state: 'waiting'});
  assert.equal(stateEl.textContent, 'queued \u00b7 Sonnet 5');
  context.Sidebar.paintDelegateCardState('child-1', {has_running_tasks: false, work_state: 'idle'});
  assert.equal(stateEl.textContent, 'idle \u00b7 Sonnet 5');
});

test('a Run header reads its own state: queued in amber, failed in red with its error', () => {
  const context = loadChatRendering();
  const header = (state, extra) => context.renderMessage({
    role: 'system', kind: 'run_header', run_id: 'run-' + state,
    content: 'Run work \u00b7 Sonnet 5 \u00b7 ' + state, state, error: '', ...extra,
  }, 'sess-1');

  const queued = header('queued');
  assertWellFormedMarkup(queued, 'queued header');
  assert.match(queued, /id="run-header-run-queued" data-run-state="queued"/);
  assert.match(queued, /id="run-dot-run-queued" class="[^"]*bg-amber-400/);
  assert.match(queued, /Run work \u00b7 Sonnet 5 \u00b7 queued/);
  assert.doesNotMatch(queued, /run-error-/);

  const failed = header('failed', {error: 'RuntimeError: worktree <prep> failed'});
  assertWellFormedMarkup(failed, 'failed header');
  assert.match(failed, /id="run-dot-run-failed" class="[^"]*bg-red-500/);
  assert.match(failed, /id="run-error-run-failed"[^>]*>(?:<span[^>]*>)?RuntimeError: worktree &lt;prep&gt; failed</);

  const multiline = header('failed', {error: 'peer closed connection\nwhile streaming\nretry exhausted'});
  assert.match(multiline, /peer closed connection/);
  assert.match(multiline, /2 more lines/);
  assert.match(multiline, /while streaming[\s\S]*retry exhausted/);

  assert.match(header('running'), /id="run-dot-run-running" class="[^"]*bg-blue-500/);
  assert.match(header('success'), /id="run-dot-run-success" class="[^"]*bg-green-500/);
  assert.match(header('stopped'), /id="run-dot-run-stopped" class="[^"]*bg-slate-500/);
});

test('a run_end separator keeps its label while an ordinary separator keeps its button group and no text', () => {
  const context = loadChatRendering();
  context.Chat.registerSeparatorButton((_sessionId, _eventIndex) => '<button class="separator-action">Action</button>');

  const managerSeparator = context.renderMessage({
    role: 'separator', id: 'manager-end', event_index: 12, thinking_seconds: 45,
  }, 'sess-1');
  assert.doesNotMatch(managerSeparator, /response complete/);
  assert.match(managerSeparator, /class="separator-action"/);
  assert.match(managerSeparator, /data-round-event-index="12"/);
  assert.match(managerSeparator, /data-round-id="manager-end"/);
  // The ordinary row carries no label, so the phone rule in styles.css leaves its two
  // hairlines on the row; the run-end row below is the one labelled row.
  assert.doesNotMatch(managerSeparator, /\bseparator-label\b/);
  assert.equal((managerSeparator.match(/\bseparator-hairline\b/g) || []).length, 2);

  const runEnd = context.renderMessage({
    role: 'separator', kind: 'run_end', id: 'run-end', event_index: 13, thinking_seconds: 68,
  }, 'sess-1');
  assert.match(runEnd, /run ended &middot; 1m08s/);
  assert.match(runEnd, /<span class="[^"]*\bseparator-label\b/);
  assert.doesNotMatch(runEnd, /<button\b/);
});

test('run_error rows fold their later lines while manager error rows remain unchanged', () => {
  const context = loadChatRendering();
  const runError = context.renderMessage({
    role: 'system', kind: 'run_error', content: 'Error: first line\nsecond line\nthird line', line_count: 3,
  }, 'sess-1');
  assert.match(runError, /Error: first line/);
  assert.match(runError, /2 more lines/);
  assert.match(runError, /second line[\s\S]*third line/);

  const managerError = context.renderMessage({
    role: 'system', content: 'Error: first line\nsecond line\nthird line',
  }, 'sess-1');
  assert.match(managerError, /Error: first line\nsecond line\nthird line/);
  assert.doesNotMatch(managerError, /more lines/);
});

test('a Run header carries its own time, and a queued header carries none', () => {
  const context = loadChatRendering();
  const header = (extra) => context.renderMessage({
    role: 'system', kind: 'run_header', run_id: 'run-t',
    content: 'Run work \u00b7 Fake \u00b7 failed', state: 'failed', error: '', ...extra,
  }, 'sess-1');

  // A real time rides the wrapper's data-message-ts and the bubble title.
  const started = header({timestamp: '2026-09-26T18:17:00+00:00', id: 'm1'});
  assert.match(started, /data-message-ts="2026-09-26T18:17:00\+00:00"/);
  assert.match(started, /title="[^"]*"/);

  // No time (a queued Run, or the server withheld it): no bubble title and no
  // data-message-ts — never a fabricated page-load time.
  const timeless = header({id: 'm2'});
  assert.doesNotMatch(timeless, /title="/);
  assert.doesNotMatch(timeless, /data-message-ts/);
});

test('a launch-failed Run header shows the no-output-log note and the ref link row', () => {
  const context = loadChatRendering();
  const header = (extra) => context.renderMessage({
    role: 'system', kind: 'run_header', run_id: 'run-lf',
    content: 'Run work \u00b7 Sonnet 5 \u00b7 failed', state: 'failed', error: '',
    launched: false, launch_failed: false, task_spec_ref: '', launch_prompt_ref: '', ...extra,
  }, 'sess-1');

  // The note rides launch_failed only; a plain failed header has none.
  const launchFailed = header({launch_failed: true, error: 'RuntimeError: base check refused'});
  assertWellFormedMarkup(launchFailed, 'launch-failed header');
  assert.match(launchFailed, /id="run-note-run-lf"[^>]*>Worker never started, so this Run has no output log\.</);
  const plainFailed = header({error: 'RuntimeError: base check refused'});
  assertWellFormedMarkup(plainFailed, 'failed header');
  assert.doesNotMatch(plainFailed, /run-note-/);

  // Under the pill the order is note, error text, then the link row.
  const noteAt = launchFailed.indexOf('run-note-run-lf');
  const errorAt = launchFailed.indexOf('run-error-run-lf');
  const linksAt = launchFailed.indexOf('run-links-run-lf');
  assert.ok(noteAt < errorAt && errorAt < linksAt, 'note, error, then link row');

  // Both refs set: file-server links opening a new tab.
  const linked = header({
    launched: true,
    task_spec_ref: '/h/runs/run-lf/task_spec.md',
    launch_prompt_ref: '/h/runs/run-lf/launch_prompt.md',
  });
  assertWellFormedMarkup(linked, 'linked header');
  assert.match(
    linked,
    /<a class="text-blue-400 hover:underline" href="\/absolute_filepath\/h\/runs\/run-lf\/task_spec\.md" target="_blank" rel="noopener">Task spec<\/a>/);
  assert.match(
    linked,
    /<a class="text-blue-400 hover:underline" href="\/absolute_filepath\/h\/runs\/run-lf\/launch_prompt\.md" target="_blank" rel="noopener">Launch prompt<\/a>/);

  // The three gray placeholders: a task spec that was never recorded, a launch
  // prompt never assembled (the Run never started), and one a started Run has
  // no file for.
  const noRefs = header({launch_failed: true});
  assert.match(noRefs, /<span class="text-slate-500">Task spec \(not recorded\)<\/span>/);
  assert.match(noRefs, /<span class="text-slate-500">Launch prompt \(not assembled\)<\/span>/);
  const startedNoPrompt = header({launched: true});
  assert.match(startedNoPrompt, /<span class="text-slate-500">Task spec \(not recorded\)<\/span>/);
  assert.match(startedNoPrompt, /<span class="text-slate-500">Launch prompt \(not recorded\)<\/span>/);
});

test('run_delivery closes the transcript with the summary and the four evidence links', () => {
  const context = loadChatRendering();
  // The Diff link registers from the diff_view package.
  vm.runInContext(readPackageStatic('features', 'diff_view', 'delivery-diff-link.js'), context,
                  {filename: 'delivery-diff-link.js'});

  const delivered = context.renderMessage({
    role: 'run_delivery',
    content: 'shipped the parser',
    task_state: 'completed',
    completed: true,
    result_refs: ['evidence/a.txt'],
    raw_log_ref: '/h/runs/run-1/raw.log',
    events_ref: '/h/runs/run-1/events.jsonl',
    result_ref: '/h/runs/run-1/raw.log',
    repo_path: '/repo',
    base_branch: 'main',
    branch_name: 'task/run-1',
  }, 'sess-1');

  assert.match(delivered, /Delivered/);
  assert.match(delivered, /shipped the parser/);
  assert.match(delivered, /Raw log/);
  assert.match(delivered, /Events/);
  assert.match(delivered, /Result/);
  assert.match(delivered, /Diff/);
  // The file-server route carries host-absolute refs; the diff page compares
  // the run's work branch against its base.
  assert.match(delivered, /href="\/absolute_filepath\/h\/runs\/run-1\/raw\.log"/);
  assert.match(delivered, /\/diff\?repo=%2Frepo&amp;base=main&amp;head=task%2Frun-1&amp;session=sess-1/);
  assert.match(delivered, /evidence\/a\.txt/);

  const unresolved = context.renderMessage({
    role: 'run_delivery',
    content: '',
    task_state: 'failed',
    completed: false,
    result_refs: [],
  }, 'sess-1');
  assert.match(unresolved, /Task failed/);
  assert.match(unresolved, /text-slate-500">Raw log/);
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
  assert.doesNotMatch(html, /Workers panel/);
  assert.doesNotMatch(html, /Old full task spec/);
  // The legacy delegation renders no child link — its thread id no longer
  // resolves to a view — and the live-state span rides the owning session's
  // status poll.
  assert.doesNotMatch(html, /thread=old-thread/);
  assert.match(html, /data-delegate-parent-session="session-a"/);
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
  context.renderProseMarkdown = (value) => {
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
  context.renderProseMarkdown = (value) => {
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
  context.renderProseMarkdown = (value) => {
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
