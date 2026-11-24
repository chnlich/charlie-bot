// The bench's segments. A segment is the steps the bench runs between two takes of the lock. Its first unit is a probe,
// one repetition of the reference cell, whose tail reads the host's CPU speed as a rate; the rate gate reads that rate.
// A segment ends with a yield whenever a process queues for the lock after a step, and by the segment's length, when a
// probe is due again without a yield. step-gate.mjs owns the lock and the caps; this module owns the probe and the
// gate.

import { cpuRateFrom, hostInterference, referenceTail } from './open.mjs';
import { LIVE_CPU_TAIL } from './profile.mjs';

// The CPU rate gate. A probe's rate must be inside RATE_MIN..RATE_MAX and within RATE_BAND of the rate the segment's
// cells run at. The derived rate rises as the host's CPU gets faster or its load falls, so RATE_MAX sits above the
// rates a host reads at moderate load. It still refuses a broken calibration: a reference document that crosses the
// link in full reads 21.68 to 30.31 (README "CPU rate").
export const RATE_MIN = 3.0;
export const RATE_MAX = 10.0;
export const RATE_BAND = 0.1;
// A segment is re-probed at most this long after its probe, so each measurement stays close to the rate that describes
// it (design value: about ten minutes).
export const SEGMENT_MAX_MS = 600000;
// A probe that fails the gate is run once more; a second failure ends the run with exit status 5.
export const PROBE_ATTEMPTS = 2;

// The run ends with exit status 5 when a segment's probe fails the rate gate twice (plan 4.1).
export class RateGateError extends Error {
  exitCode = 5;
}

// A step that passed its cap on every attempt. The caller decides whether its run goes on without it.
export class StepCapped extends Error {}

// The verdict on one probe's rate against the rate the segment's cells run at, or null when the rate is not yet fixed.
export function rateVerdict(rate, reference) {
  if (!(rate >= RATE_MIN && rate <= RATE_MAX)) {
    return { ok: false, reason: `CPU rate ${rate} is outside ${RATE_MIN}-${RATE_MAX}` };
  }
  if (reference !== null && Math.abs(rate / reference - 1) > RATE_BAND) {
    return {
      ok: false,
      reason: `CPU rate ${rate} is more than ${RATE_BAND * 100} % from the cells' rate ${reference}`,
    };
  }
  return { ok: true, reason: null };
}

export class Segments {
  #gate;
  #probe;
  #side;
  // The rate the cells run at: the invocation's shared rate, or the first accepted probe's when none is shared.
  #reference;
  #needProbe = true;
  #count = 0;
  #probeRate = null;
  #startedAt = 0;

  // `probe()` returns one reference repetition (openProbe). `reference` is the shared rate, or null.
  constructor({ gate, probe, side, reference }) {
    this.#gate = gate;
    this.#probe = probe;
    this.#side = side;
    this.#reference = reference;
  }

  // The rate the cells run at, once it is fixed.
  get reference() {
    return this.#reference;
  }

  // Runs one unit as a step of the current segment and returns the step's outcome. A probe comes first when the segment
  // is new or the last step gave the lock up: a yield ends the segment, and the next unit starts with a new probe.
  // The gate gives the lock up at each step's end (BenchGate.step), so every step, probes included, can end a segment.
  async unit(kind, name, body) {
    while (this.#needProbe) {
      await this.#enter();
    }
    const outcome = await this.#gate.step(kind, name, body);
    outcome.unit.segment = this.#count;
    outcome.unit.probeRate = this.#probeRate;
    if (Date.now() - this.#startedAt >= SEGMENT_MAX_MS || outcome.yielded) {
      this.#needProbe = true;
    }
    return outcome;
  }

  // Starts a segment with a probe that passes the gate. A probe that fails is recorded as discarded and run once more.
  async #enter() {
    let reason = null;
    for (let attempt = 1; attempt <= PROBE_ATTEMPTS; attempt++) {
      const number = this.#count + 1;
      const outcome = await this.#gate.step('probe', `segment ${number}`, () => this.#probe());
      if (outcome.status === 'capped') {
        throw new StepCapped(`the probe of segment ${number} passed its cap twice`);
      }
      const rep = outcome.value;
      const interference = hostInterference(rep);
      const rate = interference === null ? cpuRateFrom(LIVE_CPU_TAIL.medianMs, referenceTail(rep)) : null;
      const verdict = interference === null
        ? rateVerdict(rate, this.#reference)
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
    }
    throw new RateGateError(`the CPU probe failed the rate gate ${PROBE_ATTEMPTS} times: ${reason}`);
  }
}
