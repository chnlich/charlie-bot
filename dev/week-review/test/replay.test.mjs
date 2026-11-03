// Contract tests for the offline replay of the pre-send check. Every row here is synthetic and lives in a temporary
// directory that the test removes. The check itself runs in Rust (`presend-check replay`); the batch runner is tested
// against a shell-script stand-in that prints the rows the test writes.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';

import {
  batchOrder,
  deleteFlagged,
  flagKey,
  halfOf,
  median,
  parseBatch,
  percentile,
  restoreOrder,
  runCheckBatch,
  runReplayRound,
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

function writeFile(file, body) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, body);
}

function writeLog(file, events) {
  writeFile(file, `${events.map((event) => JSON.stringify(event)).join('\n')}\n`);
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

test('a batch runs grouped by parent and child, and its rows go back to the order of the labels', () => {
  const bases = [
    { sender: 'p2', child: 'c1', sent_at: '2026-10-14T01:00:00Z', event_id: 'a' },
    { sender: 'p1', child: 'c2', sent_at: '2026-10-14T02:00:00Z', event_id: 'b' },
    { sender: 'p1', child: 'c1', sent_at: '2026-10-14T03:00:00Z', event_id: 'c' },
    { sender: 'p1', child: 'c1', sent_at: '2026-10-14T01:30:00Z', event_id: 'd' },
  ];
  const order = batchOrder(bases);
  assert.deepEqual(order.map((index) => bases[index].event_id), ['d', 'c', 'b', 'a']);
  const sentOrder = order.map((index) => bases[index].event_id);
  assert.deepEqual(restoreOrder(order, sentOrder), ['a', 'b', 'c', 'd']);
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
  const answered = (event_id, child, half, verdict) => ({
    event_id,
    child,
    half,
    state: 'sent',
    after_verdict: verdict,
    after_kinds: [],
    after_reason: '',
    error: null,
  });
  writeJsonl(path.join(roundDir, 'rejudge.jsonl'), [
    answered('t1', 'c1', 'tuning', 'needed'),
    answered('t2', 'c1', 'tuning', 'needed'),
    answered('u1', 'c2', 'test', 'needed'),
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

// The stub binaries of the end-to-end round: `charliebot presend-check replay` answers one row per stdin item, and
// `claude` answers one judge object per PARENT_MESSAGE id in the packet it reads. Both are shell scripts, so the
// round runs with no real binary and no model.
function writeStubBinaries(dir) {
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, 'charliebot'),
    [
      '#!/bin/sh',
      '# Stands in for `charliebot presend-check replay`: one row per stdin item, each with one flag.',
      'while IFS= read -r line; do',
      '  [ -n "$line" ] || continue',
      '  child=$(printf \'%s\\n\' "$line" | sed \'s/.*"child":"\\([^"]*\\)".*/\\1/\')',
      '  event=$(printf \'%s\\n\' "$line" | sed \'s/.*"event":"\\([^"]*\\)".*/\\1/\')',
      '  printf \'{"child":"%s","event":"%s","backend":"stub","check_ms":5,\' "$child" "$event"',
      '  printf \'"flags":[{"n":1,"kind":"background","quote":"flagged passage","reason":"stub"}]}\\n\'',
      'done',
      '',
    ].join('\n'),
  );
  fs.writeFileSync(
    path.join(dir, 'claude'),
    [
      '#!/bin/sh',
      '# Stands in for the judge: one answer per PARENT_MESSAGE id in the packet, every verdict `needed`.',
      'ids=$(grep -o \'PARENT_MESSAGE id=[^ ]*\' | sed \'s/^PARENT_MESSAGE id=//\' | sort -u)',
      'items=',
      'for id in $ids; do',
      '  [ -n "$items" ] && items="$items,"',
      '  items="$items$(printf \'{\\\\"id\\\\":\\\\"%s\\\\",\\\\"verdict\\\\":\\\\"needed\\\\",\\\\"off_goal_percent\\\\":0,' +
        '\\\\"off_goal_kinds\\\\":[],\\\\"off_goal_quote\\\\":\\\\"\\\\",\\\\"reason\\\\":\\\\"stub\\\\"}\' "$id")"',
      'done',
      'printf \'{"is_error":false,"result":"```json\\\\n[%s]\\\\n```"}\\n\' "$items"',
      '',
    ].join('\n'),
  );
  fs.chmodSync(path.join(dir, 'charliebot'), 0o755);
  fs.chmodSync(path.join(dir, 'claude'), 0o755);
  return dir;
}

// Two parents, two children, four messages: each half of the window holds one message per child, and every message
// carries the passage the stub check flags.
function replayFixture(name) {
  const root = path.join(scratch, name, 'sessions');
  writeFile(path.join(root, 'P', 'metadata.json'), JSON.stringify({ name: 'parent one' }));
  writeFile(path.join(root, 'Q', 'metadata.json'), JSON.stringify({ name: 'parent two' }));
  writeFile(
    path.join(root, 'C', 'metadata.json'),
    JSON.stringify({
      name: 'child one',
      task_parent_id: 'P',
      profile: 'worker',
      task: { goal: 'do the thing', acceptance: [], context_refs: [] },
    }),
  );
  writeFile(
    path.join(root, 'D', 'metadata.json'),
    JSON.stringify({
      name: 'child two',
      task_parent_id: 'Q',
      profile: 'manager',
      task: { goal: 'watch the other thing', acceptance: [], context_refs: [] },
    }),
  );
  writeLog(path.join(root, 'C', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'm1', from_session: 'P', timestamp: '2026-10-15T01:00:00.000Z',
      content: 'tuning work, flagged passage aside' },
    { type: 'agent_message', id: 'm2', from_session: 'P', timestamp: '2026-10-19T01:00:00.000Z',
      content: 'test work, flagged passage aside' },
  ]);
  writeLog(path.join(root, 'D', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'm3', from_session: 'Q', timestamp: '2026-10-16T01:00:00.000Z',
      content: 'other tuning, flagged passage aside' },
    { type: 'agent_message', id: 'm4', from_session: 'Q', timestamp: '2026-10-20T01:00:00.000Z',
      content: 'other test, flagged passage aside' },
  ]);
  writeLog(path.join(root, 'P', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'r1', from_session: 'C', timestamp: '2026-10-15T02:00:00.000Z', content: 'report one' },
  ]);
  writeLog(path.join(root, 'Q', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'r2', from_session: 'D', timestamp: '2026-10-16T02:00:00.000Z', content: 'report two' },
  ]);
  return path.join(scratch, name);
}

