// Contract tests for the offline replay of the pre-send check. Every row here is synthetic and lives in a temporary
// directory that the test removes. The check itself runs in Rust (`presend-check replay`); the batch runner is tested
// against a shell-script stand-in that prints the rows the test writes.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';

import {
  deleteFlagged,
  flagKey,
  halfOf,
  median,
  parseBatch,
  percentile,
  runCheckBatch,
  tallyRound,
} from '../lib/replay.mjs';

let scratch;

before(() => {
  scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'week-review-replay-test-'));
});

after(() => {
  fs.rmSync(scratch, { recursive: true, force: true });
});

function writeJsonl(file, rows) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, rows.map((row) => `${JSON.stringify(row)}\n`).join(''));
}

test('deleting the flagged passages cuts every occurrence out and leaves the rest of the message', () => {
  const content = 'HostKey 三项。\n收到，谢谢。\nHostKey 三项。\n请装 sshd。';
  const flags = [
    { kind: 'repeat', quote: 'HostKey 三项。', reason: 'sent before' },
    { kind: 'acknowledgement', quote: '收到，谢谢。', reason: 'only receipt' },
  ];
  assert.equal(deleteFlagged(content, flags), '请装 sshd。');
  assert.equal(deleteFlagged('收到。', [{ kind: 'acknowledgement', quote: '收到。', reason: 'x' }]), '');
});

test('the tuning half is the messages sent before 10-18 UTC, the test half the rest', () => {
  assert.equal(halfOf('2026-10-14T20:24:49Z'), 'tuning');
  assert.equal(halfOf('2026-10-17T23:59:59Z'), 'tuning');
  assert.equal(halfOf('2026-10-18T00:00:00Z'), 'test');
  assert.equal(halfOf('2026-10-21T20:29:57Z'), 'test');
});

test('the median averages the two middle values of an even list, and the percentile is nearest-rank', () => {
  assert.equal(median([1, 2, 3]), 2);
  assert.equal(median([1, 2, 3, 4]), 2.5);
  assert.equal(median([]), null);
  const ten = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
  assert.equal(percentile(ten, 0.9), 9);
  assert.equal(percentile(ten, 1), 10);
});

test('two checks agree on the flag set when the kinds and quotes match, whatever the reasons say', () => {
  const first = [{ kind: 'repeat', quote: 'a', reason: 'one reason' }];
  const second = [{ kind: 'repeat', quote: 'a', reason: 'another reason' }];
  assert.equal(flagKey(first), flagKey(second));
  assert.notEqual(flagKey(first), flagKey([{ kind: 'background', quote: 'a', reason: 'x' }]));
});

test('a batch answer is accepted only when it has one row per item, in item order', () => {
  const items = [
    { child: 'c1', event: 'e1' },
    { child: 'c2', event: 'e2' },
  ];
  const rows = [
    { child: 'c1', event: 'e1', backend: 'stub', check_ms: 5, flags: [] },
    { child: 'c2', event: 'e2', error: 'every light backend failed' },
  ];
  assert.deepEqual(parseBatch(rows.map((row) => JSON.stringify(row)).join('\n'), items), rows);
  assert.throws(() => parseBatch(JSON.stringify(rows[0]), items), /answered 1 of 2 items/);
  const swapped = [rows[1], rows[0]].map((row) => JSON.stringify(row)).join('\n');
  assert.throws(() => parseBatch(swapped, items), /not the item sent/);
});

test('the batch runner passes the items on stdin and returns the rows the command prints', async () => {
  const items = [
    { child: 'c1', event: 'e1' },
    { child: 'c2', event: 'e2' },
  ];
  const rowsFile = path.join(scratch, 'batch-rows.jsonl');
  writeJsonl(rowsFile, [
    { child: 'c1', event: 'e1', backend: 'stub', check_ms: 7, flags: [] },
    { child: 'c2', event: 'e2', backend: 'stub', check_ms: 9, flags: [] },
  ]);
  // A shell-script stand-in for `charliebot presend-check replay`: it reads the items, then prints the rows the
  // test wrote.
  const stub = path.join(scratch, 'charliebot-stub');
  fs.writeFileSync(stub, '#!/bin/sh\ncat > /dev/null\ncat "$STUB_BATCH_ROWS"\n');
  fs.chmodSync(stub, 0o755);
  const previous = process.env.STUB_BATCH_ROWS;
  process.env.STUB_BATCH_ROWS = rowsFile;
  try {
    const rows = await runCheckBatch({ items, charliebot: stub, home: scratch, repoRoot: scratch, parallel: 2 });
    assert.deepEqual(
      rows.map((row) => row.check_ms),
      [7, 9],
    );
  } finally {
    if (previous === undefined) delete process.env.STUB_BATCH_ROWS;
    else process.env.STUB_BATCH_ROWS = previous;
  }
});

