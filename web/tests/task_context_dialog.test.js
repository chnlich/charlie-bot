// ---------------------------------------------------------------------------
// Task & context dialog data assembly (modals.js): opening the dialog for a
// session fetches its detail and its effective-prompt preview, renders the
// preview's task-scope block through the chat bubble's pipeline with data-raw
// carrying the block text, and lists one row per assembled block with its
// sources, delivery and measured length. It degrades to a note when the
// session is not a task node yet, when the preview is refused, and when the
// task record is empty.
// Harness follows sidebar_rename_prefill.test.js: namespace.js
// then modals.js in a vm context over the three dialog elements.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {createElement} = require('./dom_element_stub');
const {escapeHtmlText} = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const MODALS_JS = readStatic('sidebar/modals.js');

function jsonResponse(status, body) {
  return {ok: status >= 200 && status < 300, status, statusText: 'HTTP ' + status, json: async () => body};
}

// routes: url -> response, or a function returning a (possibly pending) response.
function buildHarness(routes) {
  const overlay = createElement({id: 'task-context-modal-overlay', className: 'hidden fixed'});
  const title = createElement({id: 'task-context-modal-title'});
  const body = createElement({id: 'task-context-modal-body'});
  const elements = new Map([[overlay.id, overlay], [title.id, title], [body.id, body]]);
  const calls = [];
  const proseCalls = [];
  const context = {
    Sidebar: {},
    Chat: {registerSeparatorButton: () => {}},
    globalThis: null,
    document: {getElementById: (id) => elements.get(id) || null},
    console: {error: () => {}},
    escapeHtml: escapeHtmlText,
    // The chat bubble's prose pipeline, recorded so tests can pin which text
    // the dialog hands to it. The stub models the one contract the dialog
    // relies on (the production renderer escapes raw HTML and turns ATX
    // headings into elements, so a literal '## ' never paints).
    renderProseMarkdown: (text) => {
      const raw = String(text);
      proseCalls.push(raw);
      return '<md>' + escapeHtmlText(raw).replace(/^(#{1,6}) (.*)$/gm, (_m, hashes, body) =>
        `<h${hashes.length}>${body}</h${hashes.length}>`) + '</md>';
    },
    fetch: async (url) => {
      calls.push(url);
      const route = routes[url];
      if (!route) throw new Error('unexpected fetch ' + url);
      return typeof route === 'function' ? route() : route;
    },
  };
  context.__proseCalls = proseCalls;
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  vm.runInContext(MODALS_JS, context, {filename: 'modals.js'});
  return {context, overlay, title, body, calls};
}

const DETAIL = {
  id: 's1', name: 'Auth rework', profile: 'manager',
  task: {
    goal: 'Migrate users to the new schema',
    acceptance: ['Old columns readable for one release', 'pytest green'],
    context_refs: ['docs/auth.md'],
  },
};

const TASK_BLOCK_TEXT = '# Task record\n\nMigrate users to the new schema\n\n## Acceptance\n- Old columns readable for one release\n- pytest green\n\n## Context refs\n- docs/auth.md';

const PREVIEW = {
  session_id: 's1', kind: 'manager_turn', mode: 'preview', overlay: {declared: true, error: null},
  blocks: [
    {sources: [{scope: 'base', source_ref: 'prompts/task_manager.md', source_session_id: null}],
      body_ref: 'b1', delivery: 'full', text: 'x'.repeat(1200)},
    {sources: [{scope: 'memory', source_ref: 'tooling/repo-layout', source_session_id: null},
      {scope: 'memory', source_ref: 'research/branch-flow', source_session_id: null}],
      body_ref: 'b2', delivery: 'index', text: 'y'.repeat(340)},
    {sources: [{scope: 'subtree', source_ref: 'rule:evidence', source_session_id: 'root-1'}],
      body_ref: 'b3', delivery: 'full', text: 'z'.repeat(62)},
    {sources: [{scope: 'task', source_ref: 'metadata.json#task', source_session_id: 's1'}],
      body_ref: 'b4', delivery: 'full', text: TASK_BLOCK_TEXT},
  ],
  prompt_hash: 'abc', char_count: 1602,
};

test('opening the dialog renders the task block through the prose pipeline and one row per assembled block', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });

  await h.context.openTaskContextModal('s1');

  assert.deepEqual(h.calls, ['/api/sessions/s1', '/api/sessions/s1/effective-prompt']);
  assert.equal(h.overlay.classList.contains('hidden'), false);
  assert.equal(h.overlay.classList.contains('flex'), true);
  assert.equal(h.title.textContent, 'Task & context · Auth rework');
  const html = h.body.innerHTML;
  assert.match(html, /<md><h1>Task record<\/h1>/, 'the block renders through the prose pipeline');
  assert.match(html, /Migrate users to the new schema/);
  assert.match(html, /- Old columns readable for one release\n- pytest green/, 'the acceptance items ride the block');
  assert.match(html, /## Context refs\n- docs\/auth\.md/);
  assert.match(html, /manager_turn · 1,602 chars/, 'the run kind and the measured total');
  assert.match(html, /base<\/span> prompts\/task_manager\.md[\s\S]*?>full<[\s\S]*?>1,200</);
  assert.match(html, /tooling\/repo-layout<br>[\s\S]*?research\/branch-flow[\s\S]*?>index<[\s\S]*?>340</);
  assert.match(html, /rule:evidence <span[^>]*>from root-1<\/span>[\s\S]*?>62</, 'a subtree rule names its owning node');
  assert.doesNotMatch(html, /Launch overlay unavailable|\(none\)/);
});