function replayRebuild(name) {
  const dir = path.join(scratch, `${name}-rebuild`);
  writeJsonl(path.join(dir, 'labels.jsonl'), [
    { event_id: 'm1', sender: 'P', target: 'C', sent_at: '2026-10-15T01:00:00.000Z', verdict: 'mixed',
      kinds: ['duplicate'], judge_backend: 'claude-opus-5-5' },
    { event_id: 'm2', sender: 'P', target: 'C', sent_at: '2026-10-19T01:00:00.000Z', verdict: 'unrelated',
      kinds: ['other'], judge_backend: 'claude-opus-5-5' },
    { event_id: 'm3', sender: 'Q', target: 'D', sent_at: '2026-10-16T01:00:00.000Z', verdict: 'needed',
      kinds: [], judge_backend: 'claude-opus-5-5' },
    { event_id: 'm4', sender: 'Q', target: 'D', sent_at: '2026-10-20T01:00:00.000Z', verdict: 'mixed',
      kinds: ['background'], judge_backend: 'claude-opus-5-5' },
  ]);
  fs.writeFileSync(
    path.join(dir, 'run.json'),
    JSON.stringify({ from: '2026-10-14T00:00:00Z', to: '2026-10-22T00:00:00Z', reports_until: '2026-10-22T00:00:00Z' }),
  );
  return dir;
}

test('a round runs end to end on the stub binaries, and a rerun of the round leaves no earlier file', async () => {
  const home = replayFixture('e2e');
  const rebuild = replayRebuild('e2e');
  const out = path.join(scratch, 'e2e-out');
  const bin = writeStubBinaries(path.join(scratch, 'e2e-bin'));
  // The round hashes the check instructions under the repo root it is given.
  writeFile(path.join(scratch, 'prompts', 'presend_check.md'), 'stub check instructions\n');
  const prompts = { manager: 'MANAGER PROMPT', worker: 'WORKER PROMPT' };
  const previousPath = process.env.PATH;
  process.env.PATH = `${bin}:${previousPath}`;
  try {
    const tally = await runReplayRound({
      out,
      rebuild,
      round: 1,
      home,
      charliebot: 'charliebot',
      repoRoot: scratch,
      checkParallel: 2,
      judgeParallel: 2,
      model: 'claude-opus-5-5',
      prompts,
      flipTest: true,
    });
    const roundDir = path.join(out, 'round1');
    assert.equal(tally.all.messages, 4);
    assert.deepEqual(tally.all, { messages: 4, bad: 3, bad_flagged: 3, good: 1, good_flagged: 1 });
    assert.deepEqual(tally.states, { sent: 4, not_sent: 0, check_failed: 0 });
    assert.equal(tally.test.after_judged, 2);
    assert.equal(tally.test.after_share, 0, 'the stub judge calls every edited message needed');
    assert.equal(tally.test.threshold_reached, true);
    assert.deepEqual(tally.flip, { pairs: 2, differ: 0, items: 2 });
    assert.equal(tally.latency_ms.n, 4);
    assert.deepEqual(tally.failures, { total: 0, by_half: {}, causes: [] });
    assert.equal(fs.existsSync(path.join(roundDir, 'flags.jsonl')), true);
    assert.equal(fs.existsSync(path.join(roundDir, 'flip.jsonl')), true);
    assert.equal(fs.readFileSync(path.join(roundDir, 'rejudge.jsonl'), 'utf8').split('\n').filter((l) => l !== '').length, 4);
    // A file from the earlier run that the second run does not produce.
    fs.writeFileSync(path.join(roundDir, 'stale-marker.txt'), 'from the first run');

    const rerun = await runReplayRound({
      out,
      rebuild,
      round: 1,
      home,
      charliebot: 'charliebot',
      repoRoot: scratch,
      checkParallel: 2,
      judgeParallel: 2,
      model: 'claude-opus-5-5',
      prompts,
      flipTest: false,
    });
    assert.equal(fs.existsSync(path.join(roundDir, 'stale-marker.txt')), false, 'the rerun removed the stale file');
    assert.equal(fs.existsSync(path.join(roundDir, 'flip.jsonl')), false, 'the rerun left no flip file behind');
    assert.equal(rerun.flip, null, 'the rerun tally reads no flip file');
    assert.equal(rerun.all.messages, 4);
  } finally {
    process.env.PATH = previousPath;
  }
});
