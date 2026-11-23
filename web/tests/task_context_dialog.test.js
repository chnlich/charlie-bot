// ---------------------------------------------------------------------------
// Task & context dialog data assembly (modals.js): opening the dialog for a
// session fetches its detail and its effective-prompt preview. It renders the
// task record's three fields with visible empty states, and lists one row per
// assembled block with its sources, delivery and measured length. It degrades
// to a note when the session is not a task node yet or the preview is refused.
// Harness follows sidebar_rename_prefill.test.js: namespace.js
// then modals.js in a vm context over the three dialog elements.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, readPackageStatic} = require('./read_static');
const {createElement} = require('./dom_element_stub');
const {escapeHtmlText} = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const MODALS_JS = readStatic('sidebar/modals.js');
const CRON_JS = readPackageStatic('features', 'cron', 'cron.js');

function jsonResponse(status, body) {
  return {ok: status >= 200 && status < 300, status, statusText: 'HTTP ' + status, json: async () => body};
}

// routes: url -> response, or a function returning a (possibly pending) response. beforeModals(context) runs after the
// namespace loads and before the dialog's script does, the order a package's registration meets on the page.
function buildHarness(routes, {beforeModals = () => {}} = {}) {
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
  beforeModals(context);
  vm.runInContext(MODALS_JS, context, {filename: 'modals.js'});
  return {context, overlay, title, body, calls};
}

const DETAIL = {
  id: 's1', name: 'Auth rework', profile: 'manager',
  web_status: {goal: 'Migrate users to the new schema', now: null, waiting: []},
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
  assert.match(html, /<md>Migrate users to the new schema<\/md>/, 'the goal renders through the prose pipeline');
  assert.doesNotMatch(html, /&lt;md&gt;/);
  assert.match(html, /Migrate users to the new schema/);
  assert.doesNotMatch(html, /Old columns readable|docs\/auth\.md/, 'the retired task fields stay off the dialog');
  assert.match(html, /manager_turn · 1,602 chars/, 'the run kind and the measured total');
  // One item per block, each carrying its own sources and its measured length.
  // The block takes the dialog's full width, so the source tokens stay whole.
  const items = [...html.matchAll(/<li[^>]*>([\s\S]*?)<\/li>/g)].map((m) => m[1]);
  assert.equal(items.length, 3);
  assert.match(items[0], /base<\/span> prompts\/task_manager\.md/);
  assert.match(items[0], /1,200/);
  assert.match(items[0], /full/);
  assert.match(items[1], /tooling\/repo-layout<br>[\s\S]*?research\/branch-flow/);
  assert.match(items[1], /340/);
  assert.match(items[1], /index/);
  assert.match(items[2], /rule:evidence <span[^>]*>from root-1<\/span>/, 'a subtree rule names its owning node');
  assert.match(items[2], /62/);
  assert.doesNotMatch(html, /Launch overlay unavailable|\(none\)/);
});

test('empty task fields show a visible empty state; an overlay error is named', async () => {
  const detail = {...DETAIL, web_status: {goal: '  ', now: null, waiting: []}};
  const preview = {...PREVIEW, blocks: [], char_count: 0, overlay: {declared: true, error: 'FileNotFoundError'}};
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, detail),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, preview),
  });

  await h.context.openTaskContextModal('s1');

  assert.equal((h.body.innerHTML.match(/\(none\)/g) || []).length, 1, 'the goal');
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
  assert.equal((h.body.innerHTML.match(/\(none\)/g) || []).length, 1, 'the goal');
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

// The route answers a backend id its config does not hold with the config validator's
// message, which names backends.options, backends.pools and every configured id. The
// dialog's reader needs the fact instead.
const UNCONFIGURED_BACKEND_DETAIL =
  "backend 'mock-backend' is not a backends.options or backends.pools id; valid ids: ['pool']";

