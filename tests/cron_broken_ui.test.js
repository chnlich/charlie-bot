const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic, readPackageStatic } = require('./read_static');
const { escapeHtml, escapeHtmlText } = require('./escape_html_stub');

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
    sessionUnread: {},
    updateRelativeTimes: () => {},
    relativeTime: () => '',
    formatBubbleTime: () => '',
    getSessionIndicatorState: () => 'idle',
    renderPendingTriggerIndicator: () => '',
    renderPendingPlanApprovalIndicator: () => '',
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return {context, elements};
}


// cron.js is an IIFE over the sidebar's registration entries; the sandbox
// records what it registers, so each test drives a part the way core does.
function loadCron(fetch) {
  const elements = {};
  const registered = {
    workspaceBadges: [], rowBadges: [], rowLines: [], menuItems: [], groupMenuItems: [], previewLimitExemptions: [],
  };
  const context = {
    Sidebar: {wire(globals, sidebarOnly) { Object.assign(this, globals, sidebarOnly || {}); }},
    console: {error: () => {}},
    document: {getElementById: (id) => elements[id] || (elements[id] = makeEl())},
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    JSON_HEADERS: {'Content-Type': 'application/json'},
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
  vm.runInContext(CRON_JS, context, {filename: 'cron.js'});
  return {context, elements, registered};
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
  // Clicking opens the cron editor on the first broken task by name order.
  assert.ok(html.includes("openCronEditor('a-broken')"), html);
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

test('applyCronBrokenView locks the form and shows the full error for a broken task', () => {
  const {context, elements} = loadCron();
  context.Sidebar.applyCronBrokenView({
    broken: true, error: 'yaml exploded', path: '/home/u/.charliebot/config.d/cron.d/x.yaml', enabled: false,
  });

  for (const id of ['cron-expr', 'cron-prompt-file', 'cron-repo', 'cron-project', 'cron-timezone']) {
    assert.equal(elements[id].readOnly, true, id);
  }
  assert.equal(elements['cron-backend'].disabled, true);
  assert.equal(elements['cron-enabled'].disabled, true);
  assert.equal(elements['cron-enabled'].checked, false);
  assert.equal(elements['cron-enabled'].indeterminate, false);
  assert.ok(elements['cron-save-btn'].classList.contains('hidden'));
  assert.ok(!elements['cron-error-box'].classList.contains('hidden'));
  assert.ok(elements['cron-error-box'].textContent.includes('yaml exploded'));
  assert.ok(elements['cron-error-box'].textContent.includes('/home/u/.charliebot/config.d/cron.d/x.yaml'));
});

test('an unparseable file greys the enabled box (indeterminate)', () => {
  const {context, elements} = loadCron();
  context.Sidebar.applyCronBrokenView({broken: true, error: 'syntax', path: '/x.yaml', enabled: null});

  assert.equal(elements['cron-enabled'].disabled, true);
  assert.equal(elements['cron-enabled'].checked, false);
  assert.equal(elements['cron-enabled'].indeterminate, true);
});

test('applyCronBrokenView(null) restores the editable form for normal tasks', () => {
  const {context, elements} = loadCron();
  context.Sidebar.applyCronBrokenView({broken: true, error: 'boom', path: '/x.yaml', enabled: true});
  context.Sidebar.applyCronBrokenView(null);

  for (const id of ['cron-expr', 'cron-prompt-file', 'cron-repo', 'cron-project', 'cron-timezone']) {
    assert.equal(elements[id].readOnly, false, id);
  }
  assert.equal(elements['cron-backend'].disabled, false);
  assert.equal(elements['cron-enabled'].disabled, false);
  assert.equal(elements['cron-enabled'].indeterminate, false);
  assert.ok(!elements['cron-save-btn'].classList.contains('hidden'));
  assert.ok(elements['cron-error-box'].classList.contains('hidden'));
  assert.equal(elements['cron-error-box'].textContent, '');
});

test('the schedule items: a bound row edits its task, an unbound manager adds one bound to the row', async () => {
  const requests = [];
  const {context, elements, registered} = loadCron(tasksFetch([{name: 'task-cron1', cron: '0 9 * * *', enabled: true}],
                                                              requests));
  assert.equal(registered.menuItems.length, 1);
  const source = registered.menuItems[0];
  // The row fact the items read rides the Settings button as data-schedule-task.
  assert.deepEqual({...source.rowAttrs({schedule_task: 'task-cron1'})}, {scheduleTask: 'task-cron1'});
  assert.deepEqual({...source.rowAttrs({})}, {scheduleTask: ''});
  const labels = (attrs) => [...source.items(attrs, 'r1')].map((item) => item.label);
  assert.deepEqual(labels({scheduleTask: 'task-cron1'}), ['Edit schedule\u2026']);
  assert.deepEqual(labels({profile: 'manager'}), ['Add schedule\u2026']);
  assert.deepEqual(labels({}), []);

  // Edit opens the editor on the bound task.
  await source.items({scheduleTask: 'task-cron1'}, 'r1')[0].onSelect();
  assert.deepEqual(requests.map((req) => req.url), ['/api/cron/tasks']);
  assert.equal(elements['cron-name'].value, 'task-cron1');
  assert.ok(!elements['cron-modal'].classList.contains('hidden'));

  // Add opens the adder, and its save binds the new task to the row.
  source.items({profile: 'manager'}, 'r1')[0].onSelect();
  assert.equal(elements['cron-modal-title'].textContent, 'New Scheduled Task');
  elements['cron-name'].value = 'nightly';
  await context.Sidebar.saveCronTask();
  assert.equal(requests[1].url, '/api/cron/tasks');
  assert.equal(JSON.parse(requests[1].opts.body).session_id, 'r1');
});

test('New scheduled task creates a manager node in the group, then opens the adder bound to it', async () => {
  const requests = [];
  const {context, elements, registered} = loadCron(tasksFetch([], requests));
  const created = [];
  const expanded = [];
  context.Sidebar.createSessionNode = async (taskParentId, group) => {
    created.push([taskParentId, group]);
    return {id: 'n1'};
  };
  context.Sidebar.expandSessionGroup = (group) => expanded.push(group);
  assert.deepEqual(registered.groupMenuItems.map((item) => item.label), ['New scheduled task']);

  await registered.groupMenuItems[0].onSelect('alpha');

  assert.deepEqual(created, [[null, 'alpha']]);
  assert.deepEqual(expanded, ['alpha']);
  assert.equal(elements['cron-project'].value, 'alpha');
  assert.ok(!elements['cron-modal'].classList.contains('hidden'));
  await context.Sidebar.saveCronTask();
  assert.equal(JSON.parse(requests[0].opts.body).session_id, 'n1');
});
