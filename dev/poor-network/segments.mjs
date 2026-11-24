// The bench's segments. A segment is the steps the bench runs between two takes of the lock. Its first unit is a probe,
// one repetition of the reference cell, whose tail reads the host's CPU speed as a rate; the rate gate reads that rate.
// A segment ends with a yield whenever a process queues for the lock after a step, and by the segment's length, when a
// probe is due again without a yield. On the For-you branch, a probe below the rate floor waits for the host's load to
// fall before it runs again. step-gate.mjs owns the lock and the caps; this module owns the probe and the gate.

import fs from 'node:fs';

import { cpuRateFrom, hostInterference, referenceTail } from './open.mjs';
import { LIVE_CPU_TAIL } from './profile.mjs';

// The CPU rate gate. Each segment calibrates on its own: its probe's rate must be inside RATE_MIN..RATE_MAX, and a
// probe is not compared with another segment's rate. The derived rate rises as the host's CPU gets faster or its load
// falls, so RATE_MAX sits above the rates a host reads at moderate load. It still refuses a broken calibration: a
// reference document that crosses the link in full reads 21.68 to 30.31 (README "CPU rate").
export const RATE_MIN = 3.0;
export const RATE_MAX = 10.0;
// A segment is re-probed at most this long after its probe, so each measurement stays close to the rate that describes
// it (design value: about ten minutes).
export const SEGMENT_MAX_MS = 600000;
// A probe that fails the gate is run once more; a second failure ends the run with exit status 5.
export const PROBE_ATTEMPTS = 2;

// The For-you branch's load wait (Plan 5 v4): a probe below RATE_MIN waits until load1 reads at most
// LOAD_WAIT_MAX_LOAD1 in LOAD_WAIT_SAMPLES readings LOAD_WAIT_SAMPLE_MS apart, then probes again. A loaded host reads a
// low rate without anything being wrong with the host's CPU, so the wait lets an already-discarded probe be re-taken
// while the load that spoiled it falls. The wait accrues per segment, and only its cap ends the run.
export const LOAD_WAIT_MAX_LOAD1 = 20;
export const LOAD_WAIT_SAMPLE_MS = 30000;
export const LOAD_WAIT_SAMPLES = 3;
export const LOAD_WAIT_CAP_MS = 1800000;
// The For-you scenario's cells lead their unit names with its name (foryou.mjs runCell: "foryou switch large 1").
const FOR_YOU_UNIT_PREFIX = 'foryou ';

