// The report of one side: each metric's repetitions become a median, a 90th percentile and a verdict against its target.
// Run as `node report.mjs <side.json>...` it prints the summary table of the given sides, and with a before and an
// after side it also prints a probe flag for each segment whose probe median moved over 10 % between them, then the
// cross-side verdict. It exits 4 when a scenario that passed before fails after. A flag does not change the exit
// status.

import fs from 'node:fs';

import { CALIBRATION_HOLD_MS, CALIBRATION_REFERENCE, LINK, LIVE_READBACK, TARGETS, VIEW } from './profile.mjs';

const BYTES_PER_MS = LINK.bytesPerSecond / 1000;

// A restart cell (restart.mjs) is gated on its wait from the return to the caught-up instant (plan 6 v4 section 4.2).
// The probe row's wait beside it is reported, not scored. A hidden or frozen cell's capped repetitions are those whose
// phase never came, and the cap ends them.
const restartCell = (cell, title) => ({
  id: cell,
  reps: cell,
  title,
  unit: 'ms',
  target: TARGETS.restartCatchupMs,
  value: (rep) => rep.waitMs,
});
const hiddenRestartCell = (cell, title) => ({
  ...restartCell(cell, title),
  capped: (rep) => rep.capped,
});
const probeRowCell = (cell, title) => ({
  id: `${cell}.probeRow`,
  reps: cell,
  title,
  unit: 'ms',
  target: null,
  value: (rep) => rep.probeWaitMs,
});

