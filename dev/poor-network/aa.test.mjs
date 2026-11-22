// Tests of the A/A analysis: the load effect's bootstrap interval against the quiet band, and the seeded generator.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { AA_BAND, metricVerdict, seededRandom } from './aa.mjs';

const STEADY = [10, 10.2, 9.9, 10.1, 10.05, 9.95];
const NOISY = [8, 12, 9, 11, 7, 13];

test('one seed gives one sequence, so an analysis repeats its verdict', () => {
  const first = seededRandom(7);
  const second = seededRandom(7);
  assert.deepEqual([first(), first(), first()], [second(), second(), second()]);
});

test('a steady row with no load effect is load-safe: its interval lies inside the quiet band', () => {
  const verdict = metricVerdict(STEADY, STEADY, { random: seededRandom(1) });
  assert.equal(verdict.quietMedian, 10);
  assert.ok(Math.abs(verdict.band[1] - AA_BAND * 10) < 1e-9);
  assert.equal(verdict.safe, true);
});

test('a row the load slows by 20 % is not load-safe', () => {
  const slowed = STEADY.map((value) => value * 1.2);
  const verdict = metricVerdict(STEADY, slowed, { random: seededRandom(1) });
  assert.equal(verdict.safe, false);
  assert.ok(verdict.interval[0] > verdict.band[1], 'the whole interval sits above the band');
});

test('a noisy row is not load-safe even without a load effect, because its interval is too wide for the band', () => {
  const verdict = metricVerdict(NOISY, NOISY, { random: seededRandom(1) });
  assert.equal(verdict.safe, false);
});

test('a metric measured in one window only is refused, not judged', () => {
  assert.throws(() => metricVerdict([], STEADY, { random: seededRandom(1) }), /needs repetitions in both windows/);
});
