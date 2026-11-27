// TEMPORARY (task 1795738729, the For-you open follow-ups repro; this file never lands). The registration loop of
// ~/scripts/20261126_foryou_open_followups/report.md, "Repro bench (after plan 7's A/A)", folded into its Q3 combined
// loop. Each load is the landed first-open cell's flow (foryou.mjs measureOpenCell) in a fresh profile behind a fresh
// proxy, with two changes: the cell navigates to its target whatever the warm reported, so a failed or silent warm still
// yields a classed open, and the browser attaches DevTools to the origin's service worker targets only in the `on` arm.
//
// Per load the record holds the warm report and the on-demand-files report, the worker targets' created, destroyed and
// attach times, the page's console lines and errors, the first page's worker state, the three facts that separate a
// held install from a late activation when the warm stays silent (worker state, open user-waiting requests, update
// states), and the target open: first-row ms, document bytes, the target's worker state, and the page's and the
// worker's request traces. Times are wall-clock ms, as the server log's are; the record's `t0` is the first navigation.
//
// The arms alternate load by load, and the order flips every unit, so each arm gets 20 large and 20 typical targets out
// of 80 loads. A unit is one bench step of up to LOADS_PER_UNIT loads; it starts no load past UNIT_BUDGET_MS, so a step
// stays well under the gate's 600 s cap, and a body left behind by a capped step stops at its next load.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { forYouInitScript } from './foryou.mjs';
import { DOCUMENT_WORKER_STATE, WARM_REPORT_INIT, reportOnWindow } from './open.mjs';
import { withLink } from './scenarios.mjs';
import { StepCapped } from './segments.mjs';
import { PAGE_REPORT_BINDING, pollFor } from './tabs.mjs';

export const REGLOOP_SCENARIO = 'regloop';

const LOADS_PER_UNIT = 4;
const UNIT_BUDGET_MS = 330000;
// The landed cell's warm deadline (foryou.mjs OPEN_WARM_TIMEOUT_MS), for both the warm and the files report.
const WARM_TIMEOUT_MS = 60000;
const FILES_TIMEOUT_MS = 60000;
// A network-path open on this link reaches its first row in 9-12 s (the source report's Q3 table).
const FIRST_ROW_TIMEOUT_MS = 30000;
// The times the page saw its own events, beside the reports the warm-report init keeps.
const EVENT_TIMES_INIT =
  "document.addEventListener('charliebot:page-current', () => { window.__pageCurrentAt = Date.now(); });\n" +
  "document.addEventListener('charliebot:offline-shell-warm', () => { window.__warmAt = Date.now(); });\n" +
  "document.addEventListener('charliebot:offline-shell-files-stored', () => { window.__filesAt = Date.now(); });";
// The page's install-hold state when the warm stays silent: its open user-waiting requests and the update states it saw
// (offline-shell.js keeps both at the top level of its classic script).
const HOLD_STATE = `({
  userRequestsOpen: typeof userRequestsOpen === 'undefined' ? null : [...userRequestsOpen],
  updateStates: typeof updateStates === 'undefined' ? null : updateStates,
})`;

let generation = 0;

// The planned loads: index 1..count, the arm by alternation (flipped every unit), the target by pairs.
export function plannedLoads(count) {
  const loads = [];
  for (let index = 1; index <= count; index++) {
    const unit = Math.floor((index - 1) / LOADS_PER_UNIT);
    const onFirst = unit % 2 === 0;
    const arm = (index % 2 === 1) === onFirst ? 'on' : 'off';
    const role = Math.floor((index - 1) / 2) % 2 === 0 ? 'large' : 'typical';
    loads.push({ index, arm, role });
  }
  return loads;
}

function relative(value, t0) {
  return value === null || value === undefined ? null : value - t0;
}

function workerTargetsOf(tabs, t0) {
  return [...tabs.workerTargets.entries()].map(([targetId, record]) => ({
    targetId,
    url: record.url,
    createdMs: relative(record.createdAt, t0),
    destroyedMs: relative(record.destroyedAt, t0),
    detachedMs: relative(record.detachedAt, t0),
    crashedMs: relative(record.crashedAt, t0),
    crash: record.crash,
    infoChanges: record.infoChanges.map((change) => ({ ...change, at: relative(change.at, t0) })),
    attach: record.attach && {
      requestedMs: relative(record.attach.requestedAt, t0),
      attachedMs: relative(record.attach.attachedAt, t0),
      enabledMs: relative(record.attach.enabledAt, t0),
      error: record.attach.error,
      errorMs: relative(record.attach.errorAt, t0),
    },
  }));
}

function traceRequests(requests) {
  return requests.map((request) => ({
    url: request.url.replace(/^https:\/\/localhost:\d+/, ''),
    type: request.type,
    status: request.status,
    wireStatus: request.wireStatus,
    fromServiceWorker: request.fromServiceWorker,
    fromDiskCache: request.fromDiskCache,
    failed: request.failed,
    startMs: request.startMs,
    finishMs: request.finishMs,
    bytes: request.bytes,
  }));
}

// The warm's outcome in one word, for the log line and the report's counts.
export function warmOutcome(warm) {
  if (warm === null) return 'silent';
  if (warm.stored === true) return 'stored';
  if (warm.error !== undefined) return 'error';
  if (warm.supported === false) return 'unsupported';
  return 'other';
}

