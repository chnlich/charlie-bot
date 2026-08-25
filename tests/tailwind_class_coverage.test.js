// ---------------------------------------------------------------------------
// Coverage boundary: this test only asserts that Tailwind utility classes
// produced by the render paths the fixture below actually exercises --
// renderMessage() (user/assistant/tool-activity/system/delegate/worker/plan/
// clone/trigger/separator), the workers-tab card renderer, the backlog card
// renderer, and the plan compact-card builder -- are present in the committed
// web/static/css/tailwind.css. It says nothing about class usage in code
// paths (or templates) the fixture does not touch.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const { execFileSync } = require('node:child_process');

const ROOT = path.join(__dirname, '..');
const NODE_BIN_DIR = path.join(os.homedir(), '.local', 'nodeenvs', 'charliebot-node-20', 'bin');
const BUILD_ENV = Object.assign({}, process.env, {
  PATH: `${NODE_BIN_DIR}:${process.env.PATH || ''}`,
});

function readStatic(relativePath) {
  return fs.readFileSync(path.join(ROOT, 'web', 'static', 'js', relativePath), 'utf8');
}

const { FakeElement } = require('./fake_dom');
const { escapeHtml } = require('./escape_html_stub');

function makeDocument(elements) {
  return {
    getElementById(id) { return elements.has(id) ? elements.get(id) : null; },
    createElement(tag) { return new FakeElement(tag); },
    querySelector() { return null; },
    querySelectorAll() { return []; },
  };
}

