const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, readPackageStatic, runIconModule } = require('./read_static');
const { escapeHtml, escapeHtmlText } = require('./escape_html_stub');

const UTILS_JS = readStatic('utils.js');
const GROUPS_JS = readStatic('sidebar/groups.js');
const CRON_JS = readPackageStatic('features', 'cron', 'cron.js');

function makeClassList() {
  const classes = new Set();
  return {
    add: (...cls) => cls.forEach(c => classes.add(c)),
    remove: (...cls) => cls.forEach(c => classes.delete(c)),
    toggle: (cls, force) => {
      const on = force === undefined ? !classes.has(cls) : force;
      on ? classes.add(cls) : classes.delete(cls);
    },
    contains: (cls) => classes.has(cls),
    set classes(arr) {},
    _set: classes,
  };
}

function makeEl() {
  return {
    value: '', textContent: '', readOnly: false, disabled: false,
    checked: false, indeterminate: false, innerHTML: '',
    classList: makeClassList(), style: {},
  };
}

// groups.js is an IIFE over globals defined by the other sidebar modules; the
// sandbox supplies the ones the Workspace badge path reaches for.
function loadGroups() {
  const elements = {'session-list': makeEl()};
  const context = {
    Sidebar: {wire(globals, sidebarOnly) { Object.assign(this, globals, sidebarOnly || {}); }},
    console: {error: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}},
    document: {getElementById: (id) => elements[id] || (elements[id] = makeEl())},
    setTimeout,
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    SESSION_ID: 'other-session',
    currentFilter: 'all',
    updateRelativeTimes: () => {},
    relativeTime: () => '',
    formatBubbleTime: () => '',
    getSessionIndicatorState: () => 'idle',
    renderPendingTriggerIndicator: () => '',
    renderPendingPlanApprovalIndicator: () => '',
  };
  context.globalThis = context;
  vm.createContext(context);
  runIconModule(context);
  // utils.js first, as the page loads it ahead of groups.js, which reads expandSessionColumns and sessionUpdatedMs
  // from it. Its real time forms replace the stubs above, so those land back after its load.
  const stubs = {updateRelativeTimes: context.updateRelativeTimes, relativeTime: context.relativeTime};
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  Object.assign(context, stubs);
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return {context, elements};
}


// cron.js is an IIFE over the sidebar's registration entries and the page's
// active session; the sandbox records what it registers and which session is
// active, so each test drives a part the way the page does.
function loadCron(fetch, {readyState = 'complete', sessionId = 'other-session'} = {}) {
  const elements = {};
  const registered = {
    workspaceBadges: [], rowBadges: [], rowLines: [], menuItems: [], groupMenuItems: [], previewLimitExemptions: [],
  };
  const repaints = [];
  const listeners = {};
  const context = {
    Sidebar: {
      wire(globals, sidebarOnly) { Object.assign(this, globals, sidebarOnly || {}); },
      repaintRenderedRows() { repaints.push('rows'); },
      refreshWorkspaceBadges() { repaints.push('badges'); },
    },
    console: {error: () => {}},
    document: {
      readyState,
      getElementById: (id) => elements[id] || (elements[id] = makeEl()),
      addEventListener: (type, listener) => { listeners[type] = listener; },
    },
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    SESSION_ID: sessionId,
    currentFilter: 'all',
    switchSidebarFilter: () => {},
    fetch,
    registerWorkspaceBadge: (load) => registered.workspaceBadges.push(load),
    registerSessionRowBadge: (render) => registered.rowBadges.push(render),
    registerSessionRowLine: (render) => registered.rowLines.push(render),
    registerSessionMenuItems: (source) => registered.menuItems.push(source),
    registerGroupMenuItem: (item) => registered.groupMenuItems.push(item),
    registerGroupPreviewLimitExemption: (predicate) => registered.previewLimitExemptions.push(predicate),
  };
  context.globalThis = context;
  vm.createContext(context);
  runIconModule(context);
  vm.runInContext(CRON_JS, context, {filename: 'cron.js'});
  return {context, elements, registered, repaints, listeners};
}

