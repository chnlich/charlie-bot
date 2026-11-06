// The report of one side: each metric's repetitions become a median, a 90th percentile and a verdict against its target.
// Run as `node report.mjs <side.json>...` it prints the summary table of the given sides.

import fs from 'node:fs';

import { CALIBRATION_HOLD_MS, LINK, LIVE_READBACK, TARGETS, VIEW } from './profile.mjs';

const BYTES_PER_MS = LINK.bytesPerSecond / 1000;

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
]);

// The nearest-rank percentile of `values`.
export function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  const rank = Math.ceil(fraction * sorted.length);
  return sorted[Math.max(0, rank - 1)];
}

export function summarize(reps, metric) {
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
    pass: median <= metric.target,
  };
}

// The calibration: on the baseline, a switch during the background must wait for at least CALIBRATION_HOLD_MS.
export function calibrate(background) {
  const holdMs = percentile(background.map((rep) => rep.switchMs), 0.5);
  return {
    switchHoldMedianMs: holdMs,
    requiredMs: CALIBRATION_HOLD_MS,
    reproduced: holdMs >= CALIBRATION_HOLD_MS,
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
    const targetText = metric.unit === 'ms' ? `<= ${formatMs(row.target)}` : `<= ${formatValue(definition, row.target)}`;
    lines.push(
      `| ${row.id} ${row.title} (${row.unit}) | ${formatValue(definition, row.median)} | ${formatValue(definition, row.p90)} | ${formatValue(definition, row.rawMedian)} | ${row.capped === null ? '-' : `${row.capped} of ${row.values.length}`} | ${targetText} | ${row.pass ? 'PASS' : 'FAIL'} |`,
    );
  }
  if (side.calibration) {
    const verdict = side.calibration.reproduced ? 'reproduced' : 'NOT reproduced';
    lines.push(
      `calibration: switch hold median ${formatMs(side.calibration.switchHoldMedianMs)} (needs >= ${formatMs(side.calibration.requiredMs)}): ${verdict}`,
    );
  }
  if (side.view) {
    lines.push(
      `view: ${side.view.rows} rows, raw median row ${side.view.rawRowMedianBytes} B (live view: ${VIEW.rows} rows, ${VIEW.rawRowMedianBytes} B)`,
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
    // catch-up the metric reads. The clicks were made during the blackout, so the render can precede the restore.
    const medianOf = (values) => percentile(values, 0.5);
    lines.push(
      `held recovery, ms from the restore (median): last switch shown ${formatMs(medianOf(heldReps.map((rep) => rep.shownMs)))}, ` +
        `socket ${formatMs(medianOf(heldReps.map((rep) => rep.socketMs)))}, ` +
        `caught up ${formatMs(medianOf(heldReps.map((rep) => rep.restoreToRenderedMs)))}`,
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
// repetitions. A status poll's fields are counted in the polls whose body they changed.
function formatBackground(label, reps) {
  const median = (values) => percentile(values, 0.5);
  const endpoints = Object.keys(reps[0].receivedByEndpoint).map(
    (name) => `${name} ${median(reps.map((rep) => rep.receivedByEndpoint[name])).toFixed(1)}`,
  );
  const poll = (name) => median(reps.map((rep) => rep.statusPoll[name]));
  const fieldNames = [...new Set(reps.flatMap((rep) => Object.keys(rep.statusPoll.changedFields)))].sort();
  const fields = fieldNames.map(
    (name) => `${name} ${median(reps.map((rep) => rep.statusPoll.changedFields[name] ?? 0))}`,
  );
  return [
    `${label} received by endpoint, KB/min (median): ${endpoints.join(', ')}`,
    `${label} status poll, visible tab (median): ${poll('perMinute').toFixed(1)} per min, ${poll('answered200')} ` +
      `answered 200, ${poll('answered304')} answered 304, ${poll('noWireStatus')} with no wire status; of the 200s, ` +
      `${poll('changedBodies')} changed the body, ${poll('unchangedBodies')} repeated it and ` +
      `${poll('newUrlBodies')} answered a new URL; polls per changed field: ` +
      `${fields.length > 0 ? fields.join(', ') : 'none'}`,
  ];
}

function main(paths) {
  for (const path of paths) {
    console.log(formatSide(JSON.parse(fs.readFileSync(path, 'utf8'))));
    console.log('');
  }
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  main(process.argv.slice(2));
}
