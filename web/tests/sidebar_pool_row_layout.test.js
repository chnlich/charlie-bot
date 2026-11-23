const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroups} = require('./sidebar_groups_context_stub');

function renderWithSyntheticBackends(session) {
  const context = loadGroups();
  context.BACKEND_POOLS = [{id: 'pool-a', label: 'Pool A'}];
  context.BACKEND_OPTIONS = {
    'member-a': 'Fam · Member A',
    'single-a': 'Fam · Single A',
  };
  return context.Sidebar.renderSessionItem(
    {id: 's1', name: 'demo session', updated_at: '2026-07-29T17:12:00Z', ...session},
    'all'
  );
}

function modelSpan(html) {
  return html.match(/<span class="session-backend([^"]*)" title="([^"]*)">([^<]*)<\/span>/);
}

// The date line is the wrapper that holds the .session-time span. A pool row's wrapper is a block, so its label takes
// the line under the date; a single-option row's wrapper is a flex row, so its label sits beside the date.
function dateLineClass(html) {
  const wrapper = html.match(/<span class="([^"]*)"><span class="session-time/);
  assert.ok(wrapper, 'row is missing its date line');
  return wrapper[1];
}

test('a pool-session row puts its label on its own line below the date', () => {
  const html = renderWithSyntheticBackends({
    backend: 'pool-a',
    pool_member: 'member-a',
    native_model: 'model-a',
  });
  const model = modelSpan(html);
  assert.ok(model, 'row is missing a .session-backend span');
  assert.match(model[1], /\bblock\b/);
  assert.doesNotMatch(dateLineClass(html), /\bflex\b/);
  assert.ok(html.indexOf('session-time') < html.indexOf('session-backend'));
});

test('a pool worker row with a pool field puts its label on its own line below the date', () => {
  const html = renderWithSyntheticBackends({
    backend: 'member-a',
    pool: 'pool-a',
    pool_member: 'member-a',
    native_model: 'model-a',
  });
  const model = modelSpan(html);
  assert.ok(model, 'row is missing a .session-backend span');
  assert.match(model[1], /\bblock\b/);
  assert.doesNotMatch(dateLineClass(html), /\bflex\b/);
  assert.ok(html.indexOf('session-time') < html.indexOf('session-backend'));
});

test('a single-option row keeps its label beside the date on one line', () => {
  const html = renderWithSyntheticBackends({backend: 'single-a'});
  const model = modelSpan(html);
  assert.ok(model, 'row is missing a .session-backend span');
  assert.doesNotMatch(model[1], /\bblock\b/);
  assert.match(dateLineClass(html), /\bflex\b/);
});