// A fetch that answers every request with *tasks* and records each request.
function tasksFetch(tasks, requests = []) {
  return async (url, opts = {}) => {
    requests.push({url, opts});
    return {ok: true, json: async () => tasks, text: async () => ''};
  };
}

test('the Workspace view shows the global failure badge the cron loader resolves to', async () => {
  const {registered} = loadCron(tasksFetch([
    {name: 'z-broken', error: 'boom z', broken: true, path: '/h/config.d/cron.d/z-broken.yaml', enabled: null},
    {name: 'fine', cron: '0 9 * * *', broken: false},
    {name: 'a-broken', error: 'boom a', broken: true, path: '/h/config.d/cron.d/a-broken.yaml', enabled: false},
  ]));
  assert.equal(registered.workspaceBadges.length, 1);
  const workspaceBadges = await registered.workspaceBadges[0]();
  const {context, elements} = loadGroups();
  context.Sidebar.renderGroupedSessionList([], 'all', {workspaceBadges});

  const html = elements['session-list'].innerHTML;
  assert.ok(html.includes('2 scheduled tasks failed to load'), html);
  // Clicking opens the error view on the first broken task by name order.
  assert.ok(html.includes("openCronErrorView('a-broken')"), html);
  // Even with zero sessions the badge pages the maintainer.
  assert.ok(html.indexOf('2 scheduled tasks failed to load') < html.indexOf('No sessions yet'), html);
});

test('the cron loader resolves to no badge when nothing is broken', async () => {
  const {registered} = loadCron(tasksFetch([{name: 'fine', cron: '0 9 * * *', broken: false}]));

  assert.equal(await registered.workspaceBadges[0](), '');
});

test('the cron preview limit exemption reads schedule_task for bound rows', () => {
  const {registered} = loadCron();
  const [exempts] = registered.previewLimitExemptions;

  assert.equal(registered.previewLimitExemptions.length, 1);
  assert.equal(exempts({schedule_task: 'nightly'}), true);
  assert.equal(exempts({schedule_task: null, schedule_cron: '0 9 * * *'}), false);
});

test('no badge when nothing is broken', () => {
  const {context, elements} = loadGroups();
  context.Sidebar.renderGroupedSessionList([], 'all');

  assert.ok(!elements['session-list'].innerHTML.includes('scheduled tasks failed to load'), elements['session-list'].innerHTML);
});

test('the error view shows the load error and the file path of a failed task, and no form', async () => {
  const {context, elements} = loadCron(tasksFetch([
    {name: 'x', error: 'yaml exploded', broken: true, path: '/home/u/.charliebot/config.d/cron.d/x.yaml', enabled: false},
  ]));
  await context.Sidebar.openCronErrorView('x');

  const box = elements['cron-error-box'].textContent;
  assert.ok(box.includes('yaml exploded'), box);
  assert.ok(box.includes('/home/u/.charliebot/config.d/cron.d/x.yaml'), box);
  assert.ok(!elements['cron-modal'].classList.contains('hidden'));
});

test('the cron module registers no schedule menu item, so no row or group menu offers an editor', () => {
  const {registered} = loadCron();

  assert.equal(registered.menuItems.length, 0);
  assert.equal(registered.groupMenuItems.length, 0);
});

// The page fetches cron.js after its document, so the sidebar's rows are painted before the schedule badge and line
// register. The module repaints those rows when it lands; a module that lands while the page still parses leaves the
// paint to the page, which runs after every registration.
test('a cron module fetched after the document repaints the rows and re-reads the Workspace badges', () => {
  const {repaints} = loadCron(tasksFetch([]));
  assert.deepEqual(repaints, ['rows', 'badges']);
});

test('a cron module that lands while the page still parses repaints nothing', () => {
  const {repaints} = loadCron(tasksFetch([]), {readyState: 'loading'});
  assert.equal(repaints.length, 0);
});

