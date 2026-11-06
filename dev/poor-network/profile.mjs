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

// The shape of the view the bench measures, taken from a live release's default "all" view: 124 rows, a raw median row
// of 3,260 bytes, and a switch target whose bootstrap content takes a median 26,592 bytes on the wire untrimmed. The
// bench's own sessions are rows of that view, and the mock home holds the rest. The first screen's trim serves that
// content within its budget; the sizing measures the content, not the served bytes, so both sides size alike.
export const VIEW = Object.freeze({
  rows: 124,
  rawRowMedianBytes: 3260,
  bootstrapUntrimmedWireBytes: 26592,
});

// The other sidebar views the background is measured on, at the sizes of a live release's views: the starred view held
// 18 rows (8 active, 10 archived) and the Threads view 63 active thread sessions. The bench's rows of these views are
// all active.
export const OTHER_VIEWS = Object.freeze({
  starred: { rows: 18 },
  threads: { rows: 63 },
});

// The sessions the bench creates on the trial's backend: one open, one running in the background, three for the
// probes, one switch target and five for the held blackout. They are task-tree nodes, so the server takes a message.
export const BENCH_SESSIONS = 11;

// The live readback of a release's default view, from one visible tab for 4.02 h: status polls about 20 per minute at
// 5,767 bytes received each, and full-list fetches, 174 at 113,791 bytes each. Together they receive about 206 KB a
// minute. The background scenario's before-numbers are compared with it as a second calibration check.
export const LIVE_READBACK = Object.freeze({
  receivedKbPerMin: 206,
  statusPollBytes: 5767,
  fullListBytes: 113791,
});