// ---------------------------------------------------------------------------
// Loaders for each render surface, mirroring the vm bootstrap pattern used by
// tests/compact_button.test.js and tests/plan_cards.test.js.
// ---------------------------------------------------------------------------
function loadChatContext(elements) {
  const context = {
    document: makeDocument(elements),
    console: { error: () => {}, log: () => {} },
    marked: { parse: (v) => '<p>' + String(v || '') + '</p>' },
    fixNestedFences: (v) => String(v || ''),
    renderChatMath: () => {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    confirm: () => true,
    fetch: () => Promise.resolve({ ok: true }),
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  vm.runInContext(readStatic('chat/attachments.js'), context, { filename: 'chat/attachments.js' });
  context.Chat.renderRoundRatingButtons = () => '<button class="round-rating-btn text-slate-500 hover:text-green-400"></button>';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(readStatic('chat/rendering.js'), context, { filename: 'chat/rendering.js' });
  return context;
}

function loadWorkersContext(elements) {
  const context = {
    document: makeDocument(elements),
    console: { error: () => {} },
    fetch: () => Promise.resolve({ ok: true }),
    SESSION_ID: 'sess-1',
    BACKEND_OPTIONS: { 'claude-sonnet-5': 'Sonnet 5', 'codex-o3': 'Codex o3' },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/workers.js'), context, { filename: 'sidebar/workers.js' });
  return context;
}

function loadBacklogContext(elements, fetchImpl) {
  const context = {
    document: makeDocument(elements),
    console: { error: () => {} },
    fetch: fetchImpl,
  };
  vm.createContext(context);
  // backlog-panel.js declares `const backlogPanel = (() => {...})();` at top
  // level -- a `const` binding is not a property of the vm context object, so
  // bridge it onto the context in the same script (same lexical scope).
  vm.runInContext(
      readStatic('backlog-panel.js') + '\nglobalThis.backlogPanel = backlogPanel;',
      context,
      { filename: 'backlog-panel.js' });
  return context;
}

function loadArtifactsScript() {
  const context = {
    SESSION_ID: 'sess-1',
    escapeHtml,
    hljs: { highlight: (value) => ({ value: escapeHtml(value) }) },
    localStorage: { getItem: () => null, setItem: () => {} },
    window: { addEventListener: () => {}, SESSIONS_ROOT: '/home/user/.charliebot/sessions' },
    console,
    URL: globalThis.URL,
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/artifacts.js'), context, { filename: 'artifacts.js' });
  return context;
}

// ---------------------------------------------------------------------------
// Class-token extraction and Tailwind build helpers.
// ---------------------------------------------------------------------------
function extractClassTokens(snippets) {
  const tokens = new Set();
  const classAttrRe = /\bclass="([^"]*)"/g;
  for (const html of snippets) {
    let m;
    while ((m = classAttrRe.exec(String(html)))) {
      m[1].split(/\s+/).filter(Boolean).forEach((t) => tokens.add(t));
    }
  }
  return tokens;
}

function buildTailwindCssFromTokens(tokens) {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tw-coverage-'));
  const contentFile = path.join(tmpDir, 'fixture.html');
  const configFile = path.join(tmpDir, 'tailwind.config.js');
  const entryFile = path.join(tmpDir, 'entry.css');
  const outFile = path.join(tmpDir, 'out.css');

  fs.writeFileSync(contentFile, `<div class="${Array.from(tokens).join(' ')}"></div>\n`);
  fs.writeFileSync(configFile, `module.exports = {
  content: ['${contentFile}'],
  darkMode: 'class',
  theme: { extend: { colors: { slate: { 850: '#172033' } } } },
};
`);
  fs.writeFileSync(entryFile, '@tailwind base;\n@tailwind components;\n@tailwind utilities;\n');

  // Ensure the pinned tailwindcss devDependency is installed (idempotent, and
  // independent of whether tests/test_tailwind_css_build.py has already run).
  execFileSync('npm', ['install', '--no-audit', '--no-fund'], { cwd: ROOT, env: BUILD_ENV, stdio: 'pipe' });

  const cliPath = path.join(ROOT, 'node_modules', 'tailwindcss', 'lib', 'cli.js');
  execFileSync('node', [cliPath, '--config', configFile, '--input', entryFile, '--output', outFile],
      { cwd: ROOT, env: BUILD_ENV, stdio: 'pipe' });

  const css = fs.readFileSync(outFile, 'utf8');
  fs.rmSync(tmpDir, { recursive: true, force: true });
  return css;
}

// ---------------------------------------------------------------------------
// Rule-level subset check: every leaf rule (selector + declarations) in the
// coverage build must appear, modulo whitespace, in the committed CSS.
// ---------------------------------------------------------------------------
function extractLeafRules(css) {
  const rules = [];
  let i = 0;
  while (i < css.length) {
    const braceIdx = css.indexOf('{', i);
    if (braceIdx === -1) break;
    let depth = 1;
    let j = braceIdx + 1;
    while (j < css.length && depth > 0) {
      if (css[j] === '{') depth++;
      else if (css[j] === '}') depth--;
      j++;
    }
    const selector = css.slice(i, braceIdx).trim();
    const body = css.slice(braceIdx + 1, j - 1);
    if (selector.startsWith('@media') || selector.startsWith('@supports')) {
      rules.push(...extractLeafRules(body));
    } else if (selector) {
      rules.push(`${selector}{${body}}`);
    }
    i = j;
  }
  return rules;
}

function normalizeCss(s) {
  return s.replace(/\s+/g, '');
}

function findMissingRules(coverageCss, committedCss) {
  const normalizedCommitted = normalizeCss(committedCss);
  return extractLeafRules(coverageCss)
      .map((rule) => ({ rule, normalized: normalizeCss(rule) }))
      .filter(({ normalized }) => normalized && !normalizedCommitted.includes(normalized))
      .map(({ rule }) => rule);
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------
const LONG_BASH_COMMAND = 'find . -name "*.py" -exec grep -l "def render_message_with_a_very_long_command_line_to_trigger_truncation" {} \\;';
const LONG_TOOL_OUTPUT = Array.from({ length: 30 }, (_, i) => `line ${i}: some tool output text here`).join('\n');

const CHAT_MESSAGES = [
  {
    role: 'user', id: 'u1', timestamp: '2026-07-30T12:00:00Z', is_voice: true,
    content: 'Please check the build script', uploaded_files: [{ filename: 'notes.txt', path: '/tmp/notes.txt' }],
  },
  {
    role: 'assistant', id: 'a1', timestamp: '2026-07-30T12:00:05Z',
    content: 'Sure, looking now.', thinking: 'Let me check the repo layout first.',
    tools: [
      { name: 'Bash', input: { command: LONG_BASH_COMMAND }, output: LONG_TOOL_OUTPUT, is_error: false },
      { name: 'Read', input: { file_path: '/repo/scripts/build-css.sh' } },
      { name: 'Grep', input: { pattern: 'tailwind', path: 'web' }, output: 'no matches', is_error: true },
    ],
  },
  { role: 'assistant', id: 'a2', timestamp: '2026-07-30T12:00:06Z', content: '<tool_call>raw protocol text</tool_call>' },
  { role: 'system', id: 's1', timestamp: '2026-07-30T12:00:07Z', content: 'Context compacted (auto)', kind: 'context_compacted' },
  {
    role: 'task_delegated', id: 't1', timestamp: '2026-07-30T12:00:08Z', thread_id: 'th-1',
    delegate_invocation: {
      task_type: 'implement', repo_path: '/repo', base_branch: 'main',
      task_spec_file: '/tmp/spec.md', reviewer_context_file: null, backend: 'claude', keep_worktree: false,
    },
  },
  { role: 'worker_summary', id: 'w1', timestamp: '2026-07-30T12:00:09Z', content: 'Worker finished the task.' },
  { role: 'worker_summary', id: 'w2', timestamp: '2026-07-30T12:00:10Z', content: 'Worker ran elsewhere.', thread_id: 'th-2', origin_session_id: 'parent-1' },
  { role: 'plan', id: 'p1', timestamp: '2026-07-30T12:00:10Z', content: 'Step 1. Step 2.' },
  { role: 'clone_start', id: 'c1', content: 'parent session', parent_session_id: 'parent-1' },
  { role: 'scheduled_trigger', id: 'st1', timestamp: '2026-07-30T12:00:11Z', content: 'Trigger fired' },
  { role: 'separator', id: 'sep1', thinking_seconds: 12, event_index: 3 },
];

const WORKER_THREADS = [
  { id: 'th-1', description: 'Implement the tailwind build', status: 'running', created_at: '2026-07-30T12:00:00Z', backend: 'claude-sonnet-5' },
  { id: 'th-2', description: 'Fix flaky test', status: 'completed', created_at: '2026-07-30T11:00:00Z', completed_at: '2026-07-30T11:05:00Z', backend: 'codex-o3' },
  { id: 'th-3', description: 'Investigate scroll jank', status: 'failed', created_at: '2026-07-30T10:00:00Z', completed_at: '2026-07-30T10:05:00Z' },
];

const WORKER_TRIGGERS = [
  { id: 'tr-1', message: 'remind me later', status: 'pending', fire_at: '2026-08-01T20:00:00Z', created_at: '2026-07-30T12:00:00Z' },
  { id: 'tr-2', message: 'already fired', status: 'fired', fire_at: '2026-07-30T09:00:00Z', created_at: '2026-07-30T08:00:00Z' },
  { id: 'tr-3', message: 'cancelled one', status: 'cancelled', fire_at: '2026-07-30T09:00:00Z', created_at: '2026-07-30T08:00:00Z' },
];

const BACKLOG_ITEMS = [
  { id: 1, title: 'Pending idea', description: 'desc', priority: 'high', category: 'feature', status: 'pending', created: '2026-07-30T00:00:00Z', _source: 'alpha-lab-core' },
  { id: 2, title: 'Needs revision', description: 'desc', priority: 'medium', category: 'infra', status: 'revision_requested', revision_feedback: 'please clarify', created: '2026-07-29T00:00:00Z', _source: 'alpha-lab-core' },
  { id: 3, title: 'Approved idea', description: 'desc', priority: 'low', category: 'data', status: 'approved', created: '2026-07-28T00:00:00Z' },
  { id: 4, title: 'Rejected idea', description: 'desc', priority: 'high', category: 'strategy', status: 'rejected', rejected_reason: 'not now', created: '2026-07-27T00:00:00Z' },
  { id: 5, title: 'Failed idea', description: 'desc', priority: 'medium', category: 'backtest', status: 'failed', failed_reason: 'crashed', failed_count: 2, created: '2026-07-26T00:00:00Z' },
  { id: 6, title: 'Done idea', description: 'desc', priority: 'low', category: 'feature', status: 'done', created: '2026-07-25T00:00:00Z' },
];

const BACKLOG_HISTORY = [
  { idea_id: 6, backtest_result: { sharpe: { before: 1.1, after: 1.4 } } },
];

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------
test('tailwind utility classes used by rendered messages/cards are all present in the committed build', async () => {
  const snippets = [];

  // 1. Chat messages via renderMessage(), including tool-activity rendering.
  const chatCtx = loadChatContext(new Map());
  for (const msg of CHAT_MESSAGES) snippets.push(chatCtx.renderMessage(msg, 'sess-1'));

  // 2. Workers tab: thread + trigger cards.
  const workersContainer = new FakeElement('DIV');
  const workersCtx = loadWorkersContext(new Map([['tab-workers', workersContainer]]));
  workersCtx.renderWorkersTab(WORKER_THREADS, 'sess-1', WORKER_TRIGGERS);
  snippets.push(workersContainer.innerHTML);

  // 3. Backlog cards, across every status branch.
  const backlogList = new FakeElement('DIV');
  const backlogElements = new Map([
    ['backlog-list', backlogList],
    ['backlog-filter', { value: 'all' }],
    ['backlog-module-filter', { value: 'all' }],
  ]);
  const fetchImpl = (url) => {
    if (String(url).startsWith('/api/backlog/repos')) return Promise.resolve({ ok: true, json: async () => [] });
    if (String(url).startsWith('/api/backlog/history')) return Promise.resolve({ ok: true, json: async () => BACKLOG_HISTORY });
    return Promise.resolve({ ok: true, json: async () => BACKLOG_ITEMS });
  };
  const backlogCtx = loadBacklogContext(backlogElements, fetchImpl);
  await backlogCtx.backlogPanel.refresh();
  snippets.push(backlogList.innerHTML);

  // 4. Plan compact card, in both a pending and an approved-with-takeoff state.
  const artifactsCtx = loadArtifactsScript();
  snippets.push(artifactsCtx.buildPlanCompactCardHtml(1, 1, 'Remove the Play CDN', 'awaiting approval', '/abs/plan_01.html'));
  snippets.push(artifactsCtx.buildPlanCompactCardHtml(2, 3, 'Follow-up plan', 'approved · v3', '/abs/plan_02.html'));

  const tokens = extractClassTokens(snippets);
  assert.ok(tokens.size > 50, `fixture should exercise a healthy number of distinct class tokens (got ${tokens.size})`);

  const coverageCss = buildTailwindCssFromTokens(tokens);
  const committedCss = fs.readFileSync(path.join(ROOT, 'web', 'static', 'css', 'tailwind.css'), 'utf8');
  const missing = findMissingRules(coverageCss, committedCss);

  assert.deepEqual(missing, [],
      `Tailwind rules used by rendered output are missing from the committed CSS ` +
      `(rerun scripts/build-css.sh?):\n${missing.join('\n')}`);
});