test('a refused preview for a backend the page config lacks names the backend alone', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, backend: 'mock-backend'}),
    '/api/sessions/s1/effective-prompt': jsonResponse(400, {detail: UNCONFIGURED_BACKEND_DETAIL}),
  });
  h.context.BACKEND_OPTIONS = {synthetic: {id: 'synthetic', label: 'Synthetic'}};
  h.context.BACKEND_POOLS = [{id: 'pool', label: 'Pool'}];

  await h.context.openTaskContextModal('s1');

  const html = h.body.innerHTML;
  assert.match(html, /Migrate users to the new schema/);
  assert.match(html, /Next run context unavailable: the session's backend 'mock-backend' is not configured on this server\./);
  assert.doesNotMatch(html, /backends\.options|backends\.pools|valid ids/);
});

test('an unrelated refusal keeps the server detail even when the backend is unknown', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, backend: 'mock-backend'}),
    '/api/sessions/s1/effective-prompt': jsonResponse(500, {detail: 'prompt assembly failed: template missing'}),
  });
  h.context.BACKEND_OPTIONS = {};
  h.context.BACKEND_POOLS = [];

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /Next run context unavailable: prompt assembly failed: template missing/);
  assert.doesNotMatch(h.body.innerHTML, /is not configured on this server/);
});

test('a configured backend keeps the server detail on a refused preview', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, backend: 'synthetic'}),
    '/api/sessions/s1/effective-prompt': jsonResponse(400, {detail: UNCONFIGURED_BACKEND_DETAIL}),
  });
  h.context.BACKEND_OPTIONS = {synthetic: {id: 'synthetic', label: 'Synthetic'}};
  h.context.BACKEND_POOLS = [];

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /Next run context unavailable: backend 'mock-backend' is not a backends\.options/);
});

test('a successful preview renders for a backend the page config lacks', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, backend: 'mock-backend'}),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });
  h.context.BACKEND_OPTIONS = {};
  h.context.BACKEND_POOLS = [];

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /manager_turn · 1,602 chars/);
  assert.doesNotMatch(h.body.innerHTML, /Next run context unavailable/);
});

test('a heading-bearing goal renders as prose, never a literal heading marker', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, {
      ...DETAIL,
      web_status: {goal: '## Goal\n\nSay the phrase', now: null, waiting: []},
    }),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });

  await h.context.openTaskContextModal('s1');

  // The dialog handed the raw goal to the prose renderer, which turns the
  // ATX heading into an element: no literal '## ' paints anywhere.
  assert.deepEqual(h.context.__proseCalls, ['## Goal\n\nSay the phrase']);
  const html = h.body.innerHTML;
  assert.match(html, /<h2>Goal<\/h2>/);
  // The painted markup holds no literal heading marker; data-raw (the mdDiv
  // round-trip attribute, never displayed as text) carries the raw source.
  const painted = html.replace(/data-raw="[^"]*"/g, 'data-raw=""');
  assert.doesNotMatch(painted, /## Goal/);
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
    '/api/sessions/s1': jsonResponse(200, {...DETAIL, web_status: {goal: '<b>bold</b> & co', now: null, waiting: []}}),
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

// A package adds its section between the task record and the next run context (cron's Scheduled task section).
test('a registered section paints between the task record and the next run context', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });
  const loadedFor = [];
  h.context.registerTaskContextSection(async (sessionId) => {
    loadedFor.push(sessionId);
    return '<section data-test="bound">bound task prompt</section>';
  });

  await h.context.openTaskContextModal('s1');

  assert.deepEqual(loadedFor, ['s1']);
  const html = h.body.innerHTML;
  const goal = html.indexOf('Migrate users to the new schema');
  const section = html.indexOf('bound task prompt');
  const context = html.indexOf('manager_turn');
  assert.ok(goal >= 0 && goal < section && section < context, html);
});

test('a section loader that finds no binding leaves the dialog exactly as without it', async () => {
  const routes = () => ({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });
  const plain = buildHarness(routes());
  await plain.context.openTaskContextModal('s1');
  const unbound = buildHarness(routes());
  unbound.context.registerTaskContextSection(async () => '');

  await unbound.context.openTaskContextModal('s1');

  assert.equal(unbound.body.innerHTML, plain.body.innerHTML);
});

