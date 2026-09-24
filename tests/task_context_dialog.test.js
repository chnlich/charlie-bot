// ---------------------------------------------------------------------------
// Task & context dialog data assembly (modals.js): opening the dialog for a
// session fetches its detail and its effective-prompt preview, renders the
// task record's three fields with visible empty states, lists one row per
// assembled block with its sources, delivery and measured length, and
// degrades to a note when the session is not a task node yet or the preview
// is refused. Harness follows sidebar_rename_prefill.test.js: namespace.js
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
  const context = {
    Sidebar: {},
    globalThis: null,
    document: {getElementById: (id) => elements.get(id) || null},
    console: {error: () => {}},
    escapeHtml: escapeHtmlText,
    fetch: async (url) => {
      calls.push(url);
      const route = routes[url];
      if (!route) throw new Error('unexpected fetch ' + url);
      return typeof route === 'function' ? route() : route;
    },
  };
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
  ],
  prompt_hash: 'abc', char_count: 1602,
};

test('opening the dialog renders the task record and one row per assembled block', async () => {
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
  assert.match(html, /Migrate users to the new schema/);
  assert.match(html, /<li>Old columns readable for one release<\/li><li>pytest green<\/li>/);
  assert.match(html, /<li>docs\/auth\.md<\/li>/);
  assert.match(html, /manager_turn · 1,602 chars/, 'the run kind and the measured total');
  assert.match(html, /base<\/span> prompts\/task_manager\.md[\s\S]*?>full<[\s\S]*?>1,200</);
  assert.match(html, /tooling\/repo-layout<br>[\s\S]*?research\/branch-flow[\s\S]*?>index<[\s\S]*?>340</);
  assert.match(html, /rule:evidence <span[^>]*>from root-1<\/span>[\s\S]*?>62</, 'a subtree rule names its owning node');
  assert.doesNotMatch(html, /Launch overlay unavailable|\(none\)/);
});

test('empty task fields show a visible empty state; an overlay error is named', async () => {
  const detail = {...DETAIL, task: {goal: '  ', acceptance: [], context_refs: []}};
  const preview = {...PREVIEW, blocks: [], char_count: 0, overlay: {declared: true, error: 'FileNotFoundError'}};
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, detail),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, preview),
  });

  await h.context.openTaskContextModal('s1');

  assert.equal((h.body.innerHTML.match(/\(none\)/g) || []).length, 3, 'goal, acceptance and context refs');
  assert.match(h.body.innerHTML, /manager_turn · 0 chars/);
  assert.match(h.body.innerHTML, /Launch overlay unavailable: FileNotFoundError/);
});

test('a session without a profile shows its task record and a not-a-node note, with no preview request', async () => {
  const h = buildHarness({
    '/api/sessions/legacy': jsonResponse(200, {id: 'legacy', name: 'Old chat', profile: null, task: null}),
  });

  await h.context.openTaskContextModal('legacy');

  assert.deepEqual(h.calls, ['/api/sessions/legacy']);
  assert.equal(h.title.textContent, 'Task & context · Old chat');
  assert.equal((h.body.innerHTML.match(/\(none\)/g) || []).length, 3);
  assert.match(h.body.innerHTML, /Not a task node yet/);
});

test('a refused preview keeps the task record and names the server detail', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(400, {
      detail: 'session backend gone is not configured; update the task backend before previewing',
    }),
  });

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /Migrate users to the new schema/);
  assert.match(h.body.innerHTML, /Next run context unavailable: session backend gone is not configured/);
});

test('a missing session detail is reported in place of the body', async () => {
  const h = buildHarness({'/api/sessions/gone': jsonResponse(404, {detail: 'Session not found'})});

  await h.context.openTaskContextModal('gone');

  assert.deepEqual(h.calls, ['/api/sessions/gone']);
  assert.match(h.body.innerHTML, /Session unavailable: Session not found/);
});

test('task text is escaped, and a preview that lands after close does not repaint', async () => {
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, task: {goal: '<b>bold</b> & co', acceptance: [], context_refs: []}}),
    '/api/sessions/s1/effective-prompt': () => gate.then(() => jsonResponse(200, PREVIEW)),
  });

  const opened = h.context.openTaskContextModal('s1');
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(h.body.innerHTML, /&lt;b&gt;bold&lt;\/b&gt; &amp; co/);
  assert.match(h.body.innerHTML, /Loading next run context/);

  h.context.closeTaskContextModal();
  assert.equal(h.overlay.classList.contains('hidden'), true);
  assert.equal(h.overlay.classList.contains('flex'), false);
  release();
  await opened;
  assert.doesNotMatch(h.body.innerHTML, /manager_turn/, 'the late preview does not repaint a closed dialog');
});
