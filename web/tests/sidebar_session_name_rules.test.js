// ---------------------------------------------------------------------------
// The session-name rule across its writers. groups.js owns the title rule
// (sessionRowTitle): a Threads row under a named group header shows its title
// without the header's label. status.js's updateSidebarSessionName — the one
// write the commit-rename path and the session_renamed websocket handler both
// funnel into — repaints the title the next list repaint would give it, and
// keeps the full name as the name span's title, the row's hover text and the
// rename field's source.
// One vm context loads namespace.js, status.js and groups.js in the page's
// order, so the rule's writer and its render-side reader meet the same function.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {escapeHtml, escapeHtmlText} = require('./escape_html_stub');

const UTILS_JS = readStatic('utils.js');
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
  // utils.js first, as the page loads it ahead of every sidebar module: groups.js reads expandSessionColumns and
  // sessionUpdatedMs from it. Its real time forms replace the stub above, so it lands back after its load.
  const stubs = {relativeTime: context.relativeTime};
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  Object.assign(context, stubs);
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  vm.runInContext(STATUS_JS, context, {filename: 'status.js'});
  vm.runInContext(GROUPS_JS, context, {filename: 'groups.js'});
  return context;
}

// One session-row anchor as startRename's and updateSidebarSessionName's DOM:
// the name span, which holds the row's title and the full name as its title,
// and the group container the row sits in (null for a row outside any group).
// The name defaults to the title: a row whose title is not shortened.
function rowElement({title, name = title, groupKey}) {
  const nameEl = {textContent: title, title: name};
  const groupEl = groupKey === undefined ? null : {dataset: {sgroupKey: groupKey}};
  const row = {
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
    closest: (sel) => (sel === '.session-group' ? groupEl : null),
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

test('a live rename repaints the shortened title and the hover text is the new full name', () => {
  const context = loadNameContext();
  const elements = new Map();
  const {row, nameEl} = rowElement({title: '2026-11-10 12:17', name: 'Slack #docs-review 2026-11-10 12:17',
    groupKey: 'Slack #docs-review'});
  elements.set('session-s1', row);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.currentFilter = 'threads';

  context.updateSidebarSessionName('s1', 'Slack #docs-review 2026-11-10 12:17');

  assert.equal(nameEl.textContent, '2026-11-10 12:17', 'the title reads like the next list repaint');
  assert.equal(nameEl.title, 'Slack #docs-review 2026-11-10 12:17', 'the hover text is the full name');

  // A rename away from the label: the next repaint would show the whole name,
  // so the live write shows it too, and the hover text follows the new name.
  context.updateSidebarSessionName('s1', 'Renamed thread');

  assert.equal(nameEl.textContent, 'Renamed thread');
  assert.equal(nameEl.title, 'Renamed thread', 'the hover text follows the rename');
});

test('a live rename of a child row follows the rule its root’s header sets', () => {
  const context = loadNameContext();
  const elements = new Map();
  const {row, nameEl} = rowElement({title: '2026-11-08 09:30', name: 'Slack #docs-review 2026-11-08 09:30',
    groupKey: 'Slack #docs-review'});
  elements.set('session-s1', row);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.currentFilter = 'threads';

  context.updateSidebarSessionName('s1', 'Slack #docs-review 2026-11-08 09:30');

  assert.equal(nameEl.textContent, '2026-11-08 09:30');
  assert.equal(nameEl.title, 'Slack #docs-review 2026-11-08 09:30');
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
  assert.equal(workspace.nameEl.title, 'Slack #docs-review 2026-11-10 12:17');

  context.currentFilter = 'threads';
  context.updateSidebarSessionName('ug', 'Slack #ops-misc 2026-11-09 10:20');
  assert.equal(ungrouped.nameEl.textContent, 'Slack #ops-misc 2026-11-09 10:20',
    'the keyless bucket keeps the full name');
  assert.equal(ungrouped.nameEl.title, 'Slack #ops-misc 2026-11-09 10:20');
});

// The chat header is the other writer of the session name. displayed-session.js paints the displayed session's name
// into #header-session-name and keeps the full name as the heading's hover text. The template truncates that heading
// to one line, so a long name costs one header line instead of the screen.
test('the chat header paints the session name and thread platform badge', () => {
  const headerEl = {textContent: ''};
  const platformEl = {className: 'hidden flex-shrink-0', innerHTML: ''};
  const context = {
    PAGE_HOSTNAME: 'localhost:8000',
    SESSION_ID: 's1',
    SESSION_NAME: 'Short name',
    Chat: {platformBadgeHtml: (platform) => '<span>' + platform + '</span>'},
    document: {
      title: '',
      getElementById: (id) => ({
        'header-session-name': headerEl,
        'header-session-platform': platformEl,
      })[id] || null,
    },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/displayed-session.js'), context, {filename: 'chat/displayed-session.js'});

  const longName = `${'A long session name, '.repeat(7)}${'x'.repeat(40)}`;
  context.refreshDisplayedSessionName('s1', longName);
  assert.equal(headerEl.textContent, longName);
  assert.equal(headerEl.title, longName, 'the heading carries the full name as its hover text');
  context.setDisplayedSessionPlatform('slack');
  assert.equal(platformEl.innerHTML, '<span>slack</span>');
  assert.equal(platformEl.className, 'flex-shrink-0');
  context.setDisplayedSession('s2', 'Other session');
  assert.equal(platformEl.innerHTML, '');
  assert.equal(platformEl.className, 'hidden flex-shrink-0');

  const html = fs.readFileSync(path.join(__dirname, '..', 'templates', 'index.html'), 'utf8');
  const header = html.match(/<h2 class="([^"]*)"\s+id="header-session-name">/);
  assert.ok(header, 'index.html renders the header name heading');
  assert.ok(header[1].split(/\s+/).includes('truncate'), 'the header name takes one line');
});