// Each metric reads one repetition. Excess metrics are the time beyond the floor a step's bytes need on the link, so
// their target is the allowance; the rest are compared as they are.
export const METRICS = Object.freeze([
  {
    id: 'background.received',
    scenario: 1,
    title: 'background, received',
    unit: 'KB/min',
    target: TARGETS.backgroundKbPerMinute,
    value: (rep) => rep.receivedKbPerMin,
  },
  {
    id: 'background.sent',
    scenario: 1,
    title: 'background, sent',
    unit: 'KB/min',
    target: TARGETS.backgroundKbPerMinute,
    value: (rep) => rep.sentKbPerMin,
  },
  {
    id: 'switch',
    scenario: 2,
    title: 'switch during background, over floor',
    unit: 'ms',
    target: TARGETS.switchExtraMs,
    value: (rep) => rep.switchMs - rep.switchFloorMs,
    raw: (rep) => rep.switchMs,
    capped: (rep) => rep.switchCapped,
  },
  {
    // Click to the target session's catchup_complete: the switch's own first screen plus the setup of the target's
    // real-time connection. It has no target of its own; it is the number fix F must lower.
    id: 'switch-catchup',
    scenario: 2,
    title: 'switch during background, click to catchup_complete',
    unit: 'ms',
    target: null,
    value: (rep) => rep.switchCatchupMs,
    raw: (rep) => rep.switchCatchupMs,
    capped: (rep) => rep.switchCatchupCapped,
  },
  {
    id: 'send',
    scenario: 3,
    title: 'send during background, over floor',
    unit: 'ms',
    target: TARGETS.sendExtraMs,
    value: (rep) => rep.sendMs - rep.sendFloorMs,
    raw: (rep) => rep.sendMs,
    capped: (rep) => rep.sendCapped,
  },
  {
    id: 'held-blackout',
    scenario: 4,
    title: 'held blackout, restore to last switch, over floor',
    unit: 'ms',
    target: TARGETS.heldBlackoutExtraMs,
    value: (rep) => rep.restoreToRenderedMs - rep.lastFloorMs,
    raw: (rep) => rep.restoreToRenderedMs,
  },
  {
    id: 'reset-blackout',
    scenario: 5,
    title: 'reset blackout, restore to catch-up, over bytes',
    unit: 'ms',
    target: TARGETS.resetBlackoutBaseMs,
    value: (rep) => rep.restoreToCatchupMs - rep.catchupBytes / BYTES_PER_MS,
    raw: (rep) => rep.restoreToCatchupMs,
  },
  {
    id: 'long-stream',
    scenario: 6,
    title: 'long stream, socket bytes per reply text byte',
    unit: 'ratio',
    target: TARGETS.longStreamRatio,
    value: (rep) => rep.socketBytes / rep.replyTextBytes,
  },
  {
    id: 'tool-stream.rows',
    scenario: 7,
    title: 'tool stream, row bytes per final row byte',
    unit: 'ratio',
    target: TARGETS.toolStreamRatio,
    value: (rep) => rep.toolRowBytes / rep.finalRowBytes,
  },
  {
    id: 'tool-stream.socket',
    scenario: 7,
    title: 'tool stream, socket bytes per minute',
    unit: 'KB/min',
    target: TARGETS.backgroundKbPerMinute,
    value: (rep) => rep.socketBytes / 1000 / (rep.turnMs / 60000),
  },
  {
    id: 'foryou.switch.large',
    scenario: 8,
    title: 'For-you switch, large target, click to the first row',
    unit: 'ms',
    target: TARGETS.foryouSwitchLargeMs,
    value: (rep) => rep.switch?.large?.firstRowMs,
    raw: (rep) => rep.switch?.large?.firstRowMs,
    capped: (rep) => rep.switch?.large?.capped,
  },
  {
    id: 'foryou.switch.large.pages',
    scenario: 8,
    title: 'For-you switch, large target, older-page requests before the first row',
    unit: 'requests',
    target: TARGETS.foryouPagesBeforeFirstRow,
    value: (rep) => rep.switch?.large?.pages,
  },
  {
    id: 'foryou.switch.typical',
    scenario: 8,
    title: 'For-you switch, typical target, click to the first row',
    unit: 'ms',
    target: TARGETS.foryouSwitchTypicalMs,
    value: (rep) => rep.switch?.typical?.firstRowMs,
    raw: (rep) => rep.switch?.typical?.firstRowMs,
    capped: (rep) => rep.switch?.typical?.capped,
  },
  {
    id: 'foryou.switch.typical.pages',
    scenario: 8,
    title: 'For-you switch, typical target, older-page requests before the first row',
    unit: 'requests',
    target: TARGETS.foryouPagesBeforeFirstRow,
    value: (rep) => rep.switch?.typical?.pages,
  },
  {
    id: 'foryou.switch.sparse',
    scenario: 8,
    title: 'For-you switch, sparse target (its only For-you row is the marker), click to the first row',
    unit: 'ms',
    target: null,
    value: (rep) => rep.switch?.sparse?.firstRowMs,
    raw: (rep) => rep.switch?.sparse?.firstRowMs,
    capped: (rep) => rep.switch?.sparse?.capped,
  },
  {
    id: 'foryou.switch.sparse.pages',
    scenario: 8,
    title: 'For-you switch, sparse target, older-page requests started before the first row',
    unit: 'requests',
    target: TARGETS.foryouPagesBeforeFirstRow,
    value: (rep) => rep.switch?.sparse?.pages,
  },
  {
    id: 'foryou.failed-page',
    scenario: 8,
    title: 'For-you switch with one injected failed older page, click to the first row',
    unit: 'ms',
    target: TARGETS.foryouFailedPageMs,
    value: (rep) => rep.failedPage?.firstRowMs,
    raw: (rep) => rep.failedPage?.firstRowMs,
    capped: (rep) => rep.failedPage?.capped,
  },
  {
    id: 'foryou.failed-page.retry',
    scenario: 8,
    title: 'For-you switch with one injected failed older page, its page requests after the failure',
    unit: 'requests',
    target: null,
    value: (rep) => rep.failedPage?.pageRequestsAfterFailure,
  },
  {
    id: 'foryou.open.large',
    scenario: 8,
    title: 'For-you first open, large target, navigation to the first row',
    unit: 'ms',
    target: TARGETS.foryouOpenLargeMs,
    value: (rep) => rep.open?.large?.firstRowMs,
    raw: (rep) => rep.open?.large?.firstRowMs,
  },
  {
    id: 'foryou.open.large.pages',
    scenario: 8,
    title: 'For-you first open, large target, older-page requests before the first row',
    unit: 'requests',
    target: TARGETS.foryouPagesBeforeFirstRow,
    value: (rep) => rep.open?.large?.pages,
  },
  {
    id: 'foryou.open.large.document-bytes',
    scenario: 8,
    title: 'For-you first open, large target, document bytes',
    unit: 'bytes',
    target: null,
    value: (rep) => rep.open?.large?.documentBytes,
  },
  {
    id: 'foryou.open.typical',
    scenario: 8,
    title: 'For-you first open, typical target, navigation to the first row',
    unit: 'ms',
    target: TARGETS.foryouOpenTypicalMs,
    value: (rep) => rep.open?.typical?.firstRowMs,
    raw: (rep) => rep.open?.typical?.firstRowMs,
  },
  {
    id: 'foryou.open.typical.pages',
    scenario: 8,
    title: 'For-you first open, typical target, older-page requests before the first row',
    unit: 'requests',
    target: TARGETS.foryouPagesBeforeFirstRow,
    value: (rep) => rep.open?.typical?.pages,
  },
  {
    id: 'foryou.offline.reopen',
    scenario: 8,
    title: 'For-you offline reopen of the large target after an online open, navigation to the first row',
    unit: 'ms',
    target: TARGETS.foryouOfflineReopenMs,
    afterOnly: true,
    value: (rep) => rep.offline?.firstRowMs,
    raw: (rep) => rep.offline?.firstRowMs,
    capped: (rep) => rep.offline?.capped,
  },
  {
    id: 'foryou.offline.document-bytes',
    scenario: 8,
    title: 'For-you offline reopen, document bytes',
    unit: 'bytes',
    target: TARGETS.foryouOfflineDocumentBytes,
    afterOnly: true,
    value: (rep) => rep.offline?.documentBytes,
  },
  restartCell('restart-visible', 'restart, visible: server ready to caught up'),
  probeRowCell('restart-visible', 'restart, visible: server ready to probe row (report only)'),
  hiddenRestartCell('restart-hidden-1', 'restart, hidden, shown 1 s after catch-up: return to caught up'),
  probeRowCell('restart-hidden-1', 'restart, hidden, shown 1 s after catch-up: return to probe row (report only)'),
  hiddenRestartCell('restart-hidden-2', 'restart, hidden, shown 0.3 s after the reload navigates: return to caught up'),
  probeRowCell('restart-hidden-2', 'restart, hidden, shown 0.3 s after reload navigates: return to probe row (report only)'),
  hiddenRestartCell('restart-hidden-3', 'restart, hidden, shown after the reload loads: return to caught up'),
  probeRowCell('restart-hidden-3', 'restart, hidden, shown after the reload loads: return to probe row (report only)'),
  hiddenRestartCell('restart-frozen', 'restart, frozen, shown after the reload loads: return to caught up'),
  probeRowCell('restart-frozen', 'restart, frozen, shown after the reload loads: return to probe row (report only)'),
  hiddenRestartCell('restart-1006', 'restart, hidden, 1006 reset, shown after the reload loads: return to caught up'),
  probeRowCell('restart-1006', 'restart, hidden, 1006 reset, shown after reload loads: return to probe row (report only)'),
]);

