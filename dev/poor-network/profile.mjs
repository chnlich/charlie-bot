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

// The calibration: on the baseline, a switch clicked while a background list refresh is in flight must take at least
// this long to render.
export const CALIBRATION_HOLD_MS = 10000;

// The one commit the calibration gate checks the condition on: the pre-fix baseline whose switch holds the spec cites.
// The fixes remove the background bursts the hold lived on, so a fix's own before side no longer reproduces it; the
// gate reads only a before side measured at this commit, and every other before side runs ungated.
export const CALIBRATION_REFERENCE = '408aa851c401c595f8f47e49a233fffd48f64b8a';

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

// The open scenario: the three cases of plan 2 section 3 under the link's conditions. A: this browser opened the
// session before; B: it opened a chat page before, but not this session; C: it never opened a chat page. Every
// constant the scenario reads is here.
export const OPEN = Object.freeze({
  // A session's newest message on screen, from navigation start, on the link (plan 2 section 3).
  targetMs: 1000,
  // The page's bytes after its load event are counted for this long.
  postLoadWindowMs: 30000,
  // A held or reset blackout starts this long before the open navigates, so the open happens inside it.
  blackoutLeadMs: 1000,
  // Repeated drops: the link is up for upSeconds, then down for downSeconds, until totalSeconds have passed. Each
  // drop resets every connection, so each recovery reconnects.
  flaps: Object.freeze({upSeconds: 5, downSeconds: 5, totalSeconds: 60}),
  // The first open after a release: the resource version changes this long after a held blackout starts, and the
  // open navigates openAfterMs after it, so the open is inside the hold.
  releaseAtMs: 5000,
  openAfterMs: 10000,
  // An open that has not shown its newest message or its catch-up by this time is recorded at the cap.
  capMs: 240000,
  // An offline navigation that has not shown the page within this time is recorded as not reached.
  offlineSettleMs: 5000,
});

// Chao's devices, from the live server log's page loads (2026-11-06 03:40Z to 08:40Z). A page load's CPU tail is its
// DOMContentLoaded minus its responseEnd: the time after the document's last byte, which is parsing and running the
// blocking scripts and styles. The raw DOMContentLoaded includes the document's transfer, and the live link carried
// that transfer about ten times faster than the modeled link, so the raw value is not compared. The device with
// enough loads is the one with cause "open" loads, and the other device has none.
export const LIVE_CPU_TAIL = Object.freeze({
  device: '100.84.122.38',
  loads: 13,
  medianMs: 297,
});
