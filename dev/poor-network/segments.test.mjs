// Tests of the bench's segments: the rate verdict, the probe that starts each segment, the rerun of a probe that fails
// the gate, exit status 5 when the rerun fails too, the For-you branch's load wait before a reprobe, and the probe that
// a yield makes due again.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { LIVE_CPU_TAIL } from './profile.mjs';
import {
  LOAD_WAIT_CAP_MS,
  LOAD_WAIT_SAMPLE_MS,
  RateGateError,
  RATE_MAX,
  RATE_MIN,
  Segments,
  rateVerdict,
} from './segments.mjs';

// A reference repetition whose CPU tail reads as `rate` on the live device's tail.
function probeAt(rate) {
  return {
    rep: 1,
    tailMs: LIVE_CPU_TAIL.medianMs / rate,
    documentTransferBytes: 1,
    documentBodyBytes: 2,
    navigationError: null,
    networkChanged: [],
    openInHoldMs: null,
  };
}

// A gate that runs each step in place and keeps every unit. The test scripts each step's yield in run order: a record
// when the step gave the lock up, null when it did not.
function fakeGate(yields = []) {
  const units = [];
  return {
    units,
    async step(kind, name, body) {
      const value = await body();
      const unit = { kind, name, status: 'ok', attempt: 1 };
      units.push(unit);
      const record = yields.length > 0 ? yields.shift() : null;
      return { status: 'ok', value, unit, yielded: record !== null };
    },
  };
}

// A fake load clock for the For-you branch's wait: each reading comes from `readings` with the last one repeating, and
// a sleep advances the clock by its time, so a test's wait costs no real time.
function fakeLoadWait(readings) {
  const state = { clock: 0, index: 0, reads: 0 };
  return {
    state,
    load1: () => {
      state.reads += 1;
      return readings[Math.min(state.index++, readings.length - 1)];
    },
    now: () => state.clock,
    sleep: async (ms) => {
      state.clock += ms;
    },
  };
}

function segmentsWith(gate, rates, reference = null, loadWait = fakeLoadWait([0])) {
  const queue = rates.map((rate) => probeAt(rate));
  let probes = 0;
  const segments = new Segments({
    gate,
    probe: async () => {
      probes += 1;
      return queue.shift();
    },
    side: 'after',
    reference,
    loadWait,
  });
  return { segments, probes: () => probes };
}

test('the rate gate passes a rate inside the range and fails one outside it', () => {
  assert.equal(rateVerdict(3.5).ok, true);
  assert.equal(rateVerdict(4.9).ok, true, 'a rate a host reads at moderate load');
  assert.equal(rateVerdict(RATE_MIN - 0.1).ok, false, 'below the range');
  assert.equal(rateVerdict(RATE_MAX + 0.1).ok, false, 'above the range');
});

test('a probe that fails the gate is discarded and run once more; the passing probe starts the segment', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [RATE_MAX + 0.5, 4.0]);
  const value = await segments.unit('rep', 'background 1', () => 'measured');
  assert.equal(value.status, 'ok');
  assert.equal(probes(), 2);
  const [discarded, passed, rep] = gate.units;
  assert.equal(discarded.kind, 'probe');
  assert.equal(discarded.discarded, true);
  assert.match(discarded.reason, /outside/);
  assert.equal(passed.kind, 'probe');
  assert.equal(passed.discarded, undefined);
  assert.equal(passed.segment, 1);
  assert.equal(rep.kind, 'rep');
  assert.equal(rep.segment, 1);
  assert.equal(rep.probeRate, 4.0);
});

test('two probes that fail the gate end the run with exit status 5', async () => {
  const gate = fakeGate();
  const { segments } = segmentsWith(gate, [RATE_MAX + 0.5, RATE_MAX + 0.6]);
  await assert.rejects(segments.unit('rep', 'background 1', () => 'never'), (error) => {
    assert.ok(error instanceof RateGateError);
    assert.equal(error.exitCode, 5);
    assert.match(error.message, /failed the rate gate 2 times/);
    return true;
  });
  assert.equal(gate.units.filter((unit) => unit.discarded).length, 2);
});

