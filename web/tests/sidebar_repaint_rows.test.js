// The sidebar's repaint for a module the page fetched after its document. The module registers a row decoration when
// it lands, after the list painted, and asks for the paint to run again. The repaint paints the last paint's own
// arguments, so an empty or search paint is redrawn as itself rather than replaced by the rows an earlier paint left
// behind. The groups.js vm harness is shared: web/tests/sidebar_groups_context_stub.js.
const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroups} = require('./sidebar_groups_context_stub');

function listContext() {
  const context = loadGroups();
  const nav = {innerHTML: ''};
  context.document.getElementById = (id) => (id === 'session-list' ? nav : null);
  // The grouped paint finishes by refreshing the rows' relative times.
  context.updateRelativeTimes = () => {};
  return {context, nav};
}

const SESSION = {id: 's1', name: 'demo session', updated_at: '2026-07-29T17:12:00Z', profile: 'manager'};

test('the repaint draws the last paint again, rows and all', () => {
  const {context, nav} = listContext();
  context.Sidebar.renderSessionList([SESSION], 'all');
  const painted = nav.innerHTML;
  assert.match(painted, /demo session/);
  context.Sidebar.repaintRenderedRows();
  assert.equal(nav.innerHTML, painted);
});

test('the repaint redraws an empty view as its own note, not the rows an earlier paint left behind', () => {
  const {context, nav} = listContext();
  context.Sidebar.renderSessionList([SESSION], 'all');
  context.Sidebar.renderSessionList([], 'archived');
  const empty = nav.innerHTML;
  assert.match(empty, /No archived sessions/);
  context.Sidebar.repaintRenderedRows();
  assert.equal(nav.innerHTML, empty);
});

test('a repaint before any paint draws nothing', () => {
  const {context, nav} = listContext();
  context.Sidebar.repaintRenderedRows();
  assert.equal(nav.innerHTML, '');
});

// The search box the list reads: its text tells a search paint from a Workspace paint.
function listWithSearchBox() {
  const context = loadGroups();
  const nav = {innerHTML: ''};
  const box = {value: ''};
  context.document.getElementById = (id) => {
    if (id === 'session-list') return nav;
    if (id === 'sidebar-search') return box;
    return null;
  };
  context.updateRelativeTimes = () => {};
  return {context, nav, box};
}

const OTHER = {id: 's2', name: 'other search row', updated_at: '2026-07-29T17:10:00Z', profile: 'manager'};

test('after a restore, the repaint with an empty box draws the Workspace rows, not the search rows', () => {
  const {context, nav, box} = listWithSearchBox();
  context.Sidebar.renderSessionList([SESSION], 'all');
  box.value = 'needle';
  context.Sidebar.renderSearchNote('Searching…');
  context.Sidebar.renderSessionList([OTHER], 'search');

  box.value = '';
  assert.equal(context.Sidebar.restoreWorkspaceFromMemory(), true);
  const restored = nav.innerHTML;
  assert.match(restored, /demo session/);
  assert.doesNotMatch(restored, /other search row/);

  context.Sidebar.repaintRenderedRows();
  assert.equal(nav.innerHTML, restored, 'the resize repaint draws the restored Workspace again');
});

test('a repaint during the search note draws nothing, so the note keeps the list', () => {
  const {context, nav, box} = listWithSearchBox();
  context.Sidebar.renderSessionList([SESSION], 'all');
  box.value = 'needle';
  context.Sidebar.renderSearchNote('Searching…');
  const note = nav.innerHTML;

  context.Sidebar.repaintRenderedRows();
  assert.equal(nav.innerHTML, note);
});

test('a repaint after a row merge keeps the merged rows, not the last full list', () => {
  const {context, nav} = listWithSearchBox();
  context.Sidebar.renderSessionList([SESSION], 'all');

  context.Sidebar.mergeRowsIntoRenderedList([{...SESSION, name: 'renamed session'}], []);
  assert.match(nav.innerHTML, /renamed session/);
  context.Sidebar.repaintRenderedRows();
  assert.match(nav.innerHTML, /renamed session/);
});

test('a row removed inline stays removed on a resize repaint', () => {
  const {context, nav} = listWithSearchBox();
  context.Sidebar.renderSessionList([SESSION, OTHER], 'all');

  context.Sidebar.removeSessionFromRenderedList('s1');
  context.Sidebar.repaintRenderedRows();
  assert.doesNotMatch(nav.innerHTML, /demo session/);
  assert.match(nav.innerHTML, /other search row/);
});
