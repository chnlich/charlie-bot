// Tests of the report's calibration gate: it reads the switch holds that were clicked during a list refresh, not the
// scored scenario's random-phase holds, and only the reference before side's summary line carries a verdict.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { CALIBRATION_HOLD_MS, CALIBRATION_REFERENCE } from './profile.mjs';
import { calibrate, formatSide } from './report.mjs';

// One background repetition's calibration fields. `random` is the scored scenario's hold, which the gate must not read.
function rep(duringMs, randomMs) {
  return { switchDuringRefreshMs: duringMs, switchMs: randomMs };
}

test('the gate reads the during-refresh holds, not the random-phase switch holds', () => {
  // Random-phase holds above the threshold with during-refresh holds below it must not reproduce. Reading the scored
  // scenario's holds here is the regression this suite exists to catch.
  const background = [
    rep(CALIBRATION_HOLD_MS - 2000, CALIBRATION_HOLD_MS + 4000),
    rep(CALIBRATION_HOLD_MS - 1000, CALIBRATION_HOLD_MS + 3000),
    rep(CALIBRATION_HOLD_MS - 3000, CALIBRATION_HOLD_MS + 5000),
  ];
  const calibration = calibrate(background);
  assert.equal(calibration.reproduced, false);
  assert.equal(calibration.medianMs, CALIBRATION_HOLD_MS - 2000);
});

test('a median at or above the threshold reproduces', () => {
  const background = [
    rep(CALIBRATION_HOLD_MS, 0),
    rep(CALIBRATION_HOLD_MS + 5000, 0),
    rep(CALIBRATION_HOLD_MS + 1000, 0),
  ];
  assert.equal(calibrate(background).reproduced, true);
});

test('the holds carry their count of repetitions recorded at the cap', () => {
  const background = [rep(CALIBRATION_HOLD_MS + 1000, 0), rep(CALIBRATION_HOLD_MS + 2000, 0)];
  background[1].switchDuringRefreshCapped = true;
  assert.equal(calibrate(background).cappedCount, 1);
});

function side(name, calibration, commit = 'test') {
  return { side: name, commit, chrome: { version: 'test' }, metrics: {}, calibration };
}

function calibrationLine(aSide) {
  return formatSide(aSide).split('\n').find((row) => row.startsWith('calibration:'));
}

test("the reference before side's line prints the raw holds and the verdict", () => {
  const calibration = calibrate([rep(CALIBRATION_HOLD_MS + 500, 0), rep(CALIBRATION_HOLD_MS + 1500, 0)]);
  const line = calibrationLine(side('before', calibration, CALIBRATION_REFERENCE));
  assert.match(line, /holds 10\.50 s, 11\.50 s, median 10\.50 s/);
  assert.match(line, /reproduced$/);
});

test("a before side at another commit prints the raw holds without the verdict run.sh does not apply there", () => {
  const calibration = calibrate([rep(CALIBRATION_HOLD_MS - 6000, 0), rep(CALIBRATION_HOLD_MS - 5000, 0)]);
  const line = calibrationLine(side('before', calibration, 'another-commit'));
  assert.match(line, /holds 4\.00 s, 5\.00 s, median 4\.00 s/);
  assert.match(line, /the gate reads the reference before side only$/);
  assert.doesNotMatch(line, /NOT reproduced/);
});

test("the after side's line prints the raw holds without a gate verdict", () => {
  const calibration = calibrate([rep(CALIBRATION_HOLD_MS - 5000, 0), rep(CALIBRATION_HOLD_MS - 4000, 0)]);
  const line = calibrationLine(side('after', calibration));
  assert.match(line, /holds 5\.00 s, 6\.00 s, median 5\.00 s/);
  assert.match(line, /the gate reads the reference before side only$/);
  assert.doesNotMatch(line, /reproduced/);
});
