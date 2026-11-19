// The sidebar search view's metric math: the broadcast offsets, the e2e and overlap arithmetic, the body check, the
// window's discards, and the summary's medians and totals. The page scripts that count the paints and the span run in a
// sandbox whose globals stand in for the page's; the view's own run checks them against a real page.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import vm from 'node:vm';

import { SEARCH } from './profile.mjs';
import {
  READ_SPAN_SCRIPT,
  WRAP_RENDER_SCRIPT,
  bodyCheck,
  broadcastOffsets,
  overlapSummary,
  searchRecord,
  summarizeSearch,
  windowDiscards,
} from './search.mjs';

// A context whose globals are the given stubs, with the clock and the resource timing fixed by `performance`.
function pageSandbox({ performance = {}, globals = {} }) {
  const sandbox = {
    Date,
    URL,
    performance: { now: () => 0, timeOrigin: 0, getEntriesByType: () => [], ...performance },
    requestAnimationFrame: (callback) => callback(),
    setTimeout: (callback) => callback(),
    ...globals,
  };
  sandbox.window = sandbox;
  return vm.createContext(sandbox);
}

test('a broadcast window holds exactly the renames its load names', () => {
  const { changesPerWindow, windowMs } = SEARCH.broadcast;
  const offsets = broadcastOffsets(SEARCH.broadcast);
  assert.equal(offsets.length, changesPerWindow);
  assert.equal(offsets[0], 0);
  assert.ok(offsets.every((offset) => offset < windowMs), 'every rename starts inside the window');
  assert.equal(offsets[1] - offsets[0], windowMs / changesPerWindow);
});

test('an answer records its e2e time on the page clock and its encoded and wire bytes', () => {
  const record = searchRecord({
    query: 'plan',
    input_t: 1000,
    paint: 1450.5,
    rows: 200,
    encodedBodySize: 66574,
    transferSize: 66874,
    overlap: [],
  });
  assert.equal(record.e2e_ms, 450.5);
  assert.equal(record.body_bytes, 66574);
  assert.equal(record.wire_bytes, 66874);
  assert.deepEqual(record.overlap, { count: 0, bytes: 0, status_count: 0, status_bytes: 0 });
});

test('the overlap counts every other request and splits out the status polls', () => {
  const summary = overlapSummary([
    { path: '/api/sessions/status', transferSize: 100 },
    { path: '/api/sessions/?ids=a', transferSize: 50 },
    { path: '/api/sessions/status', transferSize: 25 },
  ]);
  assert.deepEqual(summary, { count: 3, bytes: 175, status_count: 2, status_bytes: 125 });
});

test('the body check holds the checked queries to the tolerance and reports hydeogen without one', () => {
  const live = SEARCH.liveBeforeBodyBytes.sidebar;
  assert.equal(bodyCheck('sidebar', live * (1 + SEARCH.bodyToleranceShare * 0.99)).within, true);
  assert.equal(bodyCheck('sidebar', live * (1 + SEARCH.bodyToleranceShare * 1.01)).within, false);
  assert.equal(bodyCheck('hydeogen', 1).within, null);
});

test('the summary takes medians over the repetitions and sums the discards of the broadcast windows', () => {
  const records = [];
  for (const variant of ['idle', 'broadcast']) {
    for (const query of SEARCH.queries) {
      [1, 2, 3].forEach((repetition, index) => {
        const record = {
          variant,
          repetition,
          query,
          e2e_ms: 400 + index * 100,
          body_bytes: SEARCH.liveBeforeBodyBytes[query],
          wire_bytes: SEARCH.liveBeforeBodyBytes[query] + 300,
          rows: 200,
          overlap: { count: 2, bytes: 1000, status_count: 1, status_bytes: 200 },
        };
        if (variant === 'broadcast') {
          record.window = {
            renames: SEARCH.broadcast.changesPerWindow,
            task_tree_changed_frames: SEARCH.broadcast.changesPerWindow,
            search_fetches: 6 + index,
            discarded: 10 + index,
            status_bytes: 300,
          };
        }
        records.push(record);
      });
    }
  }
  const row = summarizeSearch(records).find((candidate) => candidate.variant === 'broadcast' && candidate.query === 'sidebar');
  assert.equal(row.reps, 3);
  assert.equal(row.e2e_ms_median, 500);
  assert.equal(row.discarded_total, 33);
  assert.equal(row.discarded_median, 11);
  assert.equal(row.frames_median, SEARCH.broadcast.changesPerWindow);
  assert.equal(row.fetches_median, 7);
  assert.equal(row.check.within, true);
});

test('a window discards the answers that ended without a paint, and a paint without an answer stops the run', () => {
  assert.equal(windowDiscards({ answers: 6, answer_paints: 1 }), 5);
  assert.equal(windowDiscards({ answers: 0, answer_paints: 0 }), 0);
  assert.throws(() => windowDiscards({ answers: 1, answer_paints: 2 }), /painted 2 answers but only 1 ended/);
});

test('a repaint of the list is not an answer paint, and each call of the search refetch is counted', () => {
  const sandbox = pageSandbox({
    globals: { renderSessionList: () => {}, handleSidebarSearch: () => {} },
  });
  vm.runInContext(WRAP_RENDER_SCRIPT, sandbox);
  const answer = [{ id: 'a' }];
  sandbox.renderSessionList(answer, 'search');
  sandbox.renderSessionList(answer, 'search');
  sandbox.renderSessionList([{ id: 'b' }], 'search');
  sandbox.handleSidebarSearch('plan');
  // Array.from builds the host realm's array, which the strict comparison needs.
  assert.deepEqual(Array.from(sandbox.__searchRenders, (record) => record.repaint), [false, true, false]);
  assert.equal(sandbox.__refetchCalls.length, 1);
});

test('the span counts the answers that ended in it, the search requests and paints that began in it, and the bytes', () => {
  const origin = 1000000;
  const entry = (path, startTime, responseEnd, transferSize) => ({
    name: `http://127.0.0.1:18650${path}`,
    startTime,
    responseEnd,
    transferSize,
    encodedBodySize: 66574,
  });
  const entries = [
    entry('/api/sessions/search?q=plan', 100, 900, 300),
    entry('/api/sessions/search?q=plan', 9500, 12000, 300),
    entry('/api/sessions/status', 950, 980, 300),
  ];
  // The entries are in the observer's log, and the resource timing buffer is empty, as after the page's clear.
  const sandbox = pageSandbox({ performance: { timeOrigin: origin } });
  sandbox.__resources = entries;
  sandbox.__searchRenders = [
    { filter: 'search', repaint: false, wall0: origin + 500 },
    { filter: 'search', repaint: true, wall0: origin + 600 },
    { filter: 'all', repaint: false, wall0: origin + 700 },
    { filter: 'search', repaint: false, wall0: origin + 20000 },
  ];
  sandbox.__refetchCalls = [origin + 50, origin + 20000];
  const span = JSON.parse(vm.runInContext(
    `JSON.stringify((${READ_SPAN_SCRIPT})(${origin}, ${origin + 10000}))`,
    sandbox,
  ));
  assert.deepEqual(span, {
    refetch_calls: 1,
    search_fetches: 2,
    answers: 1,
    answer_wire_bytes: 300,
    answer_paints: 1,
    repaints: 1,
    status_polls: 1,
    status_bytes: 300,
    bytes_by_path: { '/api/sessions/search': 600, '/api/sessions/status': 300 },
  });
});
