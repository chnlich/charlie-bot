// Tests of the bench's workload and report arithmetic: the two builds must run one deterministic workload whose kind
// mix, restart share and fault placement are the properties the evidence rests on.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  buildPlan,
  chooseRestarts,
  crossingAt,
  FAULT_LEAD_SECONDS,
  kindCounts,
  KIND_SHARES,
  loadW2Waits,
  pickLengths,
  RESTART_SHARE_RANGE,
} from './plan.mjs';
import { faultAllowanceMs, lengthBin } from './report.mjs';
import { intervalInForceMs, PROBE_INTERVALS_SECONDS, predictedFireSeconds } from './probe-schedule.mjs';

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

test('a restart crosses a wait until its predicted fire, not only while its job runs', () => {
  // A 100 s job registers at 0 and ends at 100. Today's schedule probes at 0, 15, 40, 85 and 170 with mean noise, so a
  // restart at 120 s finds the job finished and the wake still pending.
  const wait = { index: 0, startOffset: 0, length: 100 };
  assert.equal(predictedFireSeconds(0, 100), 170);
  assert.equal(crossingAt([wait], 120).length, 1);
  assert.equal(crossingAt([wait], 180).length, 0);
});

test('a boundary crosses two of ten waits, a fifth of the plan, not an idle server', () => {
  // Ten waits start every 10 s and run for 300 s, so each wake is predicted at 335 s after its registration. The first
  // restart that crosses exactly two waits is the one the planner takes for a 20% share.
  const waits = Array.from({ length: 10 }, (_, index) => ({ index, startOffset: index * 10, length: 300 }));
  const { boundaries, share } = chooseRestarts(waits, 1, 0.225, 0);
  assert.equal(boundaries.length, 1);
  assert.equal(crossingAt(waits, boundaries[0]).length, 2);
  assert.ok(share >= RESTART_SHARE_RANGE[0] && share <= RESTART_SHARE_RANGE[1], `share ${share}`);
});

test('a faulted wake is allowed the length of the union of the faults it overlaps', () => {
  const cut = { fromMs: 1_000, toMs: 61_000 };
  const hold = { fromMs: 1_000, toMs: 301_000 };
  const later = { fromMs: 100_000, toMs: 160_000 };
  // Faults that start together count once, at the longer one's length.
  assert.equal(faultAllowanceMs([cut, hold], 0, 500_000), 300_000);
  // Separate faults add up.
  assert.equal(faultAllowanceMs([cut, later], 0, 500_000), 120_000);
  // A wake that ends before a fault starts, or starts after it ends, is allowed nothing.
  assert.equal(faultAllowanceMs([cut, hold], 400_000, 500_000), 0);
  assert.equal(faultAllowanceMs([cut, hold], 0, 1_000), 0);
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

test('the smoke plan takes one restart and both faults land inside a running job', () => {
  // The default seed's ten-wait plan takes no restart, so the smoke names the first seed after it that does.
  const plan = buildPlan({
    waits: 10,
    seed: 20261112,
    staggerSeconds: 120,
    restarts: 1,
    faults: ['cut', 'hold'],
    cutSeconds: 60,
    holdSeconds: 120,
    lengthCap: 300,
  });
  assert.equal(plan.restarts.length, 1);
  for (const fault of plan.faults) {
    const wait = plan.waits[fault.waitIndex];
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
