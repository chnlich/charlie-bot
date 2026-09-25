// ---------------------------------------------------------------------------
// The worker leaf view (sidebar/workers.js + tabs.js): a worker node's main
// area shows its Run list in place of the chat, each Run as a worker card,
// and after delivery a banner with the summary and four evidence links.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {loadSidebarWorkersContext} = require('./sidebar_workers_context_stub');
const {readStatic} = require('./read_static');

const TABS_JS = readStatic('tabs.js');
const INDEX_HTML = fs.readFileSync(path.join(__dirname, '..', 'web', 'templates', 'index.html'), 'utf8');

function fakeContainer() {
  return {innerHTML: '', children: [], prepend(c) { this.children.unshift(c); }, appendChild(c) { this.children.push(c); }};
}

function run(overrides = {}) {
  return {
    id: 'run-1', kind: 'work', state: 'running', backend: 'codex-o3',
    started_at: '2026-09-18T10:00:00Z', ended_at: null,
    repo_path: null, base_branch: null, branch_name: null,
    raw_log_ref: null, events_ref: null, result_ref: null,
    ...overrides,
  };
}

const OPEN_DETAIL = {id: 'leaf-1', profile: 'worker', task_state: 'open', task: {goal: 'Implement the parser'}};
const DONE_DETAIL = {...OPEN_DETAIL, task_state: 'completed'};
const DELIVERED_RUN = run({
  id: 'run-2', state: 'success', ended_at: '2026-09-18T10:05:00Z',
  repo_path: '/repo', base_branch: 'main', branch_name: 'task/run-2',
  raw_log_ref: '/h/runs/run-2/raw.log', events_ref: '/h/runs/run-2/events.jsonl', result_ref: '/h/runs/run-2/raw.log',
});

function loadLeaf(elements, extra = {}) {
  return loadSidebarWorkersContext({
    document: {
      createElement: () => ({className: '', innerHTML: '', children: [], prepend() {}, appendChild() {}}),
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: () => [],
    },
    startPageTimer: () => {},
    // web/static/js/workers.js's event cache, which a finished card clears.
    loadedEventCounts: {delete() {}},
    ...extra,
  });
}

test('runCardRow folds the run state into the card vocabulary', () => {
  const ctx = loadLeaf(new Map());
  const fold = (state) => ctx.runCardRow(run({state}), 'goal').status;
  assert.equal(fold('success'), 'completed');
  assert.equal(fold('failed'), 'failed');
  assert.equal(fold('interrupted'), 'failed');
  assert.equal(fold('attention'), 'failed');
  assert.equal(fold('stopped'), 'cancelled');
  // A queued Run reads its own truth, never 'idle'.
  assert.equal(fold('queued'), 'queued');
  assert.equal(fold('running'), 'running');
  const row = ctx.runCardRow(DELIVERED_RUN, 'Implement the parser');
  assert.equal(row.id, 'run-2');
  assert.equal(row.description, 'Implement the parser');
  assert.equal(row.created_at, '2026-09-18T10:00:00Z');
  assert.equal(row.completed_at, '2026-09-18T10:05:00Z');
  assert.equal(row.backend, 'codex-o3');
});

test('renderLeafView paints the goal and one card per run, with no banner while the task is open', () => {
  const container = fakeContainer();
  const ctx = loadLeaf(new Map([['tab-workers', container]]));
  ctx.renderLeafView(OPEN_DETAIL, [
    run(),
    run({id: 'run-q', state: 'queued', started_at: null}),
    run({id: 'run-0', state: 'failed', started_at: '2026-09-18T09:00:00Z'}),
  ], '', 'leaf-1');
  const html = container.innerHTML;
  assert.match(html, /Worker task/);
  assert.match(html, /Implement the parser/);
  assert.match(html, /id="thread-dot-run-1" class="[^"]*bg-blue-500/);
  assert.match(html, /id="thread-dot-run-0" class="[^"]*bg-red-500/);
  // A queued Run reads its own state and its own color, never 'idle'.
  assert.match(html, /id="thread-status-run-q"[^>]*>queued &middot;/);
  assert.match(html, /id="thread-dot-run-q" class="[^"]*bg-amber-400/);
  assert.doesNotMatch(html, /id="thread-status-run-q"[^>]*>idle/);
  assert.match(html, /toggleThreadDetail\('run-1', 'leaf-1'\)/, 'the card addresses the Run through the thread alias');
  assert.doesNotMatch(html, /Delivered/);
  assert.match(html, /id="leaf-delivery-slot"><\/div>/, 'the banner slot stays empty while open');

  ctx.renderLeafView(OPEN_DETAIL, [], '', 'leaf-1');
  assert.match(container.innerHTML, /No runs yet/);
});

test('a delivered leaf shows the banner with the summary and the four evidence links', () => {
  const container = fakeContainer();
  const ctx = loadLeaf(new Map([['tab-workers', container]]));
  ctx.renderLeafView(DONE_DETAIL, [DELIVERED_RUN, run({id: 'run-1', state: 'failed'})], 'All tests pass', 'leaf-1');
  const html = container.innerHTML;
  assert.match(html, /Delivered/);
  assert.match(html, /All tests pass/);
  assert.match(html, /href="\/absolute_filepath\/h\/runs\/run-2\/raw\.log"[^>]*>Raw log</);
  assert.match(html, /href="\/absolute_filepath\/h\/runs\/run-2\/events\.jsonl"[^>]*>Events</);
  assert.match(html, /href="\/absolute_filepath\/h\/runs\/run-2\/raw\.log"[^>]*>Result</);
  assert.match(html, /href="\/diff\?repo=%2Frepo&amp;base=main&amp;head=task%2Frun-2&amp;session=leaf-1"[^>]*>Diff</);
});

