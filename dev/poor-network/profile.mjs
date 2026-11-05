// The link profile and the scenario targets of the poor-network bench. Every scenario and the proxy read them from this
// file; no other file names a link value or a target. The README states the same values in prose.

// The link. One bandwidth budget per direction is shared by every connection the browser opens through the proxy.
export const LINK = Object.freeze({
  // Round trip. Each direction adds half of it to every byte.
  roundTripMs: 300,
  // 100 kbit/s per direction, in bytes per second.
  bytesPerSecond: 12500,
  // Delay spikes: both directions hold for `spikeSeconds` once every `spikeEverySeconds`.
  spikeEverySeconds: 60,
  spikeSeconds: 1,
  // The default length of a scripted blackout, which holds or resets both directions.
  blackoutSeconds: 30,
});

// The scenario targets. A floor is the time the link alone needs for the bytes a step must move.
export const TARGETS = Object.freeze({
  // Scenario 1: each direction, bytes per minute at the proxy.
  backgroundKbPerMinute: 75,
  // Scenario 2: click to rendered is at most the floor plus this many ms. Floor: 0.3 s + bootstrap bytes / 12.5 KB/s.
  switchExtraMs: 1000,
  // Scenario 3: click to echo is at most the floor plus this many ms. Floor: 0.3 s + server processing.
  sendExtraMs: 500,
  // Scenario 4: restore to the last switch rendered and catchup_complete is at most that switch's floor plus this.
  heldBlackoutExtraMs: 2000,
  // Scenario 5: restore to catchup_complete is at most this plus the catch-up bytes / 12.5 KB/s.
  resetBlackoutBaseMs: 2000,
  // Scenario 6: the socket bytes of one reply are at most this multiple of the reply's text bytes.
  longStreamRatio: 1.2,
});

// The background load of scenarios 1 to 3: the task-tree changes a live turn produces, one burst per window.
export const TASK_TREE_LOAD = Object.freeze({
  changesPerWindow: 19,
  windowMs: 75000,
});

// Repetitions of each scenario. The report takes the median and the 90th percentile of the repetitions.
export const REPETITIONS = 5;

// The calibration: the origin/main baseline's switch during the background must take at least this long to render.
export const CALIBRATION_HOLD_MS = 10000;
