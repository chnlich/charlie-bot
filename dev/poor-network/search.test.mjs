// The sidebar search view's metric math: the broadcast offsets, the e2e and overlap arithmetic, the body check, and the
// summary's medians and discard totals. The page-side reads are checked by running the view, not here.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { SEARCH } from './profile.mjs';
import { bodyCheck, broadcastOffsets, overlapSummary, searchRecord, summarizeSearch } from './search.mjs';

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
  assert.equal(row.check.within, true);
});
