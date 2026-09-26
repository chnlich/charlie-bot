// ---------------------------------------------------------------------------
// The group header's hover "+" (groups.js): a named group's header renders a
// New-session-in-group button ahead of Rename that clicks through to
// createSessionInGroup with that group's name, the "(No group)" bucket renders
// none, and expandSessionGroup writes the target expanded into the
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
    nav.innerHTML.indexOf('createSessionInGroup') < nav.innerHTML.indexOf('renameGroup'),
    'the create button is placed first, before Rename');

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
  assert.doesNotMatch(nav.innerHTML, /createSessionInGroup/);
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
