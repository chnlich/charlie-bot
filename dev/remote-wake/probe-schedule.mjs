// Today's remote probe schedule. The planner predicts each wake from it, and the report bounds each measured wake by it.
//
// A remote wait probes once when it registers, then sleeps before each next probe: the step's base interval plus the
// probe noise, a whole number of seconds from 0 to PROBE_NOISE_SECONDS. The base interval follows
// PROBE_INTERVALS_SECONDS and then holds at PROBE_PLATEAU_SECONDS. A wake fires at the first probe at or after the
// job's end. The server's values are REMOTE_PROBE_INTERVALS, REMOTE_PROBE_PLATEAU_SECONDS and
// REMOTE_PROBE_NOISE_MAX_SECONDS in rust/crates/runtime/triggers/src/triggers.rs. A change there changes this file.

export const PROBE_INTERVALS_SECONDS = Object.freeze([10, 20, 40, 80, 160, 320]);
export const PROBE_PLATEAU_SECONDS = 600;
export const PROBE_NOISE_SECONDS = 10;

function baseIntervalSeconds(step) {
  return step < PROBE_INTERVALS_SECONDS.length ? PROBE_INTERVALS_SECONDS[step] : PROBE_PLATEAU_SECONDS;
}

// The interval in force `elapsedMs` after the schedule's start: the length of the sleep that ends at the first probe
// after that moment. The probe noise is not part of this interval; the caller adds it to the bound once.
export function intervalInForceMs(elapsedMs) {
  let cumulative = 0;
  for (const interval of PROBE_INTERVALS_SECONDS) {
    cumulative += interval * 1000;
    if (elapsedMs <= cumulative) {
      return interval * 1000;
    }
  }
  return PROBE_PLATEAU_SECONDS * 1000;
}

// The fire offset the schedule predicts for a wait that registers at `registrationSeconds` and whose job ends at
// `endSeconds`: the first probe at or after the end, with every sleep taking the mean probe noise. The planner places
// restarts with it; the report measures the real fire.
export function predictedFireSeconds(registrationSeconds, endSeconds) {
  const meanNoiseSeconds = PROBE_NOISE_SECONDS / 2;
  let probe = registrationSeconds;
  for (let step = 0; probe < endSeconds; step++) {
    probe += baseIntervalSeconds(step) + meanNoiseSeconds;
  }
  return probe;
}