const BOUND_TASKS = [
  {name: 'nightly', cron: '0 9 * * *', timezone: 'UTC', enabled: true, session_id: 's1', prompt_file: 'p.md',
    prompt: 'Summarize the day', repo: null, allow_failure: true, mode: 'master', handler: null,
    improve_host: 'host3', steps: null},
  {name: 'elsewhere', cron: '0 5 * * *', timezone: 'UTC', enabled: true, session_id: 's2', prompt: 'Other session'},
  {name: 'paused', cron: '0 6 * * *', timezone: 'UTC', enabled: false, session_id: 's1', prompt: 'Wait',
    steps: [{name: 'work', backend: 'claude-opus', distinct_backend_from: null, prompt_file: 'w.md',
      prompt: 'Do the work'}]},
];

test('the panel lists the tasks bound to the active session, with every setting and its prompt text', async () => {
  const requests = [];
  const {context, elements} = loadCron(tasksFetch(BOUND_TASKS, requests), {sessionId: 's1'});
  // The module's own load draws the panel once; this test counts the draw it asks for.
  requests.length = 0;

  await context.Sidebar.renderCronPanel();

  assert.deepEqual(requests.map((req) => req.url), ['/api/cron/tasks']);
  const html = elements['cron-session-panel-body'].innerHTML;
  assert.ok(html.includes('nightly') && html.includes('paused'), html);
  assert.ok(!html.includes('elsewhere'), 'a task bound to another session is not listed');
  for (const text of ['s1', 'Summarize the day', 'UTC', 'p.md', 'host3', 'Do the work', 'claude-opus']) {
    assert.ok(html.includes(text), `${text} is shown: ${html}`);
  }
  assert.ok(html.includes('Disabled'), 'a disabled task keeps its block, marked disabled');
  assert.doesNotMatch(html, /<(input|select|button|textarea)\b/, 'the panel holds no control');
  assert.ok(!elements['cron-session-panel'].classList.contains('hidden'));
});

test('the panel stays hidden for a session that no task binds', async () => {
  const {context, elements} = loadCron(tasksFetch(BOUND_TASKS), {sessionId: 'no-tasks'});

  await context.Sidebar.renderCronPanel();

  assert.ok(elements['cron-session-panel'].classList.contains('hidden'));
  assert.equal(elements['cron-session-panel-body'].innerHTML, '');
});

test('the panel redraws for the session that becomes active', async () => {
  const {context, elements, listeners} = loadCron(tasksFetch(BOUND_TASKS), {sessionId: 's1'});
  assert.ok(listeners['charliebot:active-session-changed'], 'the panel listens for session switches');

  context.SESSION_ID = 's2';
  await listeners['charliebot:active-session-changed']();

  const html = elements['cron-session-panel-body'].innerHTML;
  assert.ok(html.includes('elsewhere') && !html.includes('nightly'), html);
});

test('a session switch clears the old panel while its task list is loading', async () => {
  const {context, elements, listeners} = loadCron(tasksFetch(BOUND_TASKS), {readyState: 'loading', sessionId: 's1'});
  await context.Sidebar.renderCronPanel();
  assert.ok(!elements['cron-session-panel'].classList.contains('hidden'));

  context.SESSION_ID = 'no-tasks';
  let finishFetch;
  context.fetch = () => new Promise((resolve) => { finishFetch = resolve; });
  const pending = listeners['charliebot:active-session-changed']();
  assert.ok(elements['cron-session-panel'].classList.contains('hidden'));
  assert.equal(elements['cron-session-panel-body'].textContent, '');

  finishFetch({ok: true, json: async () => BOUND_TASKS});
  await pending;
  assert.ok(elements['cron-session-panel'].classList.contains('hidden'));
});

test('a failed task-list fetch is visible instead of looking like an unbound session', async () => {
  const {context, elements} = loadCron(async () => { throw new Error('network unavailable'); }, {sessionId: 's1'});

  await context.Sidebar.renderCronPanel();

  assert.ok(!elements['cron-session-panel'].classList.contains('hidden'));
  assert.equal(elements['cron-session-panel-body'].textContent, 'Could not load scheduled tasks: Error: network unavailable');
});
