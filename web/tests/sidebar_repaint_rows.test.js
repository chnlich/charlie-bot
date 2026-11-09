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
  context.document.getElementById = () => nav;
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
