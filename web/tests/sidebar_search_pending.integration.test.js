// ---------------------------------------------------------------------------
// Sidebar search state (filters.js handleSidebarSearch): the list stops
// showing the previous query's rows as soon as the text changes. A response for
// a superseded query never repaints the list. Harness:
// session_context_stub's sidebar context, whose inline timers fire the
// debounce inside the call, so each search's fetch starts synchronously and
// the test controls the response order.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { buildSidebarIndicatorContext, makeSessionMeta } = require('./session_context_stub');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// A fetch whose responses the test resolves, one per search in call order.
function deferredFetches(context) {
  const pending = [];
  context.fetch = (url) => new Promise((resolve, reject) => {
    pending.push({url, resolve, reject});
  });
  return pending;
}

const rowIds = (html) => [...html.matchAll(/id="session-([^"]+)"/g)].map((match) => match[1]);

test('the first keystroke replaces the previous rows before any response', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.renderSessionList([makeSessionMeta('old')], 'all');

  context.handleSidebarSearch('new');

  assert.equal(pending.length, 1, 'the search request starts while the pending note shows');
  assert.ok(nav.innerHTML.length > 0, 'the list carries a note instead of staying blank');
  assert.deepEqual(rowIds(nav.innerHTML), [], 'no row of the previous query stays under the new text');
});

test('a superseded query response does not replace the newer query rows', async () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);

  context.handleSidebarSearch('old');
  context.handleSidebarSearch('new');
  assert.equal(pending.length, 2);

  pending[1].resolve({json: async () => [makeSessionMeta('new')]});
  await flush();
  assert.deepEqual(rowIds(nav.innerHTML), ['new']);

  pending[0].resolve({json: async () => [makeSessionMeta('old')]});
  await flush();
  assert.deepEqual(rowIds(nav.innerHTML), ['new'], 'the older response arrived late and is ignored');
});

test('a failed search leaves a note rather than the previous rows', async () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.renderSessionList([makeSessionMeta('old')], 'all');

  context.handleSidebarSearch('new');
  pending[0].reject(new Error('unreachable'));
  await flush();

  assert.deepEqual(rowIds(nav.innerHTML), []);
  assert.ok(nav.innerHTML.length > 0, 'the failure shows in place of the list');
});

test('a refresh of the shown query keeps its rows until the response lands', async () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);

  context.handleSidebarSearch('same');
  pending[0].resolve({json: async () => [makeSessionMeta('same')]});
  await flush();
  assert.deepEqual(rowIds(nav.innerHTML), ['same']);

  // The WebSocket handlers refetch the open query; that refresh keeps the rows.
  context.handleSidebarSearch('same');
  assert.deepEqual(rowIds(nav.innerHTML), ['same'], 'a same-query refresh does not blank the list');
  pending[1].resolve({json: async () => [makeSessionMeta('same')]});
  await flush();
  assert.deepEqual(rowIds(nav.innerHTML), ['same']);
});

test('a workspace badge repaint does not replace the pending note', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  deferredFetches(context);
  context.renderSessionList([makeSessionMeta('old')], 'all');

  context.handleSidebarSearch('new');
  context.paintWorkspaceBadges('<span id="badge"></span>');

  assert.doesNotMatch(nav.innerHTML, /id="badge"/);
  assert.deepEqual(rowIds(nav.innerHTML), [], 'the badge repaint leaves the search note in place');
});