// The nearest-rank percentile of `values`.
export function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  const rank = Math.ceil(fraction * sorted.length);
  return sorted[Math.max(0, rank - 1)];
}

export function summarize(allReps, metric) {
  // A repetition that did not run the metric's cell or whose cell failed without a readout has no value for it (plan 5
  // v4 section 4.2 counts each cell's own repetitions), so it is left out of the metric's values.
  const reps = allReps.filter((rep) => {
    const value = metric.value(rep);
    return value !== undefined && value !== null;
  });
  const values = reps.map(metric.value);
  const median = percentile(values, 0.5);
  return {
    id: metric.id,
    title: metric.title,
    scenario: metric.scenario,
    unit: metric.unit,
    values,
    median,
    p90: percentile(values, 0.9),
    rawMedian: metric.raw ? percentile(reps.map(metric.raw), 0.5) : null,
    // A capped repetition hit the bench's cap before the step finished: its value is a lower bound.
    capped: metric.capped ? reps.filter(metric.capped).length : null,
    target: metric.target,
    // A metric without a target is reported for reading, not scored.
    pass: metric.target === null ? null : median <= metric.target,
  };
}

// The rep list that feeds each scenario number of METRICS.
export const REPS_OF_SCENARIO = Object.freeze({
  1: 'background', 2: 'background', 3: 'background', 4: 'held', 5: 'reset', 6: 'stream', 7: 'tools', 8: 'foryou',
});

// The rep list a metric reads: a restart cell names its own, every other metric takes its scenario number's.
export function repsOfMetric(metric) {
  return metric.reps ?? REPS_OF_SCENARIO[metric.scenario];
}

