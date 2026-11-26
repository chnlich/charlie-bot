// Tests of the bench's segments: the rate verdict (a range, not a band), the probe that starts each segment inside
// its unit's step, the rerun of a probe that fails the gate, exit status 5 when the rerun fails too, the reads and
// median a row carries, the probe that a yield makes due again, and a bench that yields after every step measuring a
// cell per hold.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { LIVE_CPU_TAIL } from './profile.mjs';
import { RateGateError, RATE_MAX, RATE_MIN, Segments, rateVerdict } from './segments.mjs';

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
// when the step gave the lock up, null when it did not. `yieldEveryStep` gives every step up, as a queued bench makes
// the gate do, and `calls` records every step, so a test can see how many holds a unit took.
function fakeGate({ yields = [], yieldEveryStep = false } = {}) {
  const units = [];
  const calls = [];
  return {
    units,
    calls,
    async step(kind, name, body) {
      calls.push({ kind, name });
      const value = await body();
      const unit = { kind, name, status: 'ok', attempt: 1 };
      units.push(unit);
      const record = yieldEveryStep ? { waiterPids: [42], seenToReleaseMs: 3 } : yields.shift() ?? null;
      return { status: 'ok', value, unit, yielded: record !== null };
    },
  };
}

// Segments over scripted probes: the k-th probe reads rates[k - 1] and its load1 is k, because the load is sampled
// before each probe's read.
function segmentsWith(gate, rates) {
  const queue = rates.map((rate) => probeAt(rate));
  let probes = 0;
  const segments = new Segments({
    gate,
    probe: async () => {
      probes += 1;
      return queue.shift();
    },
    loadAverage: () => probes + 1,
    side: 'after',
  });
  return { segments, probes: () => probes };
}

test('the rate gate passes a rate inside the range whatever its ratio to other probes, and fails outside it', () => {
  assert.equal(rateVerdict(RATE_MIN).ok, true, 'the range includes both ends');
  assert.equal(rateVerdict(3.5).ok, true);
  assert.equal(rateVerdict(4.9).ok, true, 'a rate a host reads at moderate load');
  assert.equal(rateVerdict(RATE_MAX).ok, true);
  assert.equal(rateVerdict(RATE_MIN - 0.1).ok, false, 'below the range');
  assert.equal(rateVerdict(RATE_MAX + 0.1).ok, false, 'above the range');
});

test('a probe at 1.5x the first segment\'s rate passes, because both rates are inside the range', async () => {
  const gate = fakeGate({ yields: [{ waiterPids: [42], seenToReleaseMs: 3 }] });
  const { segments, probes } = segmentsWith(gate, [4.0, 6.0]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'probe', 'rep']);
  assert.equal(gate.units.filter((unit) => unit.discarded).length, 0);
  assert.equal(gate.units[3].segment, 2);
  assert.equal(gate.units[3].probeMedian, 6.0);
});

test('a probe that fails the gate is discarded and run once more; the passing probe starts the segment', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [RATE_MAX + 0.5, 4.0]);
  const value = await segments.unit('rep', 'background 1', () => 'measured');
  assert.equal(value.status, 'ok');
  assert.equal(value.value, 'measured');
  assert.equal(probes(), 2);
  const [discarded, passed, rep] = gate.units;
  assert.equal(discarded.kind, 'probe');
  assert.equal(discarded.discarded, true);
  assert.equal(discarded.segment, 1);
  assert.match(discarded.reason, /outside/);
  assert.equal(passed.kind, 'probe');
  assert.equal(passed.discarded, undefined);
  assert.equal(passed.segment, 1);
  assert.equal(passed.probeRate, 4.0);
  assert.equal(rep.kind, 'rep');
  assert.equal(rep.segment, 1);
  assert.deepEqual(rep.probeReads, [{ rate: RATE_MAX + 0.5, load1: 1 }, { rate: 4.0, load1: 2 }],
    'the discarded read is one of the segment\'s reads');
  assert.equal(rep.probeMedian, 4.0, 'the median of two reads is the lower middle value');
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
  assert.equal(gate.calls.length, 1, 'a rate failure throws inside the unit\'s step');
});

test('a probe below the range is run once more, then exits 5 as one above it does', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [RATE_MIN - 0.5, RATE_MIN - 0.5]);
  await assert.rejects(segments.unit('rep', 'held 1', () => 'never'), (error) => {
    assert.ok(error instanceof RateGateError);
    assert.equal(error.exitCode, 5);
    return true;
  });
  assert.equal(probes(), 2);
});

test('a probe runs once per segment: a step without a yield does not start a new one', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [4.0]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 1);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'rep']);
});

