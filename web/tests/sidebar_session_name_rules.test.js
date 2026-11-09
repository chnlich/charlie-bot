// ---------------------------------------------------------------------------
// The session-name rule across its writers. groups.js owns the title rule
// (sessionRowTitle): a Threads row under a named group header shows its title
// without the header's label. status.js's updateSidebarSessionName — the one
// write the commit-rename path and the session_renamed websocket handler both
// funnel into — repaints the title the next list repaint would give it, and
// keeps the full name as the row's data (data-full-name) for the rename field.
// One vm context loads namespace.js, status.js and groups.js in the page's
// order, so the rule's writer and its render-side reader meet the same function.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {escapeHtml, escapeHtmlText} = require('./escape_html_stub');

const NAMESPACE_JS = readStatic('sidebar/namespace.js');
const GROUPS_JS = readStatic('sidebar/groups.js');
const STATUS_JS = readStatic('sidebar/status.js');

function loadNameContext() {
  const context = {
    Sidebar: {state: {}},
    globalThis: null,
    SESSION_ID: 'other-session',
    console: {error: () => {}, log: () => {}},
    localStorage: {getItem: () => null, setItem: () => {}},
    fetch: () => Promise.resolve({ok: true, json: async () => ({})}),
    JSON_HEADERS: {},
    setTimeout: () => 0,
    BACKEND_OPTIONS: {},
    BACKEND_POOLS: [],
    escapeHtml: escapeHtmlText,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    relativeTime: () => '',
    formatBubbleTime: () => '',
    document: {
      getElementById: () => null,
      querySelectorAll: () => [],
      querySelector: () => null,
    },
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  vm.runInContext(STATUS_JS, context, {filename: 'status.js'});
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return context;
}

// One session-row anchor as startRename's and updateSidebarSessionName's DOM:
// the name span, and the group container the row sits in (null for a row
// outside any group).
function rowElement({title, groupKey}) {
  const nameEl = {textContent: title};
  const groupEl = groupKey === undefined ? null : {dataset: {sgroupKey: groupKey}};
  const row = {
    dataset: {},
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
    closest: (sel) => (sel === '.session-group' ? groupEl : null),
    removeAttribute(name) {
      if (name === 'data-full-name') delete row.dataset.fullName;
      else throw new Error(`removeAttribute: unexpected attribute ${name}`);
    },
  };
  return {row, nameEl};
}

test('the title rule: a grouped Threads row drops the header label, every other row keeps its name', () => {
  const {sessionRowTitle} = loadNameContext();
  const name = 'Slack #docs-review 2026-11-10 12:17';

  assert.equal(sessionRowTitle({name}, 'threads', 'Slack #docs-review'), '2026-11-10 12:17');
  assert.equal(sessionRowTitle({name}, 'threads', ''), name, 'the keyless bucket keeps the full name');
  assert.equal(sessionRowTitle({name}, 'threads', undefined), name, 'no header, no shortening');
  assert.equal(sessionRowTitle({name}, 'all', 'Slack #docs-review'), name, 'the Workspace keeps the full name');
  assert.equal(sessionRowTitle({name}, 'archived', 'Slack #docs-review'), name);
  assert.equal(sessionRowTitle({name: 'Renamed thread'}, 'threads', 'Slack #docs-review'), 'Renamed thread',
    'a title that does not start with its header keeps itself');
});

test('a live rename repaints the shortened title and keeps the full name as data', () => {
  const context = loadNameContext();
  const elements = new Map();
  const {row, nameEl} = rowElement({title: '2026-11-10 12:17', groupKey: 'Slack #docs-review'});
  elements.set('session-s1', row);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.currentFilter = 'threads';

  context.updateSidebarSessionName('s1', 'Slack #docs-review 2026-11-10 12:17');

  assert.equal(nameEl.textContent, '2026-11-10 12:17', 'the title reads like the next list repaint');
  assert.equal(row.dataset.fullName, 'Slack #docs-review 2026-11-10 12:17', 'the full name stays the data');

  // A rename away from the label: the next repaint would show the whole name,
  // so the live write shows it too and drops the data attribute.
  context.updateSidebarSessionName('s1', 'Renamed thread');

  assert.equal(nameEl.textContent, 'Renamed thread');
  assert.equal(row.dataset.fullName, undefined, 'a full-title row carries no data-full-name');
});

test('a live rename of a child row follows the rule its root’s header sets', () => {
  const context = loadNameContext();
  const elements = new Map();
  const {row, nameEl} = rowElement({title: '2026-11-08 09:30', groupKey: 'Slack #docs-review'});
  elements.set('session-s1', row);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.currentFilter = 'threads';

  context.updateSidebarSessionName('s1', 'Slack #docs-review 2026-11-08 09:30');

  assert.equal(nameEl.textContent, '2026-11-08 09:30');
  assert.equal(row.dataset.fullName, 'Slack #docs-review 2026-11-08 09:30');
});

test('a live rename outside Threads, and outside any group, writes the full name', () => {
  const context = loadNameContext();
  const elements = new Map();
  const workspace = rowElement({title: 'Slack #docs-review 2026-11-10 12:17', groupKey: 'Slack #docs-review'});
  const ungrouped = rowElement({title: 'Slack #ops-misc 2026-11-09 10:20', groupKey: ''});
  elements.set('session-ws', workspace.row);
  elements.set('session-ug', ungrouped.row);
  context.document.getElementById = (id) => elements.get(id) || null;

  context.currentFilter = 'all';
  context.updateSidebarSessionName('ws', 'Slack #docs-review 2026-11-10 12:17');
  assert.equal(workspace.nameEl.textContent, 'Slack #docs-review 2026-11-10 12:17',
    'the Workspace keeps the full name, whatever the row’s header');
  assert.equal(workspace.row.dataset.fullName, undefined);

  context.currentFilter = 'threads';
  context.updateSidebarSessionName('ug', 'Slack #ops-misc 2026-11-09 10:20');
  assert.equal(ungrouped.nameEl.textContent, 'Slack #ops-misc 2026-11-09 10:20',
    'the keyless bucket keeps the full name');
  assert.equal(ungrouped.row.dataset.fullName, undefined);
});