// Fills a side's metrics and calibration from its repetitions. The bench writes the side this way, and a before side
// assembled from stored rows derives the same fields from the rows it holds.
export function deriveSide(side) {
  side.metrics = {};
  for (const metric of METRICS) {
    const reps = side.reps[repsOfMetric(metric)] ?? [];
    // A metric whose cell no repetition ran or measured (a null/failed cell, or an after-only cell on the before side)
    // has no row.
    const hasValue = (rep) => {
      const value = metric.value(rep);
      return value !== undefined && value !== null;
    };
    if (reps.some(hasValue) && side.status === 'measured') {
      side.metrics[metric.id] = summarize(reps, metric);
    }
  }
  const background = side.reps.background ?? [];
  side.calibration = background.length > 0 ? calibrate(background) : null;
  return side;
}

// The cross-side verdict: a metric the bench passed on the before side and fails on the after side is a regression. A
// metric absent from either side, or one without a target, is not compared.
export function regressions(before, after) {
  const found = [];
  for (const metric of METRICS) {
    const beforeRow = before.metrics?.[metric.id];
    const afterRow = after.metrics?.[metric.id];
    if (!beforeRow || !afterRow || beforeRow.pass === null || afterRow.pass === null) {
      continue;
    }
    if (beforeRow.pass && !afterRow.pass) {
      found.push(metric);
    }
  }
  return found;
}

// A flag is a segment whose probe median moved by more than this share of its before median. A flag reports how far the
// probe drifted between the sides; it does not change a verdict.
export const PROBE_FLAG_SHARE = 0.1;

// Each segment's probe median, by segment number. Every row unit of a segment carries its median (segments.mjs), so a
// side without row units, such as a before side assembled from stored rows, has no medians.
export function segmentMedians(side) {
  const medians = new Map();
  for (const unit of side.units) {
    if (unit.probeMedian !== undefined) {
      medians.set(unit.segment, unit.probeMedian);
    }
  }
  return medians;
}

// The segments whose probe median differs between the sides by more than PROBE_FLAG_SHARE of the before median. The
// sides pair by segment number, so a segment that only one side has is not compared.
export function probeFlags(before, after) {
  const afterMedians = segmentMedians(after);
  const flags = [];
  for (const [segment, beforeMedian] of segmentMedians(before)) {
    if (!afterMedians.has(segment)) {
      continue;
    }
    const afterMedian = afterMedians.get(segment);
    if (Math.abs(afterMedian / beforeMedian - 1) > PROBE_FLAG_SHARE) {
      flags.push({ segment, before: beforeMedian, after: afterMedian });
    }
  }
  return flags;
}

export function formatProbeFlag({ segment, before, after }) {
  const change = (after / before - 1) * 100;
  return `probe flag: segment ${segment} probe median ${before.toFixed(2)} before, ${after.toFixed(2)} after ` +
    `(${change >= 0 ? '+' : ''}${change.toFixed(1)} %); the flag does not change the verdict`;
}

// The calibration: on the before side, a switch clicked while a background list refresh is in flight must wait for at
// least CALIBRATION_HOLD_MS to render. The holds are the background repetitions' switchDuringRefreshMs, whose clicks
// the bench lands inside a refresh; the gate reads their median. The after side reports the same holds without the
// gate, so they show whether the wait is gone after the fixes.
export function calibrate(background) {
  const holdsMs = background.map((rep) => rep.switchDuringRefreshMs);
  const medianMs = percentile(holdsMs, 0.5);
  return {
    holdsMs,
    medianMs,
    requiredMs: CALIBRATION_HOLD_MS,
    cappedCount: background.filter((rep) => rep.switchDuringRefreshCapped).length,
    reproduced: medianMs >= CALIBRATION_HOLD_MS,
  };
}

export function formatMs(value) {
  return value === null ? '-' : value >= 1000 ? `${(value / 1000).toFixed(2)} s` : `${value.toFixed(0)} ms`;
}

function formatValue(metric, value) {
  if (value === null || value === undefined) {
    return '-';
  }
  if (metric.unit === 'ms') {
    return formatMs(value);
  }
  return metric.unit === 'ratio' ? value.toFixed(2) : value.toFixed(1);
}

