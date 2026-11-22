// Tests of the bench's segments: the rate verdict, the probe that starts each segment, the rerun of a probe that fails
// the gate, exit status 5 when the rerun fails too, and the probe that a yield makes due again.

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

// A gate that runs each step in place and keeps every unit, with yields scripted by the test.
function fakeGate(yields = []) {
  const units = [];
  return {
    units,
    async step(kind, name, body) {
      const value = await body();
      const unit = { kind, name, status: 'ok', attempt: 1 };
      units.push(unit);
      return { status: 'ok', value, unit };
    },
    async yieldIfWaited() {
      return yields.length > 0 ? yields.shift() : null;
    },
  };
}

function segmentsWith(gate, rates, reference = null) {
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
  });
  return { segments, probes: () => probes };
}

test('the rate gate passes a rate inside the range and inside the band of the cells, and fails either outside', () => {
  assert.equal(rateVerdict(3.5, null).ok, true);
  assert.equal(rateVerdict(RATE_MIN - 0.1, null).ok, false, 'below the range');
  assert.equal(rateVerdict(RATE_MAX + 0.1, null).ok, false, 'above the range');
  assert.equal(rateVerdict(3.8, 4.0).ok, true, 'inside the band');
  assert.equal(rateVerdict(4.0, 3.5).ok, false, 'more than 10 % from the cells rate');
});

test('a probe that fails the gate is discarded and run once more; the passing probe starts the segment', async () => {
  const gate = fakeGate();
  const { segments, probes } = segmentsWith(gate, [5.0, 4.0]);
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
  const { segments } = segmentsWith(gate, [5.0, 5.2]);
  await assert.rejects(segments.unit('rep', 'background 1', () => 'never'), (error) => {
    assert.ok(error instanceof RateGateError);
    assert.equal(error.exitCode, 5);
    assert.match(error.message, /failed the rate gate 2 times/);
    return true;
  });
  assert.equal(gate.units.filter((unit) => unit.discarded).length, 2);
});

test('a probe outside the band of the cells fails even inside the range', async () => {
  const gate = fakeGate();
  const { segments } = segmentsWith(gate, [3.0, 3.0], 4.0);
  await assert.rejects(segments.unit('rep', 'held 1', () => 'never'), RateGateError);
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
  const gate = fakeGate([{ waiterPids: [42], seenToReleaseMs: 3 }]);
  const { segments, probes } = segmentsWith(gate, [4.0, 3.9]);
  await segments.unit('rep', 'background 1', () => 1);
  await segments.unit('rep', 'background 2', () => 2);
  assert.equal(probes(), 2);
  assert.deepEqual(gate.units.map((unit) => unit.kind), ['probe', 'rep', 'probe', 'rep']);
  assert.equal(segments.reference, 4.0, 'the first accepted rate is the cells rate');
  assert.equal(gate.units[3].segment, 2);
});
