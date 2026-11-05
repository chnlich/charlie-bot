// The report of one side: each metric's repetitions become a median, a 90th percentile and a verdict against its target.
// Run as `node report.mjs <side.json>...` it prints the summary table of the given sides.

import fs from 'node:fs';

import { CALIBRATION_HOLD_MS, TARGETS, LINK } from './profile.mjs';

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
  return lines.join('\n');
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