// The load wait's readings and clock: the host's 1-minute load, the process clock, and the pause between readings. The
// segment tests pass their own.
export const loadWaitReadings = {
  load1: () => {
    const [first] = fs.readFileSync('/proc/loadavg', 'utf8').split(' ');
    const value = Number.parseFloat(first);
    if (!Number.isFinite(value)) {
      throw new Error(`/proc/loadavg's first field is not a number: ${first}`);
    }
    return value;
  },
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

// The run ends with exit status 5 when a segment's probe fails the rate gate twice (plan 4.1).
export class RateGateError extends Error {
  exitCode = 5;
}

// A step that passed its cap on every attempt. The caller decides whether its run goes on without it.
export class StepCapped extends Error {}

// The verdict on one probe's rate: its own segment's calibration, checked against the range alone.
export function rateVerdict(rate) {
  if (!(rate >= RATE_MIN && rate <= RATE_MAX)) {
    return { ok: false, reason: `CPU rate ${rate} is outside ${RATE_MIN}-${RATE_MAX}` };
  }
  return { ok: true, reason: null };
}

export class Segments {
  #gate;
  #probe;
  #side;
  #loadWait;
  // The invocation's shared rate when it passes one, or else the first accepted probe's rate: the side's JSON records
  // it. It gates no probe, since each segment's units carry that segment's own probe rate.
  #reference;
  #needProbe = true;
  #count = 0;
  #probeRate = null;
  #startedAt = 0;

  // `probe()` returns one reference repetition (openProbe). `reference` is the shared rate, or null. `loadWait` holds
  // the load wait's readings and clock; the segment tests pass their own.
  constructor({ gate, probe, side, reference, loadWait = loadWaitReadings }) {
    this.#gate = gate;
    this.#probe = probe;
    this.#side = side;
    this.#reference = reference;
    this.#loadWait = loadWait;
  }

  // The rate the side's JSON records as the invocation's reference: the shared rate, or the first accepted probe's.
  get reference() {
    return this.#reference;
  }

  // Runs one unit as a step of the current segment and returns the step's outcome. A probe comes first when the segment
  // is new or the last step gave the lock up: a yield ends the segment, and the next unit starts with a new probe.
  // The gate gives the lock up at each step's end (BenchGate.step), so every step, probes included, can end a segment.
  async unit(kind, name, body) {
    while (this.#needProbe) {
      await this.#enter(name);
    }
    const outcome = await this.#gate.step(kind, name, body);
    outcome.unit.segment = this.#count;
    outcome.unit.probeRate = this.#probeRate;
    if (Date.now() - this.#startedAt >= SEGMENT_MAX_MS || outcome.yielded) {
      this.#needProbe = true;
    }
    return outcome;
  }

  // Starts a segment with a probe that passes the gate. A probe that fails is recorded as discarded and run once more;
  // on the For-you branch, a probe below RATE_MIN instead waits for the host's load to fall and is run again, so only
  // the wait's cap ends that run.
  async #enter(name) {
    let reason = null;
    let attempts = 0;
    let waitedMs = 0;
    for (;;) {
      const number = this.#count + 1;
      const outcome = await this.#gate.step('probe', `segment ${number}`, () => this.#probe());
      if (outcome.status === 'capped') {
        throw new StepCapped(`the probe of segment ${number} passed its cap twice`);
      }
      const rep = outcome.value;
      const interference = hostInterference(rep);
      const rate = interference === null ? cpuRateFrom(LIVE_CPU_TAIL.medianMs, referenceTail(rep)) : null;
      const verdict = interference === null
        ? rateVerdict(rate)
        : { ok: false, reason: `host interference: ${interference}` };
      outcome.unit.segment = number;
      outcome.unit.probeRate = rate;
      if (verdict.ok) {
        this.#count = number;
        this.#probeRate = rate;
        this.#reference ??= rate;
        // A probe whose step gave the lock up is followed by a new probe, so the segment it started ends at once.
        this.#needProbe = outcome.yielded;
        this.#startedAt = Date.now();
        return;
      }
      outcome.unit.discarded = true;
      outcome.unit.reason = verdict.reason;
      reason = verdict.reason;
      process.stderr.write(`[${this.#side}] segment ${number} discarded: ${verdict.reason}\n`);
      if (rate !== null && rate < RATE_MIN && name.startsWith(FOR_YOU_UNIT_PREFIX)) {
        const remaining = LOAD_WAIT_CAP_MS - waitedMs;
        if (remaining <= 0) {
          throw this.#waitCapError(reason);
        }
        const wait = await this.#waitForLoad(remaining);
        waitedMs += wait.waitedMs;
        outcome.unit.loadWaitMs = wait.waitedMs;
        outcome.unit.loadWaitLoad1 = wait.readings;
        process.stderr.write(`[${this.#side}] segment ${number}: waited ${Math.round(wait.waitedMs / 1000)} s` +
          ` for load1 <= ${LOAD_WAIT_MAX_LOAD1} before the reprobe (${wait.readings.join(', ')})\n`);
        if (!wait.done) {
          throw this.#waitCapError(reason);
        }
        continue;
      }
      attempts += 1;
      if (attempts >= PROBE_ATTEMPTS) {
        throw new RateGateError(`the CPU probe failed the rate gate ${PROBE_ATTEMPTS} times: ${reason}`);
      }
    }
  }

  // Waits, within `limitMs`, for LOAD_WAIT_SAMPLES readings of load1 at most LOAD_WAIT_MAX_LOAD1, LOAD_WAIT_SAMPLE_MS
  // apart. Returns the wait's time and readings, with `done` false when the limit ran out first.
  async #waitForLoad(limitMs) {
    const { load1, now, sleep } = this.#loadWait;
    const started = now();
    const readings = [];
    let inRange = 0;
    for (;;) {
      const reading = load1();
      readings.push(reading);
      inRange = reading <= LOAD_WAIT_MAX_LOAD1 ? inRange + 1 : 0;
      const waitedMs = now() - started;
      if (inRange >= LOAD_WAIT_SAMPLES) {
        return { done: true, waitedMs, readings };
      }
      if (waitedMs + LOAD_WAIT_SAMPLE_MS > limitMs) {
        return { done: false, waitedMs, readings };
      }
      await sleep(LOAD_WAIT_SAMPLE_MS);
    }
  }

  #waitCapError(reason) {
    return new RateGateError(`the For-you probe's load wait reached its ${LOAD_WAIT_CAP_MS / 60000} min cap:` +
      ` ${reason}`);
  }
}
