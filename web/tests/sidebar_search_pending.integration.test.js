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

// A full answer holds only the newest matches, so the list says so. The notice stands above the first row: a
// reader who scans the rows first has concluded that the match they want is not there.
test('a full search answer states its cap above the first row', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const rows = Array.from({length: 200}, (_, index) => makeSessionMeta('s' + index));

  context.renderSessionList(rows, 'search');

  const notice = nav.innerHTML.indexOf('newest 200 matches');
  const firstRow = nav.innerHTML.indexOf('id="session-s0"');
  assert.ok(notice >= 0, 'a full answer carries the cap notice');
  assert.ok(firstRow >= 0, 'the rows follow the notice');
  assert.ok(notice < firstRow, 'the notice stands above the rows, where the reader starts');
});

test('a short search answer carries no cap notice', () => {
  const {context, nav} = buildSidebarIndicatorContext([]);

  context.renderSessionList(Array.from({length: 199}, (_, index) => makeSessionMeta('s' + index)), 'search');

  assert.deepEqual(rowIds(nav.innerHTML).length, 199);
  assert.doesNotMatch(nav.innerHTML, /newest 200 matches/);
});

// ---------------------------------------------------------------------------
// Broadcast refreshes of an open search (handleSidebarSearch with fromBroadcast). A broadcast while a request is in
// flight only marks the rows due and keeps the generation, so its answer is never discarded. The request's end sends
// one request for the newest text, and a broadcast during that request marks one more. window.sidebarSearchCounters
// counts the requests, the discarded responses and the list paints.
// ---------------------------------------------------------------------------

const counters = (context) => ({...context.sidebarSearchCounters});

test('broadcasts during a search in flight discard nothing and send one trailing request', async () => {
  const {context} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  const start = counters(context);

  context.handleSidebarSearch('debug', true);
  context.handleSidebarSearch('debug', true);
  context.handleSidebarSearch('debug', true);
  assert.equal(pending.length, 1, 'no request goes out while the first one is in flight');

  pending[0].resolve({json: async () => [makeSessionMeta('a')]});
  await flush();
  assert.equal(pending.length, 2, 'the end of the in-flight request sends one trailing request');
  assert.equal(context.sidebarSearchCounters.discard, start.discard, 'no broadcast discards a response');
  assert.equal(context.sidebarSearchCounters.fetch - start.fetch, 1, 'three broadcasts cost one request');
});

test('a burst of broadcasts across a trailing window adds at most one request each', async () => {
  const {context} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  const start = counters(context);

  // Each round, a broadcast lands while the newest request is in flight, and then that request answers.
  const broadcasts = 6;
  for (let round = 0; round < broadcasts; round += 1) {
    context.handleSidebarSearch('debug', true);
    pending[pending.length - 1].resolve({json: async () => [makeSessionMeta('a')]});
    await flush();
  }

  assert.equal(pending.length, broadcasts + 1, 'each broadcast adds one trailing request to the wire');
  assert.ok(context.sidebarSearchCounters.fetch - start.fetch <= broadcasts + 1,
      'the fetch increment stays within the broadcast count plus one');
  assert.equal(context.sidebarSearchCounters.discard, start.discard, 'no burst response is discarded');
});

test('the list converges on the server answer after the last broadcast', async () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  context.handleSidebarSearch('debug', true);

  // The first answer predates the broadcast; the trailing request reads the state the broadcast reports.
  pending[0].resolve({json: async () => [makeSessionMeta('old')]});
  await flush();
  pending[1].resolve({json: async () => [makeSessionMeta('old'), makeSessionMeta('new')]});
  await flush();

  assert.deepEqual(rowIds(nav.innerHTML), ['old', 'new']);
});

test('an identical answer paints nothing', async () => {
  const {context} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  pending[0].resolve({json: async () => [makeSessionMeta('a')]});
  await flush();
  const painted = counters(context).render;

  context.handleSidebarSearch('debug', true);
  pending[1].resolve({json: async () => [makeSessionMeta('a')]});
  await flush();

  assert.equal(context.sidebarSearchCounters.render, painted, 'the same rows do not repaint the list');
});

test('a broadcast during the trailing request sends one more request', async () => {
  const {context} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  context.handleSidebarSearch('debug', true);
  pending[0].resolve({json: async () => [makeSessionMeta('a')]});
  await flush();
  assert.equal(pending.length, 2, 'the trailing request is on the wire');

  context.handleSidebarSearch('debug', true);
  assert.equal(pending.length, 2, 'the broadcast marks the trailing request due');
  pending[1].resolve({json: async () => [makeSessionMeta('a'), makeSessionMeta('b')]});
  await flush();

  assert.equal(pending.length, 3, 'the trailing request\'s end sends the one that is due');
});

test('a rejected search releases the in-flight state and sends the pending request', async () => {
  const {context, nav} = buildSidebarIndicatorContext([]);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  context.handleSidebarSearch('debug', true);

  pending[0].reject(new Error('injected failure'));
  await flush();
  assert.equal(pending.length, 2, 'the failure releases the request and the due one goes out');

  pending[1].resolve({json: async () => [makeSessionMeta('a')]});
  await flush();
  assert.deepEqual(rowIds(nav.innerHTML), ['a']);
  assert.equal(context.sidebarSearchCounters.discard, 0, 'the failed request was the current one, not superseded');
});

test('a search row whose session stores a waiting item shows the orange marker', async () => {
  const {context, nav} = buildSidebarIndicatorContext(['w']);
  const pending = deferredFetches(context);
  context.handleSidebarSearch('debug');
  pending[0].resolve({json: async () => [makeSessionMeta('w', {has_waiting_items: true})]});
  await flush();

  const markerClass = nav.innerHTML.match(/<[a-z]+ id="waiting-marker-w"[^>]*class="([^"]*)"/);
  assert.ok(markerClass, 'the search row paints its waiting marker');
  assert.equal(/\bhidden\b/.test(markerClass[1]), false, 'the marker is shown on the row that stores a waiting item');
});