// One side's rows: the metric, its median and p90 over the repetitions, its target and the verdict.
export function formatSide(side) {
  const lines = [`side ${side.side}  commit ${side.commit}  chrome ${side.chrome.version}`];
  lines.push('| metric | median | p90 | raw median | at cap | target | verdict |');
  lines.push('|---|---|---|---|---|---|---|');
  for (const metric of METRICS) {
    const row = side.metrics[metric.id];
    if (!row) {
      continue;
    }
    const definition = METRICS.find((candidate) => candidate.id === metric.id);
    const targetText =
      row.target === null ? '-' : metric.unit === 'ms' ? `<= ${formatMs(row.target)}` : `<= ${formatValue(definition, row.target)}`;
    const verdict = row.pass === null ? '-' : row.pass ? 'PASS' : 'FAIL';
    lines.push(
      `| ${row.id} ${row.title} (${row.unit}) | ${formatValue(definition, row.median)} | ${formatValue(definition, row.p90)} | ${formatValue(definition, row.rawMedian)} | ${row.capped === null ? '-' : `${row.capped} of ${row.values.length}`} | ${targetText} | ${verdict} |`,
    );
  }
  if (side.calibration) {
    const calibration = side.calibration;
    // Only the reference before side is gated (run.sh), so only that side's line carries a verdict; every other side,
    // its own before side included, reports the raw holds beside the gate it does not face.
    const gated = side.side === 'before' && side.commit === CALIBRATION_REFERENCE;
    const verdict = gated
      ? calibration.reproduced
        ? 'reproduced'
        : 'NOT reproduced'
      : 'the gate reads the reference before side only';
    const capNote = calibration.cappedCount > 0 ? `, ${calibration.cappedCount} at cap` : '';
    lines.push(
      `calibration: switch clicked during a list refresh, holds ${calibration.holdsMs.map(formatMs).join(', ')}${capNote}, median ${formatMs(calibration.medianMs)} (needs >= ${formatMs(calibration.requiredMs)}): ${verdict}`,
    );
  }
  if (side.view) {
    lines.push(
      `view: ${side.view.rows} rows, raw median row ${side.view.rawRowMedianBytes} B (live view: ${VIEW.rows} rows, ${VIEW.rawRowMedianBytes} B)`,
    );
  }
  if (side.statusChurn) {
    const churn = side.statusChurn;
    const perMinute = (flips) => ((flips * 60000) / churn.windowMs).toFixed(1);
    lines.push(
      `status churn per ${churn.windowMs / 1000} s window: a held turn starts and is cancelled ` +
        `${churn.turnFlipsPerWindow} times (${perMinute(churn.turnFlipsPerWindow)} a minute, broadcast), and a delayed ` +
        `trigger appears and is cancelled ${churn.triggerFlipsPerWindow} times ` +
        `(${perMinute(churn.triggerFlipsPerWindow)} a minute, silent)`,
    );
  }
  const backgroundReps = side.reps?.background ?? [];
  if (backgroundReps.length > 0) {
    lines.push(...formatBackground('background', backgroundReps));
    // The switch's raw hold, split by whether a sidebar list refresh was in flight when the click
    // landed. A switch inside a refresh burst waits behind the refresh's bytes, so the two groups
    // are not comparable and the target is read from the split that applies.
    const groups = [
      ['overlapping a sidebar list refresh', backgroundReps.filter((rep) => (rep.switchListRefreshes ?? []).length > 0)],
      ['between sidebar list refreshes', backgroundReps.filter((rep) => (rep.switchListRefreshes ?? []).length === 0)],
    ];
    for (const [label, reps] of groups) {
      if (reps.length === 0) continue;
      lines.push(
        `background switch raw, ${label}: ${reps.map((rep) => formatMs(rep.switchMs)).join(', ')}`,
      );
    }
  }
  const heldReps = side.reps?.held ?? [];
  if (heldReps.length > 0 && (heldReps[0].shownMs ?? null) !== null) {
    // The held recovery's parts, medians from the restore: the last switch's render, the socket's own return, and the
    // catch-up the metric reads. The clicks were made during the blackout, so the render can precede the restore. A
    // page that keeps one socket (fix F) opens no socket for the target, so its socket part is absent.
    const medianOf = (values) => {
      const present = values.filter((value) => value !== null && value !== undefined);
      return present.length === 0 ? null : percentile(present, 0.5);
    };
    lines.push(
      `held recovery, ms from the restore (median): last switch shown ${formatMs(medianOf(heldReps.map((rep) => rep.shownMs)))}, ` +
        `socket ${formatMs(medianOf(heldReps.map((rep) => rep.socketMs)))}, ` +
        `caught up ${formatMs(medianOf(heldReps.map((rep) => rep.restoreToRenderedMs)))}`,
    );
    lines.push(
      `held recoveries, ms from the restore (per repetition): ${heldReps
        .map((rep, index) => `${index + 1} ${formatMs(rep.restoreToRenderedMs)} (shown ${formatMs(rep.shownMs)}, caught up ${formatMs(rep.catchupMs)})`)
        .join(', ')}`,
    );
  }
  const resetReps = side.reps?.reset ?? [];
  if (resetReps.length > 0 && resetReps.every((rep) => (rep.handshakeSendsMs ?? []).length > 0)) {
    // The recovery's phases, medians from the restore: the page's socket creation, the browser's first handshake byte,
    // the open and the catch-up. The page owns the first; the browser's connection setup owns the second, and it can
    // hold a socket the page created early.
    const medianOf = (values) => percentile(values, 0.5);
    lines.push(
      `reset recovery, ms from the restore (median): socket created ${formatMs(medianOf(resetReps.map((rep) => rep.socketMs)))}, ` +
        `handshake sent ${formatMs(medianOf(resetReps.map((rep) => rep.handshakeSendsMs[0])))}, ` +
        `opened ${formatMs(medianOf(resetReps.map((rep) => rep.reconnectMs)))}, ` +
        `caught up ${formatMs(medianOf(resetReps.map((rep) => rep.restoreToCatchupMs)))}`,
    );
    lines.push(
      `reset recoveries, ms from the restore (per repetition): ${resetReps
        .map((rep, index) => `${index + 1} ${formatMs(rep.restoreToCatchupMs)} (socket created ${formatMs(rep.socketMs)}, handshake sent ${formatMs(rep.handshakeSendsMs[0])}, opened ${formatMs(rep.reconnectMs)})`)
        .join(', ')}`,
    );
  }
  for (const name of ['starred', 'threads']) {
    const reps = side.reps?.[name] ?? [];
    if (reps.length === 0) {
      continue;
    }
    const view = side.otherViews[name];
    const received = reps.map((rep) => rep.receivedKbPerMin);
    const sent = reps.map((rep) => rep.sentKbPerMin);
    lines.push(
      `background on the ${name} view (${view.rows} rows, raw median row ${view.rawRowMedianBytes} B, ` +
        `${percentile(reps.map((rep) => rep.viewRowsPainted), 0.5)} painted): received median ` +
        `${percentile(received, 0.5).toFixed(1)}, p90 ${percentile(received, 0.9).toFixed(1)} KB/min; sent median ` +
        `${percentile(sent, 0.5).toFixed(1)}, p90 ${percentile(sent, 0.9).toFixed(1)} KB/min`,
    );
    lines.push(...formatBackground(`${name} view`, reps));
  }
  for (const [name, reps] of Object.entries(side.reps ?? {})) {
    reps.forEach((rep, index) => {
      const others = rep.otherTrials.length > 0 ? rep.otherTrials.join(', ') : 'none';
      lines.push(`${name} repetition ${index + 1} (${rep.startedAt} to ${rep.finishedAt}), other trials: ${others}`);
    });
  }
  // The switch's own transfer and request count: the bytes its bootstrap moved before the first
  // screen rendered, and how often each of the switch's request paths went out within one switch.
  // A path whose median count is above one is a repeated download.
  const switchReps = backgroundReps.filter((rep) => rep.switchRequests);
  if (switchReps.length > 0) {
    const bytes = percentile(switchReps.map((rep) => rep.switchBootstrapBytes), 0.5);
    const total = percentile(switchReps.map((rep) => rep.switchRequestTotal), 0.5);
    const paths = [...new Set(switchReps.flatMap((rep) => Object.keys(rep.switchRequests)))].sort();
    const parts = paths.map((path) => {
      const counts = switchReps.map((rep) => rep.switchRequests[path] ?? 0);
      return `${path.replace('/api/sessions/', '')} ${percentile(counts, 0.5)}`;
    });
    lines.push(
      `switch (median): bootstrap ${bytes.toFixed(0)} B on the wire before the first screen, ` +
        `${total} requests per switch (${parts.join(', ')})`,
    );
  }
  const spinReps = backgroundReps.filter((rep) => rep.sidebarSpin);
  if (spinReps.length > 0) {
    const median = (name) => percentile(spinReps.map((rep) => rep.sidebarSpin[name]), 0.5);
    lines.push(
      `sidebar spinner, visible tab (median): ${median('restartsPerMin').toFixed(1)} spinner restarts/min, ` +
        `${median('repaintsPerMin').toFixed(1)} full sidebar repaints/min`,
    );
  }
  const background = side.metrics['background.received'];
  if (background) {
    const gap = background.median - LIVE_READBACK.receivedKbPerMin;
    lines.push(
      `live readback: received ${LIVE_READBACK.receivedKbPerMin} KB/min; background received median ${background.median.toFixed(1)} KB/min (gap ${gap.toFixed(1)} KB/min)`,
    );
  }
  return lines.join('\n');
}

