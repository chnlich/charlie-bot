// The bench's segments. A segment is the units the bench runs between two takes of the lock. The first unit's step runs
// a probe before it: one repetition of the reference cell, whose tail reads the host's CPU speed as a rate. The probe
// and the unit share the step, so one hold of the lock and one cap cover both. The rate gate reads that rate. A segment
// ends with a yield whenever a process queues for the lock after a step, and by the segment's length, when a probe is
// due again without a yield. step-gate.mjs owns the lock and the caps; this module owns the probe.

import { cpuRateFrom, hostInterference, median, referenceTail } from './open.mjs';
import { LIVE_CPU_TAIL } from './profile.mjs';

// The CPU rate gate. A probe passes when its rate is inside RATE_MIN..RATE_MAX, whatever it reads against another
// probe: on a shared host near idle the rate flips between two levels about 1.7x apart (README "Probe reads"), and the
// cells throttle at their own fixed rate either way. The derived rate rises as the host's CPU gets faster or its load
// falls, so RATE_MAX sits above the rates a host reads at moderate load. It still refuses a broken calibration: a
// reference document that crosses the link in full reads 21.68 to 30.31 (README "CPU rate").
export const RATE_MIN = 3.0;
export const RATE_MAX = 10.0;
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

// The verdict on one probe's rate: a rate inside RATE_MIN..RATE_MAX passes.
export function rateVerdict(rate) {
  if (!(rate >= RATE_MIN && rate <= RATE_MAX)) {
    return { ok: false, reason: `CPU rate ${rate} is outside ${RATE_MIN}-${RATE_MAX}` };
  }
  return { ok: true, reason: null };
}

function isoOf(ms) {
  return new Date(ms).toISOString();
}

export class Segments {
  #gate;
  #probe;
  #loadAverage;
  #side;
  #needProbe = true;
  #count = 0;
  // The reads taken since the last segment started. The next segment takes them, so a read of a retried probe or of a
  // capped step's probe belongs to the segment it would start.
  #pendingReads = [];
  #segmentReads = [];
  #segmentMedian = null;
  #startedAt = 0;

  // `probe()` returns one reference repetition (openProbe). `loadAverage()` returns the host's load1 when it is called.
  constructor({ gate, probe, loadAverage, side }) {
    this.#gate = gate;
    this.#probe = probe;
    this.#loadAverage = loadAverage;
    this.#side = side;
  }

  // Runs one unit as a step of the current segment and returns the step's outcome. When a probe is due, it runs first
  // inside this step, so the probe and the unit share one hold of the lock and one cap; a step that passes its cap runs
  // both again. A yield after the step ends the segment: the next unit probes again.
  async unit(kind, name, body) {
    const due = this.#needProbe;
    let pass = 0;
    const outcome = await this.#gate.step(kind, name, async () => {
      pass += 1;
      const probe = due ? await this.#runProbe(pass) : null;
      return { probe, value: await body() };
    });
    if (outcome.status !== 'ok') {
      // A capped step started no segment: the probes it ran stay in the gate's units, and the next unit probes again.
      if (outcome.yielded || Date.now() - this.#startedAt >= SEGMENT_MAX_MS) {
        this.#needProbe = true;
      }
      return outcome;
    }
    const { probe, value } = outcome.value;
    if (probe !== null) {
      this.#count = probe.number;
      this.#segmentReads = this.#pendingReads;
      this.#pendingReads = [];
      this.#segmentMedian = median(this.#segmentReads.map((read) => read.rate));
      this.#startedAt = probe.endedAtMs;
      this.#needProbe = false;
    }
    outcome.unit.segment = this.#count;
    outcome.unit.probeReads = this.#segmentReads;
    outcome.unit.probeMedian = this.#segmentMedian;
    if (outcome.yielded || Date.now() - this.#startedAt >= SEGMENT_MAX_MS) {
      this.#needProbe = true;
    }
    return { status: 'ok', value, unit: outcome.unit, yielded: outcome.yielded };
  }

  // Runs the probe the segment starts with, inside the unit's step, recording each attempt as a unit of the gate and as
  // a read of the pending segment. The first probe that passes the rate gate starts the segment, and a probe that fails
  // the gate is recorded as discarded and run once more; a second failure ends the run with exit status 5. `pass` is
  // the step's pass, which the gate's own units carry.
  async #runProbe(pass) {
    let reason = null;
    for (let tries = 1; tries <= PROBE_ATTEMPTS; tries++) {
      const number = this.#count + 1;
      const startedAt = Date.now();
      const load1 = this.#loadAverage();
      const rep = await this.#probe();
      const endedAt = Date.now();
      const unit = {
        kind: 'probe', name: `segment ${number}`, attempt: pass, status: 'ok',
        startedAt: isoOf(startedAt), endedAt: isoOf(endedAt), segment: number,
      };
      this.#gate.units.push(unit);
      const interference = hostInterference(rep);
      const rate = interference === null ? cpuRateFrom(LIVE_CPU_TAIL.medianMs, referenceTail(rep)) : null;
      this.#pendingReads.push({ rate, load1 });
      unit.probeRate = rate;
      const verdict = interference === null
        ? rateVerdict(rate)
        : { ok: false, reason: `host interference: ${interference}` };
      if (verdict.ok) {
        return { number, endedAtMs: endedAt };
      }
      unit.discarded = true;
      unit.reason = verdict.reason;
      reason = verdict.reason;
      process.stderr.write(`[${this.#side}] segment ${number} discarded: ${verdict.reason}\n`);
    }
    throw new RateGateError(`the CPU probe failed the rate gate ${PROBE_ATTEMPTS} times: ${reason}`);
  }
}