test('the probe runs inside its unit\'s step, so one hold covers the probe and the cell', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [4.0]);
  const outcome = await segments.unit('rep', 'background 1', () => 'measured');
  assert.equal(outcome.status, 'ok');
  assert.equal(outcome.value, 'measured');
  assert.equal(probes(), 1);
  assert.deepEqual(gate.calls.map((call) => call.kind), ['rep'], 'the probe takes no step of its own');
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep']);
  assert.equal(gate.units[0].segment, 1);
  assert.equal(gate.units[0].probeRate, 4.0);
  assert.equal(gate.units[1].segment, 1);
});

test('a capped step starts no segment: the probes stay in the units and the next unit probes again', async () => {
  const gate = {
    units: [],
    async step(kind, name, body) {
      await body();
      return { status: 'capped', unit: { kind, name, status: 'capped', attempt: 2 }, yielded: false };
    },
  };
  const { segments, probes } = segmentsWith(gate, [4.0, 4.0]);
  const capped = await segments.unit('rep', 'background 1', () => 'capped');
  assert.equal(capped.status, 'capped');
  await segments.unit('rep', 'background 2', () => 'capped');
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.filter((unit) => unit.kind === 'probe').map((unit) => unit.segment), [1, 1],
    'the capped step\'s probe does not advance the segment');
});

test('a capped step\'s probe read belongs to the segment that the next step starts', async () => {
  // The first step passes its cap after its probe has run; the second step finishes inside its cap.
  const outcomes = ['capped', 'ok'];
  const units = [];
  const gate = {
    units,
    async step(kind, name, body) {
      const value = await body();
      const status = outcomes.shift();
      const unit = { kind, name, status, attempt: 1 };
      units.push(unit);
      return status === 'ok' ? { status, value, unit, yielded: false } : { status, unit, yielded: false };
    },
  };
  const { segments, probes } = segmentsWith(gate, [4.0, 3.9]);
  const capped = await segments.unit('rep', 'background 1', () => 'capped');
  assert.equal(capped.status, 'capped');
  const outcome = await segments.unit('rep', 'background 2', () => 'measured');
  assert.equal(outcome.status, 'ok');
  assert.equal(probes(), 2);
  assert.equal(outcome.unit.segment, 1);
  assert.deepEqual(outcome.unit.probeReads, [{ rate: 4.0, load1: 1 }, { rate: 3.9, load1: 2 }]);
  assert.equal(outcome.unit.probeMedian, 3.9);
});

test('a yield makes the next unit start with a new probe; each segment\'s rows carry its own median', async () => {
  // The first step gives the lock up; the second does not.
  const gate = fakeGate({ yields: [{ waiterPids: [42], seenToReleaseMs: 3 }] });
  const { segments, probes } = segmentsWith(gate, [4.0, 3.9]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'probe', 'rep']);
  assert.equal(gate.units[1].probeMedian, 4.0);
  assert.equal(gate.units[3].segment, 2);
  assert.deepEqual(gate.units[3].probeReads, [{ rate: 3.9, load1: 2 }]);
  assert.equal(gate.units[3].probeMedian, 3.9);
});

test('a bench that yields after every step measures one cell per lock hold', async () => {
  // Every step ends with a process queued for the lock, as run 20261124T151626Z had: the probe's own step yielded
  // before any cell ran, so the unit probed again and again. The probe limit fails a regression instead of hanging it.
  const gate = fakeGate({ yieldEveryStep: true });
  const LIMIT = 40;
  let probes = 0;
  const segments = new Segments({
    gate,
    side: 'after',
    loadAverage: () => 1,
    probe: async () => {
      probes += 1;
      assert.ok(probes <= LIMIT, `the bench probed ${probes} times without measuring a cell`);
      return probeAt(4.0);
    },
  });
  const cells = [];
  for (let i = 1; i <= 18; i++) {
    const outcome = await segments.unit('rep', `background ${i}`, () => i);
    assert.equal(outcome.status, 'ok');
    cells.push(outcome.value);
  }
  assert.deepEqual(cells, Array.from({ length: 18 }, (_, i) => i + 1), 'a cell ran in every hold');
  assert.equal(probes, 18);
  assert.deepEqual(gate.calls.map((call) => call.kind), Array(18).fill('rep'), 'one hold per cell');
  assert.deepEqual(gate.units.map((unit) => unit.kind), Array(18).fill(['probe', 'rep']).flat());
  assert.deepEqual(gate.units.filter((unit) => unit.kind === 'probe').map((unit) => unit.segment),
    Array.from({ length: 18 }, (_, i) => i + 1));
});