// The dialog's script is a deferred sidebar interaction, so a package's registration runs before the script has. The
// page's cron module registers this way; a registration that found no function would stop the module's load.
test('a section registered before the dialog script loads paints when the dialog opens', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  }, {
    beforeModals: (context) => context.registerTaskContextSection(
      async () => '<section data-test="early">registered before the dialog script</section>'),
  });

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /registered before the dialog script/);
});

test('a section that fails surfaces its error instead of painting a body without it', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
  });
  h.context.registerTaskContextSection(async () => { throw new Error('section boom'); });

  await h.context.openTaskContextModal('s1');

  assert.match(h.body.innerHTML, /section boom/);
});

// The cron package's script on the page: it runs after the sidebar's namespace and before the dialog's script. Its
// sidebar row registrations stay stubs, since the sidebar modules that own them are not loaded here.
function loadCronModule(context) {
  Object.assign(context, {
    registerWorkspaceBadge() {},
    registerGroupPreviewLimitExemption() {},
    registerSessionRowBadge() {},
    registerSessionRowLine() {},
    registerSessionMenuItems() {},
  });
  context.document.readyState = 'loading';
  vm.runInContext(CRON_JS, context, {filename: 'cron.js'});
}

const BOUND_PROMPT_TASK = {
  name: 'weekly', cron: '0 7 * * 1', timezone: 'UTC', enabled: true, session_id: 's1', prompt_file: 'p.md',
  prompt: 'Summarize the day', repo: null, allow_failure: false, mode: 'master', handler: null, improve_host: null,
  steps: null, broken: false,
};

// The real field and prose helpers paint the Scheduled task section between the Goal and the next run context, and the
// section carries no control.
test('a bound task paints through the dialog helpers between TASK and NEXT RUN CONTEXT, with no control', async () => {
  const h = buildHarness({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
    '/api/cron/tasks': jsonResponse(200, [BOUND_PROMPT_TASK]),
  }, {beforeModals: loadCronModule});

  await h.context.openTaskContextModal('s1');

  const html = h.body.innerHTML;
  const goal = html.indexOf('Migrate users to the new schema');
  const section = html.indexOf('Scheduled task');
  const next = html.indexOf('Next run context');
  assert.ok(goal >= 0 && goal < section && section < next, html);
  const sectionHtml = html.slice(section, next);
  assert.match(sectionHtml, /<div class="text-xs text-slate-400 mb-1">Schedule<\/div>/, 'the real field helper labels it');
  assert.match(sectionHtml, /<div class="prose-msg break-words" data-raw="Summarize the day">/, 'the prompt is prose');
  assert.ok(h.context.__proseCalls.includes('Summarize the day'), 'the prompt goes through the Goal\'s prose renderer');
  assert.doesNotMatch(sectionHtml, /<(input|select|textarea|button)\b/, 'the section holds no control');
});

// A session no task binds gets the dialog exactly as the dialog paints without the cron package.
test('an unbound session paints the dialog byte-identical to a dialog without the cron package', async () => {
  const routes = (tasks) => ({
    '/api/sessions/s1': jsonResponse(200, DETAIL),
    '/api/sessions/s1/effective-prompt': jsonResponse(200, PREVIEW),
    '/api/cron/tasks': jsonResponse(200, tasks),
  });
  const plain = buildHarness(routes([]));
  await plain.context.openTaskContextModal('s1');
  const unbound = buildHarness(routes([{...BOUND_PROMPT_TASK, session_id: 'other-session'}]), {
    beforeModals: loadCronModule,
  });

  await unbound.context.openTaskContextModal('s1');

  assert.equal(unbound.body.innerHTML, plain.body.innerHTML);
  assert.doesNotMatch(unbound.body.innerHTML, /Scheduled task/);
});
