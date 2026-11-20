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


// The prose renderer's stand-in: the escaped text inside a tag, so a test can see which text the section handed over.
const proseStub = (text) => `<md>${escapeHtmlText(text)}</md>`;

// cron.js is an IIFE over the sidebar's registration entries and the page's
// active session; the sandbox records what it registers and which session is
// active, so each test drives a part the way the page does.
function loadCron(fetch, {readyState = 'complete', sessionId = 'other-session'} = {}) {
  const elements = {};
  const registered = {
    workspaceBadges: [], rowBadges: [], rowLines: [], menuItems: [], groupMenuItems: [], previewLimitExemptions: [],
    taskContextSections: [],
  };
  const repaints = [];
  const listeners = {};
  const context = {
    Sidebar: {
      wire(globals, sidebarOnly) { Object.assign(this, globals, sidebarOnly || {}); },
      repaintRenderedRows() { repaints.push('rows'); },
      refreshWorkspaceBadges() { repaints.push('badges'); },
      // modals.js is not loaded here; these stand in for its dialog helpers with the same shape.
      taskContextField: (label, valueHtml) => `<field label="${escapeHtmlText(label)}">${valueHtml}</field>`,
      taskContextProseHtml: (text) => `<prose>${proseStub(text)}</prose>`,
    },
    renderProseMarkdown: proseStub,
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
    registerTaskContextSection: (load) => registered.taskContextSections.push(load),
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

// The loader takes the Workspace's signal into its fetch. An abort ends the load with the AbortError, so the Workspace
// knows the badge was cut short; it is not an empty answer.
test('an aborted cron badge load rejects with the abort, and its fetch carries the signal', async () => {
  const signals = [];
  const abortableFetch = (url, opts = {}) => new Promise((resolve, reject) => {
    signals.push(opts.signal);
    opts.signal.addEventListener('abort', () => reject(Object.assign(new Error('aborted'), {name: 'AbortError'})));
  });
  // Keep the page's initial cron panel fetch out of this badge-loader assertion.
  const {registered} = loadCron(abortableFetch, {readyState: 'loading'});
  const controller = new AbortController();
  const load = registered.workspaceBadges[0](controller.signal);

  controller.abort();
  await assert.rejects(load, {name: 'AbortError'});
  assert.equal(signals[0], controller.signal);
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

// The four task shapes the section renders, as GET /api/cron/tasks serializes them (every field present, unset ones null).
const PROMPT_TASK = {
  name: 'weekly', cron: '0 7 * * 1', timezone: 'UTC', enabled: true, session_id: 's1', prompt_file: 'p.md',
  prompt: 'Summarize the day', repo: '/repo', allow_failure: false, mode: 'worker', handler: null,
  improve_host: 'host3', steps: null, broken: false,
};
const MASTER_TASK = {
  name: 'okr-envelope-weekly', cron: '0 17 15 12 *', timezone: 'UTC', enabled: true, session_id: 's1',
  prompt_file: 'okr.md', prompt: 'Weekly envelope', repo: null, allow_failure: true, mode: 'master', handler: null,
  improve_host: null, steps: null, broken: false,
};
const STEPS_TASK = {
  name: 'memory-curator', cron: '27 6 * * *', timezone: 'America/Los_Angeles', enabled: true, session_id: 's1',
  prompt_file: null, prompt: null, repo: null, allow_failure: false, mode: null, handler: null, improve_host: null,
  steps: [
    {name: 'selector', backend: null, distinct_backend_from: null, prompt_file: 'selector.md', prompt: '# Select'},
    {name: 'reviewer', backend: 'claude-opus-5', distinct_backend_from: 'selector', prompt_file: 'reviewer.md',
      prompt: 'Review the picks'},
  ],
  broken: false,
};
const HANDLER_TASK = {
  name: 'storage-cool', cron: '30 4 * * *', timezone: 'America/Los_Angeles', enabled: false, session_id: 's1',
  prompt_file: null, prompt: null, repo: null, allow_failure: false, mode: null, handler: 'cool_storage',
  improve_host: null, steps: null, broken: false,
};
const ELSEWHERE_TASK = {
  name: 'elsewhere', cron: '0 5 * * *', timezone: 'UTC', enabled: true, session_id: 's2', prompt_file: 'e.md',
  prompt: 'Other session', repo: null, allow_failure: false, mode: null, handler: null, improve_host: null,
  steps: null, broken: false,
};

// The number of times a substring occurs in the section HTML.
const occurrences = (html, text) => html.split(text).length - 1;

test('the Scheduled task section lists only the tasks bound to the session, with a readable schedule title', async () => {
  const requests = [];
  const boba = {...PROMPT_TASK, name: 'boba-order', cron: '3,33 8-12 * * 1-5', timezone: 'America/Los_Angeles',
    prompt: 'Check the order', mode: null, repo: null, improve_host: null};
  const {registered} = loadCron(tasksFetch([PROMPT_TASK, ELSEWHERE_TASK, boba], requests));
  assert.equal(registered.taskContextSections.length, 1, 'the module registers one dialog section');
  const [loadSection] = registered.taskContextSections;
  requests.length = 0;

  const html = await loadSection('s1');

  assert.deepEqual(requests.map((req) => req.url), ['/api/cron/tasks']);
  assert.ok(html.includes('weekly') && html.includes('boba-order'), html);
  assert.ok(!html.includes('elsewhere'), 'a task bound to another session is not listed');
  assert.ok(html.includes('Every Monday at 07:00'), 'the weekly schedule has a readable title');
  assert.ok(
    html.includes('At minutes 03 and 33 past each hour from 08:00 through 12:00 on Monday through Friday'),
    'a weekday schedule with minute and hour ranges has a readable title',
  );
  assert.doesNotMatch(html, /<(input|select|button|textarea)\b/, 'the section holds no control');
});

test('a prompt task shows its schedule, its set settings and its prompt, with no default line and no dash', async () => {
  const {registered} = loadCron(tasksFetch([PROMPT_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.includes('0 7 * * 1 (UTC)'), 'the Schedule field reads the cron and its timezone');
  assert.ok(html.includes('<field label="Mode">worker</field>'), html);
  assert.ok(html.includes('<field label="Repo">/repo</field>'), html);
  assert.ok(html.includes('<field label="Improve host">host3</field>'), html);
  assert.ok(html.includes('<field label="Prompt file">p.md</field>'), html);
  assert.ok(html.includes('<field label="Prompt"><prose><md>Summarize the day</md></prose></field>'), html);
  assert.ok(!html.includes('label="Enabled"') && !html.includes('Enabled:'), 'an enabled task omits the default line');
  assert.ok(!html.includes('label="Allow failure"'), 'an unset allow_failure is omitted');
  assert.ok(!html.includes('Disabled'), 'an enabled task carries no Disabled line');
  assert.ok(!html.includes('—'), 'no empty value shows a dash');
});

test('a master task shows its mode, and allow failure shows only when it is set', async () => {
  const {registered} = loadCron(tasksFetch([MASTER_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.includes('<field label="Mode">master</field>'), html);
  assert.ok(html.includes('<field label="Allow failure">yes</field>'), html);
  assert.ok(!html.includes('label="Repo"') && !html.includes('label="Improve host"'), 'unset settings are omitted');
});

test('a steps task lists each step in its declared order, with the backend and prompt each step sets', async () => {
  const {registered} = loadCron(tasksFetch([STEPS_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.includes('<field label="Steps">'), html);
  assert.ok(html.indexOf('selector') < html.indexOf('reviewer'), 'the steps keep their declared order');
  assert.ok(html.includes('<field label="Backend">claude-opus-5</field>'), html);
  assert.ok(html.includes('<field label="Distinct backend from">selector</field>'), html);
  assert.ok(html.includes('<prose><md>Review the picks</md></prose>'), html);
  assert.equal(occurrences(html, 'label="Backend"'), 1, 'a step with no backend shows no Backend line');
  assert.equal(occurrences(html, 'label="Prompt file"'), 2, 'each step shows its own prompt file');
  assert.equal(occurrences(html, 'label="Prompt"'), 2, 'each step shows its own prompt text');
});

test('a handler task shows its handler and the Disabled line, with no prompt or steps', async () => {
  const {registered} = loadCron(tasksFetch([HANDLER_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.includes('<field label="Handler">cool_storage</field>'), html);
  assert.ok(html.includes('text-xs font-medium text-amber-400">Disabled</div>'), html);
  assert.ok(!html.includes('label="Prompt'), 'a handler task has no prompt line');
  assert.ok(!html.includes('label="Steps"'), 'a handler task has no steps');
  assert.ok(!html.includes('label="Enabled"'), 'a disabled task says Disabled, not Enabled');
  assert.ok(!html.includes('—'), 'no empty value shows a dash');
});

test('a multi-task section separates its tasks with one flat divider each, in the order the list gives', async () => {
  const {registered} = loadCron(tasksFetch([PROMPT_TASK, HANDLER_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.indexOf('weekly') < html.indexOf('storage-cool'), 'tasks keep the order the list gives');
  assert.equal(occurrences(html, 'border-t border-slate-700/60'), 2, 'one divider per task');
});

test('the section holds no control, no nested scroll box and no dash across every task shape', async () => {
  const {registered} = loadCron(tasksFetch([PROMPT_TASK, MASTER_TASK, STEPS_TASK, HANDLER_TASK]));

  const html = await registered.taskContextSections[0]('s1');

  assert.doesNotMatch(html, /<(input|select|button|textarea)\b/, 'the section holds no control');
  assert.ok(!html.includes('max-h-56') && !html.includes('overflow-y-auto') && !html.includes('<pre'),
    'no nested scroll box');
  assert.ok(!html.includes('—'), 'no empty value shows a dash');
});

test('the Scheduled task section stays empty for a session that no task binds', async () => {
  const {registered} = loadCron(tasksFetch([ELSEWHERE_TASK]));

  assert.equal(await registered.taskContextSections[0]('no-tasks'), '');
});

test('a failed task-list fetch is visible in the section instead of looking like an unbound session', async () => {
  const {registered} = loadCron(async () => { throw new Error('network unavailable'); });

  const html = await registered.taskContextSections[0]('s1');

  assert.ok(html.includes('Error: network unavailable'), html);
});

test('a task that fails to load stays out of the sections and shows in the Workspace badge', async () => {
  const {registered} = loadCron(tasksFetch([
    {name: 'bad', error: 'boom', broken: true, session_id: 's1', path: '/h/config.d/cron.d/bad.yaml', enabled: null},
  ]));

  assert.equal(await registered.taskContextSections[0]('s1'), '');
  assert.notEqual(await registered.workspaceBadges[0](), '');
});
