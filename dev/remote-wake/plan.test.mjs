// Tests of the bench's workload and report arithmetic: the two builds must run one deterministic workload whose kind
// mix, restart share and fault placement are the properties the evidence rests on.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { buildPlan, chooseRestarts, kindCounts, loadW2Waits, pickLengths, FAULT_LEAD_SECONDS, KIND_SHARES, RESTART_SHARE_RANGE } from './plan.mjs';
import { intervalInForceMs, lengthBin, PROBE_INTERVALS_SECONDS } from './report.mjs';

const PLAN_OPTIONS = Object.freeze({
  waits: 100,
  seed: 20261108,
  staggerSeconds: 3600,
  restarts: 3,
  faults: ['cut', 'hold'],
  cutSeconds: 60,
  holdSeconds: 300,
});

test('the W2 length list holds the production window\'s 104 waits', () => {
  const waits = loadW2Waits();
  assert.equal(waits.length, 104);
  assert.ok(waits.every((value) => Number.isInteger(value) && value >= 0));
});

test('the kind mix follows the W2 shares and gives every kind a wait', () => {
  const counts = kindCounts(100);
  assert.deepEqual(counts, { pid: 71, cmd: 4, slurm: 25 });
  for (const kind of Object.keys(KIND_SHARES)) {
    assert.ok(kindCounts(10)[kind] >= 1, `${kind} has no wait in a 10-wait plan`);
  }
});

test('lengths keep the floor and the cap', () => {
  const random = () => 0.5;
  const lengths = pickLengths([0, 1, 4, 7201, 300], 5, random, 7200);
  assert.deepEqual([...lengths].sort((a, b) => a - b), [5, 5, 5, 300, 7200]);
});

test('the same arguments produce the same plan', () => {
  assert.deepEqual(buildPlan(PLAN_OPTIONS), buildPlan(PLAN_OPTIONS));
});

test('the restart boundaries put the crossed share inside the production range', () => {
  const plan = buildPlan(PLAN_OPTIONS);
  assert.ok(plan.restarts.length > 0);
  assert.ok(
    plan.restartShare >= RESTART_SHARE_RANGE[0] && plan.restartShare <= RESTART_SHARE_RANGE[1],
    `restart share ${plan.restartShare} is outside ${RESTART_SHARE_RANGE}`,
  );
  for (const boundary of plan.restarts) {
    assert.ok(boundary >= PLAN_OPTIONS.staggerSeconds / 4, `restart at ${boundary} falls inside the first quarter`);
  }
});

test('a boundary restarts a server with waits in flight, not an idle one', () => {
  // Ten waits start one after another and each runs for 300 s: a late boundary crosses two of them, a fifth of the
  // plan, which is the share the production window showed.
  const waits = Array.from({ length: 10 }, (_, index) => ({ index, startOffset: index * 10, length: 300 }));
  const { boundaries, share } = chooseRestarts(waits, 1, 0.225, 0);
  assert.equal(boundaries.length, 1);
  assert.ok(boundaries[0] > 300 && boundaries[0] < 400);
  assert.ok(share >= RESTART_SHARE_RANGE[0] && share <= RESTART_SHARE_RANGE[1], `share ${share}`);
});

test('both faults start together after every registration and inside a running wait', () => {
  const plan = buildPlan(PLAN_OPTIONS);
  const lastStart = Math.max(...plan.waits.map((wait) => wait.startOffset));
  const lastRestart = Math.max(0, ...plan.restarts);
  assert.equal(plan.faults.length, 2);
  for (const fault of plan.faults) {
    assert.equal(fault.startOffset, Math.max(lastStart, lastRestart) + FAULT_LEAD_SECONDS);
    const wait = plan.waits.find((item) => item.index === fault.waitIndex);
    assert.ok(wait.startOffset < fault.startOffset && fault.startOffset < wait.startOffset + wait.length);
  }
});

test('no restart boundary falls inside the fault window', () => {
  const plan = buildPlan({ ...PLAN_OPTIONS, holdSeconds: 300 });
  const from = Math.min(...plan.faults.map((fault) => fault.startOffset));
  const to = Math.max(...plan.faults.map((fault) => fault.startOffset + fault.seconds));
  for (const boundary of plan.restarts) {
    assert.ok(boundary < from || boundary > to, `restart at ${boundary} falls inside the fault window ${from}-${to}`);
  }
});

test('the probe interval in force follows the cumulative schedule', () => {
  assert.equal(intervalInForceMs(0), 10_000);
  assert.equal(intervalInForceMs(10_000), 10_000);
  assert.equal(intervalInForceMs(10_001), 20_000);
  assert.equal(intervalInForceMs(30_000), 20_000);
  assert.equal(intervalInForceMs(70_000), 40_000);
  const last = PROBE_INTERVALS_SECONDS.reduce((sum, interval) => sum + interval, 0) * 1000;
  assert.equal(intervalInForceMs(last + 1_000), 600_000);
  assert.equal(intervalInForceMs(5_000_000), 600_000);
});

test('length bins cover the production split', () => {
  assert.equal(lengthBin(0), '0-70');
  assert.equal(lengthBin(70), '0-70');
  assert.equal(lengthBin(71), '70-310');
  assert.equal(lengthBin(310), '70-310');
  assert.equal(lengthBin(311), '310-630');
  assert.equal(lengthBin(631), 'over-630');
});