test('the tally counts the baseline, the edited test half, the flips and the failures of a round', () => {
  const roundDir = path.join(scratch, 'round1');
  const flags = [{ n: 1, kind: 'repeat', quote: 'x', reason: 'r' }];
  const sent = (overrides) => ({
    sender: 'p',
    check: 'ok',
    backend: 'stub',
    flags: [],
    error: null,
    state: 'sent',
    edited: 'y',
    ...overrides,
  });
  const rows = [
    // Tuning half: a bad message flagged, a good one not.
    sent({ event_id: 't1', child: 'c1', sent_at: '2026-10-14T01:00:00Z', half: 'tuning', label_verdict: 'mixed',
      check_ms: 10, flags }),
    sent({ event_id: 't2', child: 'c1', sent_at: '2026-10-14T02:00:00Z', half: 'tuning', label_verdict: 'needed',
      check_ms: 20 }),
    // Test half: a wholly unrelated message that the edit leaves needing nothing, and a failed check.
    sent({ event_id: 'u1', child: 'c2', sent_at: '2026-10-20T01:00:00Z', half: 'test', label_verdict: 'unrelated',
      check_ms: 30, flags }),
    sent({ event_id: 'f1', child: 'c2', sent_at: '2026-10-20T02:00:00Z', half: 'test', label_verdict: 'needed',
      check: 'failed', backend: null, check_ms: null, state: 'check_failed', edited: null,
      error: 'every light backend failed' }),
  ];
  writeJsonl(path.join(roundDir, 'flags.jsonl'), rows);
  const answered = (event_id, half, verdict) => ({
    event_id,
    child: 'x',
    half,
    state: 'sent',
    after_verdict: verdict,
    after_kinds: [],
    after_reason: '',
    error: null,
  });
  writeJsonl(path.join(roundDir, 'rejudge.jsonl'), [
    answered('t1', 'tuning', 'needed'),
    answered('t2', 'tuning', 'needed'),
    answered('u1', 'test', 'needed'),
    {
      event_id: 'f1',
      child: 'c2',
      half: 'test',
      state: 'check_failed',
      after_verdict: null,
      after_kinds: null,
      after_reason: null,
      error: 'check failed: every light backend failed',
    },
  ]);
  writeJsonl(path.join(roundDir, 'flip.jsonl'), [
    { event_id: 'u1', child: 'c2', half: 'test', check: 'ok', backend: 'stub', check_ms: 31,
      flags: [{ n: 1, kind: 'other', quote: 'x', reason: 'r' }], error: null },
    { event_id: 'f1', child: 'c2', half: 'test', check: 'ok', backend: 'stub', check_ms: 12, flags: [], error: null },
  ]);
  fs.writeFileSync(
    path.join(roundDir, 'run.json'),
    JSON.stringify({
      round: 1,
      prompt_sha256: 'abc',
      states: { sent: 3, not_sent: 0, check_failed: 1 },
      check_items: 4,
      flip_check_items: 2,
      judge_model: 'claude-opus-5-5',
      judge_packets: 2,
      judge_packets_failed: 0,
    }),
  );

  const tally = tallyRound(roundDir);
  assert.deepEqual(tally.all, { messages: 4, bad: 2, bad_flagged: 2, good: 2, good_flagged: 0 });
  assert.equal(tally.test.messages, 2);
  assert.equal(tally.test.baseline_unneeded, 1);
  assert.equal(tally.test.baseline_unrelated, 1);
  assert.equal(tally.test.after_judged, 1);
  assert.equal(tally.test.after_unneeded, 0);
  assert.equal(tally.test.after_share, 0);
  assert.equal(tally.test.threshold_reached, true);
  assert.equal(tally.test.failed, 1);
  assert.deepEqual(tally.flip, { pairs: 1, differ: 1, items: 2 });
  assert.equal(tally.latency_ms.n, 3);
  assert.equal(tally.latency_ms.median, 20);
  assert.equal(tally.failures.total, 1);
});