test('each segment calibrates on its own: its units carry its own probe rate', async () => {
  // The first repetition's step gives the lock up, so the second repetition starts a new segment with its own probe.
  const gate = fakeGate([null, { waiterPids: [42], seenToReleaseMs: 3 }]);
  const { segments } = segmentsWith(gate, [3.0, 4.6]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  const reps = gate.units.filter((unit) => unit.kind === 'rep');
  assert.deepEqual(reps.map((unit) => unit.probeRate), [3.0, 4.6]);
  assert.equal(segments.reference, 3.0, 'the first accepted rate is recorded');
});

test('a probe inside the range is accepted whatever rate the invocation shares', async () => {
  const gate = fakeGate();
  const { segments } = segmentsWith(gate, [5.5], 4.0);
  const value = await segments.unit('rep', 'held 1', () => 'measured');
  assert.equal(value.status, 'ok');
  assert.equal(segments.reference, 4.0, 'the shared rate is recorded, not enforced');
  assert.equal(gate.units.filter((unit) => unit.kind === 'rep')[0].probeRate, 5.5);
});

test('a probe runs once per segment: a step without a yield does not start a new one', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [4.0]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 1);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'rep']);
});

test('a yield makes the next unit start with a new probe; the reference stays the first accepted rate', async () => {
  // The probe's step gives no lock up; the first repetition's step does.
  const gate = fakeGate([null, { waiterPids: [42], seenToReleaseMs: 3 }]);
  const { segments, probes } = segmentsWith(gate, [4.0, 3.9]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'probe', 'rep']);
  assert.equal(segments.reference, 4.0, 'the first accepted rate is recorded');
  assert.equal(gate.units[3].segment, 2);
});

test('a probe whose step gives the lock up is followed by a new probe before the unit runs', async () => {
  const gate = fakeGate([{ waiterPids: [42], seenToReleaseMs: 3 }]);
  const { segments, probes } = segmentsWith(gate, [4.0, 3.9]);
  await segments.unit('rep', 'background 1', () => 1);
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'probe', 'rep']);
  assert.equal(gate.units[2].segment, 2, 'the unit runs in the segment its new probe started');
  assert.equal(segments.reference, 4.0);
});

test('a For-you probe below the rate floor waits for load1 <= 20 and is then run again', async () => {
  const loadWait = fakeLoadWait([26, 23, 19, 19, 19]);
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [RATE_MIN - 0.5, 4.0], null, loadWait);
  const value = await segments.unit('cell', 'foryou switch large 1', () => 'measured');
  assert.equal(value.status, 'ok');
  assert.equal(probes(), 2, 'the probe is re-taken once the load is down');
  const [discarded, passed] = gate.units;
  assert.equal(discarded.discarded, true);
  assert.equal(discarded.loadWaitMs, 4 * LOAD_WAIT_SAMPLE_MS, 'three readings in range, 30 s apart');
  assert.deepEqual(discarded.loadWaitLoad1, [26, 23, 19, 19, 19]);
  assert.equal(passed.kind, 'probe');
  assert.equal(passed.discarded, undefined);
  assert.equal(passed.segment, 1);
});

test('a probe below the rate floor outside the For-you branch keeps the one-rerun rule and reads no load', async () => {
  const loadWait = fakeLoadWait([25]);
  const gate = fakeGate();
  const { segments } = segmentsWith(gate, [RATE_MIN - 0.5, RATE_MIN - 0.4], null, loadWait);
  await assert.rejects(segments.unit('rep', 'background 1', () => 'never'), RateGateError);
  assert.equal(loadWait.state.reads, 0);
});

test('a For-you probe above the rate max keeps the one-rerun rule: the wait covers the floor alone', async () => {
  const loadWait = fakeLoadWait([19]);
  const gate = fakeGate();
  const { segments } = segmentsWith(gate, [RATE_MAX + 0.5, RATE_MAX + 0.6], null, loadWait);
  await assert.rejects(segments.unit('cell', 'foryou switch large 1', () => 'never'), (error) => {
    assert.match(error.message, /failed the rate gate 2 times/);
    return true;
  });
  assert.equal(loadWait.state.reads, 0);
});

test('the For-you load wait ends the run when its 30 min cap runs out with the load still high', async () => {
  const loadWait = fakeLoadWait([25]);
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [RATE_MIN - 0.5, 4.0], null, loadWait);
  await assert.rejects(segments.unit('cell', 'foryou switch large 1', () => 'never'), (error) => {
    assert.ok(error instanceof RateGateError);
    assert.equal(error.exitCode, 5);
    assert.match(error.message, /30 min cap/);
    return true;
  });
  assert.equal(probes(), 1, 'the wait is exhausted instead of a passing probe');
  assert.equal(gate.units[0].loadWaitMs, LOAD_WAIT_CAP_MS);
  assert.equal(gate.units[0].loadWaitLoad1.at(-1), 25);
});
