const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const GROUPS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'sidebar', 'groups.js'),
  'utf8'
);
const MODALS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'sidebar', 'modals.js'),
  'utf8'
);

function escapeHtml(str) {
  return String(str)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

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
// sandbox supplies the ones the scheduled-list badge path reaches for.
function loadGroups(cronTasksPayload) {
  const elements = {'session-list': makeEl()};
  const fetches = [];
  const context = {
    Sidebar: {expose() {}},
    console: {error: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}},
    document: {getElementById: (id) => elements[id] || (elements[id] = makeEl())},
    setTimeout,
    fetch: (url) => {
      fetches.push(url);
      const payload = url.includes('/api/cron/tasks') ? cronTasksPayload : [];
      return Promise.resolve({ok: true, json: () => Promise.resolve(payload)});
    },
    escapeHtml,
    SESSION_ID: 'other-session',
    currentFilter: 'scheduled',
    sessionUnread: {},
    updateRelativeTimes: () => {},
    refreshTuiDots: () => {},
    relativeTime: () => '',
    formatNextRun: () => '',
    formatLastRun: () => '',
    getSessionIndicatorState: () => 'idle',
    renderPendingTriggerIndicator: () => '',
    renderPendingPlanApprovalIndicator: () => '',
    renderTuiStatusDot: () => '',
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return {context, elements, fetches};
}

async function settle() {
  await new Promise(r => setTimeout(r, 5));
}

test('the scheduled list shows the global failure badge derived from the tasks fetch', async () => {
  const {context, elements} = loadGroups([
    {name: 'a-ok', cron: '0 0 * * *', prompt: 'p'},
    {name: 'z-broken', error: 'boom z', broken: true, path: '/h/config.d/cron.d/z-broken.yaml', enabled: null},
    {name: 'a-broken', error: 'boom a', broken: true, path: '/h/config.d/cron.d/a-broken.yaml', enabled: false},
  ]);
  context.Sidebar.renderGroupedScheduledList([]);
  await settle();

  const html = elements['session-list'].innerHTML;
  assert.ok(html.includes('2 个定时任务加载失败'), html);
  // Clicking opens the cron editor on the first broken task by name order.
  assert.ok(html.includes("openCronEditor('a-broken')"), html);
  // Even with zero sessions the badge pages the maintainer.
  assert.ok(html.indexOf('2 个定时任务加载失败') < html.indexOf('No scheduled sessions'), html);
});

test('no badge when nothing is broken', async () => {
  const {context, elements} = loadGroups([{name: 'a-ok', cron: '0 0 * * *', prompt: 'p'}]);
  context.Sidebar.renderGroupedScheduledList([]);
  await settle();

  assert.ok(!elements['session-list'].innerHTML.includes('加载失败'), elements['session-list'].innerHTML);
});

function loadModals() {
  const elements = {};
  const context = {
    Sidebar: {expose() {}},
    console: {error: () => {}},
    document: {getElementById: (id) => elements[id] || (elements[id] = makeEl())},
    BACKEND_OPTIONS: {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(MODALS_JS, context, {filename: 'modals.js'});
  return {context, elements};
}

test('applyCronBrokenView locks the form and shows the full error for a broken task', () => {
  const {context, elements} = loadModals();
  context.Sidebar.applyCronBrokenView({
    broken: true, error: 'yaml exploded', path: '/home/u/.charliebot/config.d/cron.d/x.yaml', enabled: false,
  });

  for (const id of ['cron-expr', 'cron-prompt', 'cron-repo', 'cron-project', 'cron-timezone']) {
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
  const {context, elements} = loadModals();
  context.Sidebar.applyCronBrokenView({broken: true, error: 'syntax', path: '/x.yaml', enabled: null});

  assert.equal(elements['cron-enabled'].disabled, true);
  assert.equal(elements['cron-enabled'].checked, false);
  assert.equal(elements['cron-enabled'].indeterminate, true);
});

test('applyCronBrokenView(null) restores the editable form for normal tasks', () => {
  const {context, elements} = loadModals();
  context.Sidebar.applyCronBrokenView({broken: true, error: 'boom', path: '/x.yaml', enabled: true});
  context.Sidebar.applyCronBrokenView(null);

  for (const id of ['cron-expr', 'cron-prompt', 'cron-repo', 'cron-project', 'cron-timezone']) {
    assert.equal(elements[id].readOnly, false, id);
  }
  assert.equal(elements['cron-backend'].disabled, false);
  assert.equal(elements['cron-enabled'].disabled, false);
  assert.equal(elements['cron-enabled'].indeterminate, false);
  assert.ok(!elements['cron-save-btn'].classList.contains('hidden'));
  assert.ok(elements['cron-error-box'].classList.contains('hidden'));
  assert.equal(elements['cron-error-box'].textContent, '');
});