async function oneLoad(ctx, plan, unitName) {
  const target = ctx.sessions.foryou[plan.role];
  const record = {
    ...plan, unit: unitName, targetId: target.id, t0: null, load1Start: os.loadavg()[0], load1End: null, error: null,
  };
  try {
    await withLink(ctx, `rl${plan.index}`, [], async ({ tabs }) => {
      const documentPattern = new RegExp(`\\?session=${target.id}$`);
      const tab = await tabs.open(
        `regloop-${plan.index}`, `${WARM_REPORT_INIT}\n${EVENT_TIMES_INIT}\n${forYouInitScript(target.marker)}`);
      await tab.page.send('Runtime.addBinding', { name: PAGE_REPORT_BINDING });
      const t0 = Date.now();
      record.t0 = t0;
      try {
        await tabs.navigate(tab, `/?session=${ctx.sessions.open.id}`);
        await tab.ready();
        record.readyMs = Date.now() - t0;
        record.warm = await reportOnWindow(tab, '__warmReport', WARM_TIMEOUT_MS);
        record.warmOutcome = warmOutcome(record.warm);
        const times = await tab.evaluate('({pageCurrentAt: window.__pageCurrentAt ?? null, warmAt: window.__warmAt ?? null})');
        record.pageCurrentMs = relative(times.pageCurrentAt, t0);
        record.warmMs = relative(times.warmAt, t0);
        record.firstPageWorkerState = await tab.evaluate(DOCUMENT_WORKER_STATE);
        if (record.warm === null) {
          record.holdState = await tab.evaluate(HOLD_STATE);
        }
        record.filesStored = null;
        if (record.warmOutcome === 'stored') {
          record.filesStored = await reportOnWindow(tab, '__filesStored', FILES_TIMEOUT_MS);
          record.filesMs = relative(await tab.evaluate('window.__filesAt ?? null'), t0);
        }
        const navigatedAt = Date.now();
        record.targetNavigatedMs = navigatedAt - t0;
        await tabs.navigate(tab, `/?session=${target.id}`);
        const report = await pollFor(() => {
          const reports = tab.reports.filter(
            (entry) => entry.kind === 'foryou' && entry.at && documentPattern.test(entry.href));
          return reports[reports.length - 1] ?? null;
        }, FIRST_ROW_TIMEOUT_MS);
        record.firstRowMs = report === null ? null : report.at - navigatedAt;
        record.documentBytes = tab.bytesOf(documentPattern, navigatedAt, Infinity);
        record.targetWorkerState = await tab.evaluate(DOCUMENT_WORKER_STATE);
      } finally {
        record.workerTargets = workerTargetsOf(tabs, t0);
        record.console = tab.consoleLines.map((line) => ({ at: line.at - t0, text: line.text }));
        record.pageErrors = tab.pageErrors.map((line) => ({ at: line.at - t0, text: line.text }));
        record.pageTrace = traceRequests(tab.traceFrom(t0).requests);
        record.workerTrace = traceRequests(tabs.workerTrace(t0));
      }
    }, null, { attachWorkers: plan.arm === 'on' });
  } catch (caught) {
    record.error = caught.stack ?? String(caught);
  }
  record.load1End = os.loadavg()[0];
  return record;
}

function summary(record) {
  const targets = (record.workerTargets ?? []).map((target) =>
    `created ${target.createdMs} destroyed ${target.destroyedMs}` +
      (target.attach?.error ? ` attach-error ${target.attach.errorMs}` : ''));
  return `load ${record.index} arm=${record.arm} target=${record.role} warm=${record.warmOutcome ?? '-'}` +
    `${record.warm?.error ? ` (${record.warm.error})` : ''} warmMs=${record.warmMs ?? '-'} ` +
    `firstRowMs=${record.firstRowMs ?? '-'} doc=${record.documentBytes ?? '-'} workers=[${targets.join('; ')}]` +
    `${record.error ? ` ERROR ${record.error.split('\n')[0]}` : ''}`;
}

// The loop: `count` planned loads, run in units of the bench (ctx.unit), each record appended to `results` and to
// regloop.jsonl in the run directory as it ends. A unit that passes its cap twice is named in `cappedSteps`.
export async function regLoopScenario(ctx, count, results, cappedSteps) {
  const queue = plannedLoads(count);
  const jsonl = path.join(ctx.runDir, 'regloop.jsonl');
  let unitNumber = 0;
  while (queue.length > 0) {
    unitNumber += 1;
    const name = `regloop ${unitNumber}`;
    try {
      await ctx.unit('cell', name, async () => {
        const mine = ++generation;
        const startedAt = Date.now();
        let done = 0;
        while (queue.length > 0 && done < LOADS_PER_UNIT && Date.now() - startedAt < UNIT_BUDGET_MS &&
            mine === generation) {
          const record = await oneLoad(ctx, queue.shift(), name);
          results.push(record);
          fs.appendFileSync(jsonl, `${JSON.stringify(record)}\n`);
          process.stderr.write(`[regloop] ${new Date().toISOString()} ${summary(record)}\n`);
          done += 1;
        }
      });
    } catch (caught) {
      if (!(caught instanceof StepCapped)) {
        throw caught;
      }
      cappedSteps.push(caught.message);
    }
  }
}
