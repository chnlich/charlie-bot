// ---------------------------------------------------------------------------
// The group header's actions (groups.js): a named group's header renders a
// New-session-in-group button ahead of a Settings gear. The gear clicks through
// to openGroupHeaderMenu, which reads the group name off the gear's data
// attribute and hands openRowMenu the New scheduled task / Rename group /
// danger Delete group items. The "(No group)" bucket renders neither button.
// expandSessionGroup writes the target expanded into the
// session-group-collapsed localStorage state before the create's repaint.
// Harness follows sidebar_rename_prefill.test.js via
// sidebar_groups_context_stub.js (namespace.js + groups.js only); the gear's
// test adds session-actions.js and session-menus.js, whose openGroupHeaderMenu
// the gear calls. The deferral case runs the loader the page's document carries
// in place of the row-menu module and watches the group's one download.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {loadGroups} = require('./sidebar_groups_context_stub');

const SESSION_GROUP_COLLAPSED_STORAGE_KEY = 'session-group-collapsed';

function buildContext() {
  const context = loadGroups();
  const nav = {innerHTML: ''};
  context.document = {getElementById: (id) => (id === 'session-list' ? nav : null)};
  context.updateRelativeTimes = () => {};
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
  assert.match(button[0], /sidebarControl\('createSessionInGroup', this\.dataset\.groupName\)/);
  assert.ok(
    nav.innerHTML.indexOf('createSessionInGroup') < nav.innerHTML.indexOf('openGroupHeaderMenu'),
    'the create button is placed first, before the Settings gear');

  // A click calls the createSessionInGroup control with this.dataset.groupName.
  const calls = [];
  const stopped = [];
  context.registerSidebarControl('createSessionInGroup', (name) => calls.push(name));
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
  vm.runInContext(readStatic('sidebar/session-actions.js'), context, {filename: 'session-actions.js'});
  vm.runInContext(readStatic('sidebar/session-menus.js'), context, {filename: 'session-menus.js'});
  // A package's registered item leads the menu and receives the group name.
  const calls = [];
  context.registerGroupMenuItem({label: 'Package item', onSelect: (name) => calls.push(['package', name])});

  context.renderSessionList([sessionRow('s1', 'alpha')], 'all');

  const gear = nav.innerHTML.match(/<button[^>]*title="Settings"[^>]*>[\s\S]*?<\/button>/);
  assert.ok(gear, 'the named group header is missing the Settings gear');
  assert.match(gear[0], /sidebarControl\('openGroupHeaderMenu', this\)/);
  assert.match(gear[0], /class="[^"]*opacity-0 group-hover:opacity-100[^"]*"/);
  assert.ok(
    nav.innerHTML.indexOf('title="New session in group"') < nav.innerHTML.indexOf('title="Settings"'),
    'the gear sits after the create button');

  // A click stops at the header and opens the menu from the gear's own group
  // name. openGroupHeaderMenu hands openRowMenu the anchor and the items, with
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

  // Each item gets the gear's group name. Rename and delete run
  // session-actions.js's renameGroup and deleteGroup, so their wiring shows at
  // the fetch boundary.
  const posts = [];
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
    ['Package item', 'Rename group', 'Delete group']);
  assert.deepEqual(items.map((item) => !!item.separator), [false, false, true, false],
    'the separator sits before Delete group');
  assert.equal(items[3].danger, true, 'Delete group keeps its danger styling');
  const click = {preventDefault() {}, stopPropagation() {}};
  items[0].onSelect(click);
  items[1].onSelect(click);
  items[3].onSelect(click);
  await new Promise(setImmediate);
  assert.deepEqual(calls, [['package', 'alpha']]);
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

test('a control click before the interaction scripts arrive loads the group once and then dispatches', async () => {
  const {context} = buildContext();
  const requests = [];
  const forwarded = [];
  let settle;
  context.loadPageScripts = (key, paths) => {
    requests.push([key, Array.from(paths)]);
    return new Promise((resolve) => { settle = resolve; });
  };
  const loader = fs.readFileSync(
    path.join(__dirname, '..', 'templates', 'js', 'sidebar', 'row-menu.js'), 'utf8');
  vm.runInContext(loader, context, {filename: 'templates/js/sidebar/row-menu.js'});

  assert.equal(typeof context.Sidebar.openSessionRowMenu, 'function', 'the loader forwards the menu control');
  assert.equal(requests.length, 0, 'the loader fetches nothing until a control call');

  const pending = context.sidebarControl('openSessionRowMenu', {id: 'gear'}, 's1');
  assert.equal(requests.length, 1, 'the first call starts one download');
  assert.equal(requests[0][0], 'sidebar-interactions');
  assert.deepEqual(requests[0][1], [
    '/static/js/sidebar/row-menu.js', '/static/js/sidebar/modals.js',
    '/static/js/sidebar/session-actions.js', '/static/js/sidebar/session-menus.js',
  ]);

  // A second click while the group downloads joins the first load.
  const second = context.sidebarControl('openSessionRowMenu', {id: 'gear2'}, 's2');
  assert.equal(requests.length, 1);

  // The module's own registration arrives with the download and takes the dispatch.
  context.registerSidebarControl('openSessionRowMenu', (...args) => { forwarded.push(args); });
  settle();
  await pending;
  await second;
  assert.deepEqual(forwarded, [[{id: 'gear'}, 's1'], [{id: 'gear2'}, 's2']]);
});

test('a deferred handler reads the currentTarget the click was made with', async () => {
  const {context} = buildContext();
  let settle;
  context.loadPageScripts = () => new Promise((resolve) => { settle = resolve; });
  const loader = fs.readFileSync(
    path.join(__dirname, '..', 'templates', 'js', 'sidebar', 'row-menu.js'), 'utf8');
  vm.runInContext(loader, context, {filename: 'templates/js/sidebar/row-menu.js'});

  const wrapper = {id: 'header-wrapper'};
  const event = {currentTarget: wrapper, preventDefault() {}};
  const seen = [];
  const pending = context.startRename(event, 's1');
  // The browser clears currentTarget when the dispatch ends, before the group arrives.
  event.currentTarget = null;
  context.Sidebar.startRename = (e, id) => { seen.push([e.currentTarget, id]); };
  settle();
  await pending;
  assert.deepEqual(seen, [[wrapper, 's1']]);
});