// The background repetitions' received bytes by endpoint and the visible tab's status polls, as medians over the
// repetitions. A status poll's fields are counted in the polls whose body they changed. The share of 200 answers is
// compared with the live device's, which the churn model reproduces.
function formatBackground(label, reps) {
  const median = (values) => percentile(values, 0.5);
  const endpoints = Object.keys(reps[0].receivedByEndpoint).map(
    (name) => `${name} ${median(reps.map((rep) => rep.receivedByEndpoint[name])).toFixed(1)}`,
  );
  const poll = (name) => median(reps.map((rep) => rep.statusPoll[name]));
  const share = (reps) => {
    const polls = reps.map((rep) => rep.statusPoll.answered200 + rep.statusPoll.answered304);
    return median(reps.map((rep, index) => (polls[index] === 0 ? 0 : rep.statusPoll.answered200 / polls[index])));
  };
  const bytes = (name) => {
    const values = reps.map((rep) => rep.statusPoll[name]).filter((value) => value !== null);
    return values.length > 0 ? `${median(values).toFixed(0)} B` : '-';
  };
  const fieldNames = [...new Set(reps.flatMap((rep) => Object.keys(rep.statusPoll.changedFields)))].sort();
  const fields = fieldNames.map(
    (name) => `${name} ${median(reps.map((rep) => rep.statusPoll.changedFields[name] ?? 0))}`,
  );
  const roleNames = [...new Set(reps.flatMap((rep) => Object.keys(rep.statusPoll.changedRows ?? {})))].sort();
  const roles = roleNames.map(
    (name) => `${name} ${median(reps.map((rep) => rep.statusPoll.changedRows?.[name] ?? 0))}`,
  );
  return [
    `${label} received by endpoint, KB/min (median): ${endpoints.join(', ')}`,
    `${label} status poll, visible tab (median): ${poll('perMinute').toFixed(1)} per min, ${poll('answered200')} ` +
      `answered 200 (share ${share(reps).toFixed(2)}, live ${LIVE_READBACK.status200Share.toFixed(2)}), ` +
      `${poll('answered304')} answered 304, ${poll('noWireStatus')} with no wire status; received per answer ` +
      `${bytes('bytes200')} on a 200 and ${bytes('bytes304')} on a 304; of the 200s, ` +
      `${poll('changedBodies')} moved a row, ${poll('unchangedBodies')} repeated the state and ` +
      `${poll('firstBodies')} answered a row set the tab held no state for; polls per changed field: ` +
      `${fields.length > 0 ? fields.join(', ') : 'none'}; per changed row: ` +
      `${roles.length > 0 ? roles.join(', ') : 'none'}`,
  ];
}

function main(paths) {
  const sides = paths.map((path) => JSON.parse(fs.readFileSync(path, 'utf8')));
  for (const side of sides) {
    console.log(formatSide(side));
    console.log('');
  }
  const before = sides.find((side) => side.side === 'before');
  const after = sides.find((side) => side.side === 'after');
  if (!before || !after) {
    return;
  }
  for (const flag of probeFlags(before, after)) {
    console.log(formatProbeFlag(flag));
  }
  const failed = regressions(before, after);
  if (failed.length === 0) {
    console.log('regressions: none; every scenario the bench passed on the before side passes on the after side');
    return;
  }
  for (const metric of failed) {
    console.log(`regression: ${metric.id} ${metric.title} passed on the before side and fails on the after side`);
  }
  // run.sh keeps this code, so a caller reads the regression from the exit status as well as the line.
  process.exitCode = 4;
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  main(process.argv.slice(2));
}
