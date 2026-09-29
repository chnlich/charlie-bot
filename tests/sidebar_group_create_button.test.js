// ---------------------------------------------------------------------------
// The group header's actions (groups.js): a named group's header renders a
// New-session-in-group button ahead of a Settings gear -- the gear clicks
// through to openGroupHeaderMenu, which reads the group name off the gear's
// data attribute and hands openRowMenu the New scheduled task / Rename group /
// danger Delete group items; the "(No group)" bucket renders neither button,
// and expandSessionGroup writes the target expanded into the
// session-group-collapsed localStorage state before the create's repaint.
// Harness follows sidebar_rename_prefill.test.js via
// sidebar_groups_context_stub.js (namespace.js + groups.js only).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {loadGroups} = require('./sidebar_groups_context_stub');

const SESSION_GROUP_COLLAPSED_STORAGE_KEY = 'session-group-collapsed';

function buildContext() {
  const context = loadGroups();
  const nav = {innerHTML: ''};
  context.document = {getElementById: (id) => (id === 'session-list' ? nav : null)};
  context.updateRelativeTimes = () => {};
  context.refreshTuiDots = () => {};
  return {context, nav};
}

function sessionRow(id, group) {
  return {id, name: 'Session ' + id, group, updated_at: '2026-07-29T17:12:00Z'};
}

test('a named group header renders the create button first, clicking through with the group name', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([sessionRow('s1', 'alpha')], 'all');

  const button = nav.innerHTML.match(/<button data-group-name="alpha"[^>]*>[\s\S]*?<\/button>/);
  assert.ok(button, 'the named group header is missing the create button');
  assert.match(button[0], /title="New session in group"/);
  assert.match(button[0], /class="[^"]*hover:text-green-400[^"]*"/);
  assert.match(button[0], /createSessionInGroup\(this\.dataset\.groupName\)/);
  assert.ok(
    nav.innerHTML.indexOf('createSessionInGroup') < nav.innerHTML.indexOf('openGroupHeaderMenu'),
    'the create button is placed first, before the Settings gear');

  // A click resolves the createSessionInGroup global with this.dataset.groupName.
  const calls = [];
  const stopped = [];
  context.createSessionInGroup = (name) => calls.push(name);
  const onclick = button[0].match(/onclick="([^"]+)"/)[1];
  context.__button = {dataset: {groupName: 'alpha'}};
  context.__event = {stopPropagation: () => stopped.push(true)};
  vm.runInContext(`(function (event) { ${onclick} }).call(__button, __event)`, context);
  assert.deepEqual(calls, ['alpha']);
  assert.deepEqual(stopped, [true], 'the click does not reach the header toggle');
});

test('the "(No group)" header renders no create button', () => {
  const {context, nav} = buildContext();

  context.renderSessionList([sessionRow('s1', null)], 'all');

  assert.match(nav.innerHTML, /\(No group\)/);
  assert.doesNotMatch(nav.innerHTML, /createSessionInGroup|openGroupHeaderMenu/);
  const header = nav.innerHTML.match(/data-sgroup-toggle-key[\s\S]*?<div class="session-group-items/);
  assert.ok(header, 'the group header block is missing');
  assert.doesNotMatch(header[0], /<button/, 'the "(No group)" header renders no button at all');
});

test('a named group header\u2019s Settings gear reads the group name and builds the menu items', async () => {
  const {context, nav} = buildContext();

  context.renderSessionList([sessionRow('s1', 'alpha')], 'all');

  const gear = nav.innerHTML.match(/<button[^>]*title="Settings"[^>]*>[\s\S]*?<\/button>/);
  assert.ok(gear, 'the named group header is missing the Settings gear');
  assert.match(gear[0], /openGroupHeaderMenu\(this\)/);
  assert.match(gear[0], /class="[^"]*opacity-0 group-hover:opacity-100[^"]*"/);
  assert.ok(
    nav.innerHTML.indexOf('title="New session in group"') < nav.innerHTML.indexOf('title="Settings"'),
    'the gear sits after the create button');

  // A click stops at the header and opens the menu from the gear's own group
  // name: openGroupHeaderMenu hands openRowMenu the anchor and the items, with
  // no module-level state in between.
  const stopped = [];
  let captured = null;
  context.openRowMenu = (anchor, items) => { captured = {anchor, items}; };
  context.__button = {dataset: {groupName: 'alpha'}};
  context.__event = {stopPropagation: () => stopped.push(true)};
  const onclick = gear[0].match(/onclick="([^"]+)"/)[1];
  vm.runInContext(`(function (event) { ${onclick} }).call(__button, __event)`, context);
  assert.deepEqual(stopped, [true], 'the click does not reach the header toggle');
  assert.equal(captured.anchor, context.__button);

  // Each item keeps its old direct button's call, with the gear's group name.
  // New scheduled task runs session-view.js's global; rename and delete are
  // groups.js-local, so their wiring shows at the fetch boundary.
  const calls = [];
  const posts = [];
  context.createScheduledTaskInGroup = (name) => calls.push(['scheduled', name]);
  context.prompt = () => 'beta';
  context.confirm = () => true;
  context.JSON_HEADERS = {'Content-Type': 'application/json'};
  context.switchSidebarFilter = () => {};
  context.fetch = async (url, opts = {}) => {
    posts.push([url, opts.body]);
    return {ok: true};
  };
  const items = [...captured.items];
  assert.deepEqual(items.filter((item) => !item.separator).map((item) => item.label),
    ['New scheduled task', 'Rename group', 'Delete group']);
  assert.deepEqual(items.map((item) => !!item.separator), [false, false, true, false],
    'the separator sits before Delete group');
  assert.equal(items[3].danger, true, 'Delete group keeps its danger styling');
  const click = {preventDefault() {}, stopPropagation() {}};
  items[0].onSelect(click);
  items[1].onSelect(click);
  items[3].onSelect(click);
  await new Promise(setImmediate);
  assert.deepEqual(calls, [['scheduled', 'alpha']]);
  assert.deepEqual(posts, [
    ['/api/sessions/groups/rename', JSON.stringify({old_name: 'alpha', new_name: 'beta'})],
    ['/api/sessions/groups/delete', JSON.stringify({group: 'alpha'})],
  ]);
});

test('expandSessionGroup turns a stored collapsed group into an expanded one', () => {
  const {context} = buildContext();
  const writes = [];
  context.localStorage = {
    getItem: (key) => (key === SESSION_GROUP_COLLAPSED_STORAGE_KEY ? '{"alpha": true}' : null),
    setItem: (key, value) => writes.push([key, value]),
  };

  context.Sidebar.expandSessionGroup('alpha');

  assert.deepEqual(writes, [[SESSION_GROUP_COLLAPSED_STORAGE_KEY, '{"alpha":false}']]);
});