test('a preview without a task-scope block shows the empty-record note; an overlay error is named', async () => {
  const preview = {
    ...PREVIEW,
    blocks: PREVIEW.blocks.slice(0, 3),
    char_count: 1602 - TASK_BLOCK_TEXT.length,
    overlay: {declared: true, error: 'FileNotFoundError'},
  };
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, preview),
  });

  await h.context.openTaskContextModal('s1');

  const html = h.body.innerHTML;
  assert.match(html, /\(no task record: the goal, acceptance and context refs are all empty\)/);
  assert.match(html, /manager_turn · 1,455 chars/);
  assert.match(html, /Launch overlay unavailable: FileNotFoundError/) ;
});

test('a session without a profile shows the not-a-node note, with no preview request', async () => {
  const h = buildHarness({
    '/api/sessions/legacy': jsonResponse(200, {id: 'legacy', name: 'Old chat', profile: null, task: null}),
  });

  await h.context.openTaskContextModal('legacy');

  assert.deepEqual(h.calls, ['/api/sessions/legacy']);
  assert.equal(h.title.textContent, 'Task & context · Old chat');
  assert.match(h.body.innerHTML, /Not a task node yet/);
  assert.doesNotMatch(h.body.innerHTML, /Task record/);
});

test('a refused preview shows the server detail and no fallback layout', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(400, {
      detail: 'session backend gone is not configured; update the task backend before previewing',
    }),
  });

  await h.context.openTaskContextModal('s1');

  const html = h.body.innerHTML;
  assert.match(html, /Task &amp; context unavailable: session backend gone is not configured/);
  assert.doesNotMatch(html, /Migrate users to the new schema/, 'no field layout fallback');
  assert.doesNotMatch(html, /Next run context/);
});

test('the task block renders as prose, never a literal heading marker, and data-raw holds its text', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });

  await h.context.openTaskContextModal('s1');

  // The dialog handed the block text to the prose renderer, which turns the
  // ATX headings into elements: no literal '# Task record' paints anywhere.
  assert.deepEqual(h.context.__proseCalls, [TASK_BLOCK_TEXT]);
  const html = h.body.innerHTML;
  assert.match(html, /<h1>Task record<\/h1>/);
  // The painted markup holds no literal heading marker; data-raw (the mdDiv
  // round-trip attribute, never displayed as text) carries the block text.
  const painted = html.replace(/data-raw="[^"]*"/g, 'data-raw=""');
  assert.doesNotMatch(painted, /# Task record/);
  assert.equal(html.includes('data-raw="' + TASK_BLOCK_TEXT + '"'), true, 'data-raw carries the block text itself');
});

test('a missing session detail is reported in place of the body', async () => {
  const h = buildHarness({'/api/sessions/gone': jsonResponse(404, {detail: 'Session not found'})});

  await h.context.openTaskContextModal('gone');

  assert.deepEqual(h.calls, ['/api/sessions/gone']);
  assert.match(h.body.innerHTML, /Session unavailable: Session not found/);
});

const ESCAPE_PREVIEW = {
  ...PREVIEW,
  blocks: PREVIEW.blocks.slice(0, 3).concat([{
    sources: [{scope: 'task', source_ref: 'metadata.json#task', source_session_id: 's1'}],
    body_ref: 'b4', delivery: 'full', text: '<b>bold</b> & co',
  }]),
};

test('task text is escaped, and a preview that lands after close does not repaint', async () => {
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': () => gate.then(() => jsonResponse(200, ESCAPE_PREVIEW)),
  });

  const opened = h.context.openTaskContextModal('s1');
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(h.body.innerHTML, /Loading/);

  h.context.closeTaskContextModal();
  assert.equal(h.overlay.classList.contains('hidden'), true);
  assert.equal(h.overlay.classList.contains('flex'), false);
  release();
  await opened;
  assert.doesNotMatch(h.body.innerHTML, /manager_turn/, 'the late preview does not repaint a closed dialog');
});

test('the painted task block escapes its raw text', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, ESCAPE_PREVIEW),
  });

  await h.context.openTaskContextModal('s1');

  assert.equal(
    h.body.innerHTML.includes('data-raw="&lt;b&gt;bold&lt;/b&gt; &amp; co"'), true, 'data-raw holds the escaped text');
  assert.match(h.body.innerHTML, /&lt;b&gt;bold&lt;\/b&gt; &amp; co/, 'the prose renderer escapes raw HTML');
});
