// The bench's result: one row per wake, the per-build lag summary, the parity check and the run's gates.
//
// A wake's lag is the fire time minus max(the job's own end time, the trigger's creation time). The job's end
// time is the record the job wrote inside the container, so it does not depend on the server code under test.
// A wake that never fired has no lag and is reported with its last status.
//
// The probe schedule bound is what today's back-off allows: the wake comes at most one probe interval after
// max(job end, the waiter's effective start), plus the 10 s of interval noise. The effective start is the
// creation time, unless a server restart happened after it, in which case it is the restarted server's start.
//
// Exit: 0 the run's gate passed; 3 the gate failed; 1 the report could not be built.

import fs from 'node:fs';
import path from 'node:path';

export const PROBE_INTERVALS_SECONDS = Object.freeze([10, 20, 40, 80, 160, 320]);
export const PROBE_PLATEAU_SECONDS = 600;
export const PROBE_NOISE_SECONDS = 10;
export const LENGTH_BINS = Object.freeze([
  { label: '0-70', max: 70 },
  { label: '70-310', max: 310 },
  { label: '310-630', max: 630 },
  { label: 'over-630', max: Infinity },
]);
export const AFTER_MEAN_TARGET_MS = 17000;

function readJson(file, fallback) {
  if (!fs.existsSync(file)) {
    return fallback;
  }
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function readJsonl(file) {
  if (!fs.existsSync(file)) {
    return [];
  }
  return fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

function isoToMs(text) {
  return text === null || text === undefined ? null : Date.parse(text);
}

// The interval in force `elapsedMs` after the waiter's effective start: the length of the sleep that ends at the first
// probe after that moment. The probe noise is not part of this interval; the caller adds it to the bound once.
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

export function lengthBin(length) {
  const bin = LENGTH_BINS.find((candidate) => length <= candidate.max);
  return bin.label;
}

function percentile(values, fraction) {
  const sorted = [...values].sort((a, b) => a - b);
  if (sorted.length === 0) {
    return null;
  }
  const position = (sorted.length - 1) * fraction;
  const lower = Math.floor(position);
  const upper = Math.min(lower + 1, sorted.length - 1);
  return sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower);
}

function stats(values) {
  if (values.length === 0) {
    return { n: 0, mean_ms: null, median_ms: null, p90_ms: null, max_ms: null };
  }
  const mean = values.reduce((sum, value) => sum + value, 0) / values.length;
  return {
    n: values.length,
    mean_ms: Math.round(mean),
    median_ms: Math.round(percentile(values, 0.5)),
    p90_ms: Math.round(percentile(values, 0.9)),
    max_ms: Math.max(...values),
  };
}

// Merge the segment snapshots. A fired record wins over a pending one; otherwise the newest snapshot wins.
function mergeRecords(runDir, build, segments) {
  const merged = new Map();
  for (const segment of segments) {
    const records = readJson(path.join(runDir, 'snapshots', `${build}-${segment}`, 'triggers.json'), []);
    for (const record of records) {
      const previous = merged.get(record.id);
      if (previous === undefined || (previous.status !== 'fired' && record.status === 'fired')) {
        merged.set(record.id, record);
      }
    }
  }
  return merged;
}

function segmentWindows(runDir, build, segments) {
  return segments.map((index) => {
    const readyFile = path.join(runDir, 'ready', `${build}-${index}`);
    const segmentFile = path.join(runDir, 'segments', `${build}-${index}.json`);
    const readyAt = fs.existsSync(readyFile) ? Number(fs.readFileSync(readyFile, 'utf8')) : null;
    const endedAt = fs.existsSync(segmentFile) ? isoToMs(readJson(segmentFile, {}).ended_at) : null;
    return { index, readyAt, endedAt };
  });
}

function fireSegment(windows, firedMs) {
  for (const window of windows) {
    if (window.readyAt !== null && window.endedAt !== null && firedMs >= window.readyAt && firedMs <= window.endedAt) {
      return window;
    }
  }
  return null;
}

// remote_ssh_started lines carry a trigger_ids field. A build without the line has no count to report.
function countSshProcesses(logsDir, build, triggerId) {
  const dir = path.join(logsDir);
  if (!fs.existsSync(dir)) {
    return { logged: false, count: 0 };
  }
  let logged = false;
  let count = 0;
  for (const name of fs.readdirSync(dir)) {
    if (!name.startsWith(`${build}-`)) {
      continue;
    }
    for (const line of fs.readFileSync(path.join(dir, name), 'utf8').split('\n')) {
      if (!line.includes('remote_ssh_started')) {
        continue;
      }
      logged = true;
      const match = /trigger_ids=([^\s]+)/.exec(line);
      if (match && match[1].replace(/[[\]"]/g, '').split(',').includes(triggerId)) {
        count += 1;
      }
    }
  }
  return { logged, count };
}

function sshdConnections(groundTruth, user, fromMs, toMs) {
  return groundTruth.sshd_connections.filter(
    (entry) => entry.user === user && entry.at_ms >= fromMs && entry.at_ms <= toMs,
  ).length;
}

export function buildReport({ runDir, gate }) {
  const plan = readJson(path.join(runDir, 'plan.json'), null);
  const groundTruth = readJson(path.join(runDir, 'ground-truth.json'), { jobs: {}, sshd_connections: [], rtt_ms: null });
  const meta = readJson(path.join(runDir, 'meta.json'), {});
  if (plan === null) {
    throw new Error(`no plan.json in ${runDir}`);
  }
  const segmentCount = plan.segments.length;
  const segmentIndexes = Array.from({ length: segmentCount }, (_, index) => index);
  const faults = readJsonl(path.join(runDir, 'faults.jsonl')).filter((entry) => entry.applied);
  const faultWindows = faults.map((entry) => ({
    kind: entry.kind,
    waitIndex: entry.wait_index,
    fromMs: isoToMs(entry.started_at),
    toMs: isoToMs(entry.started_at) + entry.seconds * 1000,
  }));

  const rows = [];
  for (const build of ['before', 'after']) {
    const registrations = readJsonl(path.join(runDir, `registrations-${build}.jsonl`));
    const records = mergeRecords(runDir, build, segmentIndexes);
    const windows = segmentWindows(runDir, build, segmentIndexes);
    const jobs = groundTruth.jobs[build] ?? {};
    for (const registration of registrations) {
      const record = records.get(registration.trigger_id);
      const endMs = jobs[registration.job_id]?.end_ms ?? null;
      const createdMs = isoToMs(record?.created_at ?? registration.created_at);
      const firedMs = isoToMs(record?.fired_at ?? null);
      const fireReason = record?.status === 'fired' ? record.fire_reason : record?.status ?? 'missing';
      const firedSegment = firedMs === null ? null : fireSegment(windows, firedMs);
      const crossedRestart = firedSegment !== null && firedSegment.index > registration.segment;
      const effectiveStartMs = firedSegment?.readyAt ?? createdMs;
      const lagBaseMs = Math.max(endMs ?? -Infinity, createdMs);
      const lagMs = firedMs === null ? null : firedMs - lagBaseMs;
      const restartOffsetMs = Math.max(0, effectiveStartMs - Math.max(endMs ?? createdMs, createdMs));
      const elapsedMs = firedMs === null ? null : firedMs - effectiveStartMs;
      const intervalMs = elapsedMs === null ? null : intervalInForceMs(elapsedMs);
      const intervalBoundMs = intervalMs === null ? null : intervalMs + PROBE_NOISE_SECONDS * 1000;
      const boundMs = intervalBoundMs === null ? null : intervalBoundMs + restartOffsetMs;
      // The faults are global to the container's sshd, so every wake whose window touches a fault window is affected.
      const faulted = faultWindows.some(
        (window) => createdMs < window.toMs && (firedMs === null || firedMs > window.fromMs),
      );
      // The verify-on-create connection is made after the registration request, so a wake's connection count starts at
      // the recorded request time, not at the server's creation time a few seconds later.
      const requestedMs = isoToMs(registration.registered_at);
      const { logged, count } = countSshProcesses(path.join(runDir, 'logs'), build, registration.trigger_id);
      rows.push({
        build,
        wait: registration.wait,
        kind: registration.kind,
        length_s: registration.length,
        length_bin: lengthBin(registration.length),
        crossed_restart: crossedRestart,
        fire_reason: fireReason,
        lag_ms: lagMs,
        schedule_interval_ms: intervalMs,
        interval_bound_ms: intervalBoundMs,
        schedule_bound_ms: boundMs,
        within_interval: lagMs !== null && intervalBoundMs !== null && lagMs <= intervalBoundMs,
        within_bound: lagMs !== null && boundMs !== null && lagMs <= boundMs,
        faulted,
        ssh_processes: logged ? count : null,
        sshd_connections:
          firedMs === null
            ? null
            : sshdConnections(groundTruth, build === 'before' ? meta.ssh_users?.before : meta.ssh_users?.after, requestedMs, firedMs),
      });
    }
  }
  rows.sort((a, b) => a.wait - b.wait || a.build.localeCompare(b.build));

  const summary = { run_dir: runDir, gate, rtt_ms: groundTruth.rtt_ms ?? null, restarts: plan.restarts, faults: faultWindows.map((window) => ({ kind: window.kind, wait: window.waitIndex })), builds: {} };
  for (const build of ['before', 'after']) {
    const buildRows = rows.filter((row) => row.build === build);
    const lagged = buildRows.filter((row) => row.lag_ms !== null).map((row) => row.lag_ms);
    const completed = buildRows.filter((row) => row.fire_reason === 'completed');
    const summaryBuild = {
      waits: buildRows.length,
      completed: completed.length,
      missed: buildRows.filter((row) => row.fire_reason !== 'completed').map((row) => ({ wait: row.wait, reason: row.fire_reason, kind: row.kind, length_s: row.length_s, faulted: row.faulted })),
      lag: stats(lagged),
      lag_by_bin: {},
      lag_by_restart: {
        crossed: stats(buildRows.filter((row) => row.crossed_restart && row.lag_ms !== null).map((row) => row.lag_ms)),
        not_crossed: stats(buildRows.filter((row) => !row.crossed_restart && row.lag_ms !== null).map((row) => row.lag_ms)),
      },
      ssh_processes: {
        logged: buildRows.some((row) => row.ssh_processes !== null),
        total: buildRows.reduce((sum, row) => sum + (row.ssh_processes ?? 0), 0),
      },
      sshd_connections: buildRows.reduce((sum, row) => sum + (row.sshd_connections ?? 0), 0),
      within_interval: buildRows.filter((row) => row.within_interval).length,
      within_bound: buildRows.filter((row) => row.within_bound).length,
      outside_bound: buildRows.filter((row) => row.lag_ms !== null && !row.within_bound).map((row) => ({ wait: row.wait, lag_ms: row.lag_ms, bound_ms: row.schedule_bound_ms, faulted: row.faulted, crossed_restart: row.crossed_restart })),
    };
    for (const bin of LENGTH_BINS) {
      summaryBuild.lag_by_bin[bin.label] = stats(
        buildRows.filter((row) => row.length_bin === bin.label && row.lag_ms !== null).map((row) => row.lag_ms),
      );
    }
    summary.builds[build] = summaryBuild;
  }

  const beforeByWait = new Map(rows.filter((row) => row.build === 'before').map((row) => [row.wait, row]));
  const afterByWait = new Map(rows.filter((row) => row.build === 'after').map((row) => [row.wait, row]));
  const parityFailures = [];
  for (const [wait, before] of beforeByWait) {
    const after = afterByWait.get(wait);
    if (before.fire_reason === 'completed' && after?.fire_reason !== 'completed') {
      parityFailures.push({ wait, before: before.fire_reason, after: after?.fire_reason ?? 'missing' });
    }
  }
  summary.parity = { checked: beforeByWait.size, failures: parityFailures };

  summary.gate_failures = evaluateGate(summary, rows, gate);
  summary.passed = summary.gate_failures.length === 0;
  return { rows, summary };
}

function evaluateGate(summary, rows, gate) {
  const failures = [];
  const before = summary.builds.before;
  const after = summary.builds.after;
  if (summary.parity.failures.length > 0) {
    failures.push(`parity: ${summary.parity.failures.length} wait(s) completed on before but not after`);
  }
  if (gate === 'smoke') {
    for (const [build, buildSummary] of Object.entries(summary.builds)) {
      if (buildSummary.missed.length > 0) {
        failures.push(`${build}: ${buildSummary.missed.length} wait(s) did not fire completed`);
      }
    }
    for (const row of rows) {
      // A fault window is an outage on the container's link: today's server cannot probe through it either, so the
      // schedule bound does not apply to a wake it touches. The wake must still fire completed, which the earlier
      // check requires.
      if (row.lag_ms !== null && !row.faulted && !row.within_bound) {
        failures.push(
          `wait ${row.wait} (${row.build}): lag ${Math.round(row.lag_ms)} ms outside the schedule bound ${row.schedule_bound_ms} ms`,
        );
      }
    }
  } else {
    for (const [build, buildSummary] of Object.entries(summary.builds)) {
      if (buildSummary.missed.length > 0) {
        failures.push(`${build}: ${buildSummary.missed.length} wait(s) did not fire completed`);
      }
    }
    if (after.lag.mean_ms === null || after.lag.mean_ms > AFTER_MEAN_TARGET_MS) {
      failures.push(`after mean lag ${after.lag.mean_ms} ms is above the ${AFTER_MEAN_TARGET_MS} ms target`);
    }
  }
  return failures;
}

function formatMs(value) {
  return value === null || value === undefined ? '-' : `${(value / 1000).toFixed(1)}s`;
}

export function formatReport(summary) {
  const lines = [];
  lines.push(`remote-wake run ${summary.run_dir}`);
  lines.push(`rtt=${summary.rtt_ms} ms restarts=${summary.restarts.length} faults=${summary.faults.map((fault) => `${fault.kind}@wait${fault.wait}`).join(',')}`);
  lines.push('');
  const header = ['build', 'waits', 'completed', 'mean', 'median', 'p90', 'max', 'interval/bound-ok', 'ssh-proc', 'tcp-conn'];
  lines.push(header.join('\t'));
  for (const build of ['before', 'after']) {
    const item = summary.builds[build];
    lines.push(
      [
        build,
        item.waits,
        item.completed,
        formatMs(item.lag.mean_ms),
        formatMs(item.lag.median_ms),
        formatMs(item.lag.p90_ms),
        formatMs(item.lag.max_ms),
        `${item.within_interval}/${item.within_bound}/${item.lag.n}`,
        item.ssh_processes.logged ? item.ssh_processes.total : 'not-logged',
        item.sshd_connections,
      ].join('\t'),
    );
  }
  lines.push('');
  lines.push('lag by length bin (mean/median/p90/max, n)');
  for (const bin of LENGTH_BINS) {
    const cells = ['before', 'after'].map((build) => {
      const item = summary.builds[build].lag_by_bin[bin.label];
      return `${formatMs(item.mean_ms)}/${formatMs(item.median_ms)}/${formatMs(item.p90_ms)}/${formatMs(item.max_ms)} n=${item.n}`;
    });
    lines.push(`  ${bin.label}\tbefore ${cells[0]}\tafter ${cells[1]}`);
  }
  lines.push('');
  lines.push('lag by restart crossing (mean/median/p90/max, n)');
  for (const key of ['crossed', 'not_crossed']) {
    const cells = ['before', 'after'].map((build) => {
      const item = summary.builds[build].lag_by_restart[key];
      return `${formatMs(item.mean_ms)}/${formatMs(item.median_ms)}/${formatMs(item.p90_ms)}/${formatMs(item.max_ms)} n=${item.n}`;
    });
    lines.push(`  ${key}\tbefore ${cells[0]}\tafter ${cells[1]}`);
  }
  lines.push('');
  lines.push(`parity: checked ${summary.parity.checked}, failures ${summary.parity.failures.length}`);
  for (const failure of summary.parity.failures) {
    lines.push(`  wait ${failure.wait}: before=${failure.before} after=${failure.after}`);
  }
  lines.push('');
  if (summary.gate_failures.length === 0) {
    lines.push(`gate ${summary.gate}: PASS`);
  } else {
    lines.push(`gate ${summary.gate}: FAIL`);
    for (const failure of summary.gate_failures) {
      lines.push(`  ${failure}`);
    }
  }
  return `${lines.join('\n')}\n`;
}

function toTsv(rows) {
  const columns = [
    'build', 'wait', 'kind', 'length_s', 'length_bin', 'crossed_restart', 'fire_reason', 'lag_ms',
    'schedule_bound_ms', 'within_bound', 'faulted', 'ssh_processes', 'sshd_connections',
  ];
  const lines = [columns.join('\t')];
  for (const row of rows) {
    lines.push(columns.map((column) => (row[column] === null ? '' : row[column])).join('\t'));
  }
  return `${lines.join('\n')}\n`;
}

const invokedDirectly = process.argv[1] && path.resolve(process.argv[1]) === new URL(import.meta.url).pathname;
if (invokedDirectly) {
  const args = new Map();
  for (let index = 2; index < process.argv.length; index += 2) {
    args.set(process.argv[index], process.argv[index + 1]);
  }
  const runDir = args.get('--run-dir');
  if (!runDir) {
    console.error('report.mjs: --run-dir is required');
    process.exit(1);
  }
  try {
    const { rows, summary } = buildReport({ runDir, gate: args.get('--gate') ?? 'smoke' });
    fs.writeFileSync(path.join(runDir, 'rows.tsv'), toTsv(rows));
    fs.writeFileSync(path.join(runDir, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
    const text = formatReport(summary);
    fs.writeFileSync(path.join(runDir, 'report.txt'), text);
    process.stdout.write(text);
    process.exit(summary.passed ? 0 : 3);
  } catch (error) {
    console.error(`remote-wake report: ${error.stack ?? error}`);
    process.exit(1);
  }
}