test('a closed task without evidence paths shows the labels unlinked', () => {
  const ctx = loadLeaf(new Map());
  const html = ctx.leafDeliveryHtml({...OPEN_DETAIL, task_state: 'cancelled'}, [run({state: 'stopped'})], '', 'leaf-1');
  assert.match(html, /Task cancelled/);
  assert.doesNotMatch(html, /<a /);
  assert.match(html, /<span class="text-slate-500">Diff<\/span>/);
  assert.equal(ctx.leafDeliveryHtml(OPEN_DETAIL, [], '', 'leaf-1'), '');
});

test('the leaf load fetches the detail and the runs, then the poll updates cards in place and adds new runs', async () => {
  const container = fakeContainer();
  const runsContainer = fakeContainer();
  const dot = {className: 'w-2 h-2 rounded-full flex-shrink-0 bg-blue-500', classList: {contains: () => false}};
  const statusText = {textContent: 'running · 09/18 10:00'};
  const elements = new Map([['tab-workers', container]]);
  const calls = [];
  let runs = [run()];
  let detail = OPEN_DETAIL;
  const timers = [];
  const ctx = loadLeaf(elements, {
    SESSION_ID: 'leaf-1',
    fetch: (url) => {
      calls.push(url);
      const body = url.endsWith('/runs?order=desc&limit=50') ? {items: runs, next_cursor: null} : detail;
      return Promise.resolve({ok: true, status: 200, json: async () => body});
    },
    startPageTimer: (name) => { timers.push(name); },
  });
  ctx.setLeafSession('leaf-1', 'Done: parser lands');

  await ctx.ensureWorkersLoadedForActiveSession({force: true});
  assert.deepEqual([...calls], ['/api/sessions/leaf-1', '/api/sessions/leaf-1/runs?order=desc&limit=50']);
  assert.match(container.innerHTML, /thread-dot-run-1/);
  assert.deepEqual([...timers], ['workers-list']);

  // The rendered card's live elements, as the browser would expose them.
  elements.set('leaf-runs', runsContainer);
  elements.set('leaf-delivery-slot', {innerHTML: ''});
  elements.set('thread-dot-run-1', dot);
  elements.set('thread-status-run-1', statusText);
  runs = [DELIVERED_RUN, run({state: 'success', ended_at: '2026-09-18T10:02:00Z'})];
  detail = DONE_DETAIL;

  ctx.pollWorkers();
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));
  assert.match(dot.className, /bg-green-500/, 'the existing card takes its new status in place');
  assert.equal(statusText.textContent, 'completed · 09/18 10:00');
  assert.equal(runsContainer.children.length, 1, 'the new run gets a card under leaf-runs');
  assert.match(runsContainer.children[0].innerHTML, /thread-dot-run-2/);
  assert.match(elements.get('leaf-delivery-slot').innerHTML, /Delivered[\s\S]*Done: parser lands/);
});

test('a session that is not a leaf loads and polls nothing', async () => {
  const calls = [];
  const ctx = loadLeaf(new Map([['tab-workers', fakeContainer()]]), {
    SESSION_ID: 'manager-1',
    fetch: (url) => { calls.push(url); return Promise.resolve({ok: true, json: async () => ({})}); },
  });
  ctx.setLeafSession(null);
  await ctx.ensureWorkersLoadedForActiveSession({force: true});
  ctx.pollWorkers();
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual([...calls], []);
  assert.equal(ctx.activeSessionIsLeaf(), false);

  // A leaf id left over from another session never counts for this one.
  ctx.setLeafSession('leaf-1');
  assert.equal(ctx.activeSessionIsLeaf(), false);
});

function loadTabs(leaf) {
  const el = () => ({classList: {hidden: null, toggle(name, on) { if (name === 'hidden') this.hidden = on; }, add() {}, remove() {}}, style: {}});
  const elements = new Map(['tab-chat', 'tab-workers', 'tab-plans', 'tab-terminal', 'btn-chat', 'btn-terminal'].map(id => [id, el()]));
  const loads = [];
  const context = {
    document: {getElementById: (id) => elements.get(id) || null},
    platform: {isMobile: false},
    activeSessionIsLeaf: () => leaf,
    ensureWorkersLoadedForActiveSession: () => { loads.push('leaf'); },
    planPanel: {onTabShown() {}},
    console,
  };
  vm.createContext(context);
  vm.runInContext(TABS_JS, context, {filename: 'tabs.js'});
  return {context, elements, loads};
}

test('switchTab shows the Run list in place of the chat for a leaf, and the chat otherwise', () => {
  const leaf = loadTabs(true);
  leaf.context.switchTab('chat');
  assert.equal(leaf.elements.get('tab-chat').classList.hidden, true);
  assert.equal(leaf.elements.get('tab-workers').classList.hidden, false);
  assert.deepEqual([...leaf.loads], ['leaf'], 'the leaf view loads on show');
  leaf.context.switchTab('chat-plans');
  assert.equal(leaf.elements.get('tab-workers').classList.hidden, true, 'a full-area tab hides the leaf view too');

  const chat = loadTabs(false);
  chat.context.switchTab('chat');
  assert.equal(chat.elements.get('tab-chat').classList.hidden, false);
  assert.equal(chat.elements.get('tab-workers').classList.hidden, true);
  assert.deepEqual([...chat.loads], []);
});

test('Workers is no longer a tab: tabs.js lists no workers tab and the page has no Workers button', () => {
  assert.doesNotMatch(TABS_JS, /'workers'/);
  assert.doesNotMatch(INDEX_HTML, /btn-workers|switchTab\('workers'\)/);
  assert.match(INDEX_HTML, /id="tab-workers"/, 'the leaf view container stays');
});
