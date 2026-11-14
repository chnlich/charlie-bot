// The open scenario: the cases of plan 2 section 3, A (this browser opened the session before), B (it opened a chat
// page before, but not this session) and C (it never opened one), under the link's conditions. Every cell runs in a
// fresh browser profile behind a fresh proxy. The CDN files cross the shaped link, from the CDN stand-in (cdn.mjs), as
// the page's own bytes do. A cell's five repetitions share the warm profile of their case; each repetition copies it.
//
// The cells run in this order. A reference cell (A, link, CPU rate 1) comes first, and its CPU tail sets the CPU rate
// of the cells that follow. The other conditions come next, at that rate. The release cells come last, because each
// one changes the server's resource version and the warm profile of its case is built at the version it then serves.

import fs from 'node:fs';
import path from 'node:path';

import { launchBrowser, chromeBinary } from './chrome.mjs';
import { cdnRoutes } from './cdn.mjs';
import { LINK, OPEN, LIVE_CPU_TAIL } from './profile.mjs';
import { ShapingProxy } from './proxy.mjs';
import { percentile } from './report.mjs';
import { NETWORK_CHANGED_ERROR, Tabs, sleep } from './tabs.mjs';

const CONDITIONS = ['link', 'held', 'reset', 'flaps', 'offline'];
const CASES = ['A', 'B', 'C'];
// The conditions a run selects with --open-conditions: the link conditions, then the release cells.
export const OPEN_CONDITIONS = Object.freeze([...CONDITIONS, 'release']);
const REFERENCE = Object.freeze({ id: 'reference A link rate 1', case: 'A', condition: 'link', rate: 1 });
const SETTLE_MS = 300000;
// The request a browser makes before a repetition to open a connection that stays idle in its pool. A and B have
// opened CharlieBot before, so their browsers hold such a connection; C's does not, so it holds none.
const PRECONNECT_PATH = '/api/concurrency/now';
const RELEASE_MARK = '// bench release';

function randomTag() {
  return Math.random().toString(16).slice(2, 10);
}

// The median of a list of numbers, or null when no value is there: a cell whose repetitions all missed the
// newest message has no median.
function median(values) {
  const sorted = values.filter((value) => value !== null).sort((a, b) => a - b);
  return sorted.length === 0 ? null : percentile(sorted, 0.5);
}

// The CPU rate that makes the bench's CPU tail match the live one. A rate below 1 would speed the browser up, so the
// rate is at least 1.
export function cpuRateFrom(liveTailMs, referenceTailMs) {
  if (!(referenceTailMs > 0)) {
    throw new Error(`the reference CPU tail is ${referenceTailMs} ms; the rate cannot be derived from it`);
  }
  return Math.max(1, Math.round((liveTailMs / referenceTailMs) * 100) / 100);
}

// The CPU record of an open run: the rate the invocation's cells run at — the shared rate when the invocation
// passes one, so the sides of one `--side both` run compare at one CPU speed — beside the inputs. `derivedRate` is
// what this side's own reference tail gives, so a side that ran at the other side's rate shows the gap.
export function openCpuRecord(sharedCpuRate, referenceTails) {
  const referenceTail = median(referenceTails);
  const derivedRate = cpuRateFrom(LIVE_CPU_TAIL.medianMs, referenceTail);
  return {
    rate: sharedCpuRate ?? derivedRate,
    derivedRate,
    rateSource: sharedCpuRate === null ? "this side's reference cell" : "the before side's reference cell",
    liveTailMs: LIVE_CPU_TAIL.medianMs,
    liveDevice: LIVE_CPU_TAIL.device,
    liveLoads: LIVE_CPU_TAIL.loads,
    referenceTailMedianMs: referenceTail,
    referenceTails,
  };
}

// The cause a repetition names when the host's network changed under it, or null when the repetition is a page
// result. Chrome drops every connection when the host's interfaces change and fails the requests in flight with
// `net::ERR_NETWORK_CHANGED`, a document cut mid-body included; such a repetition measures the host, not the page.
export function hostInterference(rep) {
  if (rep.networkChanged.length > 0) {
    const named = rep.networkChanged.slice(0, 3).map((failure) => {
      const url = new URL(failure.url);
      return (url.pathname + url.search).slice(0, 60);
    });
    const rest = rep.networkChanged.length - named.length;
    return `Chrome dropped ${rep.networkChanged.length} request(s) on a host network change ` +
      `(${named.join(', ')}${rest > 0 ? `, ${rest} more` : ''})`;
  }
  if (rep.navigationError === NETWORK_CHANGED_ERROR) {
    return `the navigation failed with ${NETWORK_CHANGED_ERROR}`;
  }
  return null;
}

// One cell's repetitions. A repetition the host's network change spoiled is rerun — at most
// OPEN.interferenceReruns times, and each rerun recorded — because it never counts as the page's result; any other
// failure is the rep's record, as before. A repetition that still meets the interference at the bound fails the run.
export async function runRepetitions(measure, repetitions, results = []) {
  for (let repetition = 1; repetition <= repetitions; repetition++) {
    const reruns = [];
    let rep = await measure(repetition);
    let interference = hostInterference(rep);
    while (interference !== null && reruns.length < OPEN.interferenceReruns) {
      reruns.push({ reranAt: new Date().toISOString(), cause: interference });
      process.stderr.write(`[open] host interference, rerunning (${reruns.length}/${OPEN.interferenceReruns}): ` +
        `${interference}\n`);
      rep = await measure(repetition);
      interference = hostInterference(rep);
    }
    if (interference !== null) {
      // Keep the failed repetition and every rerun in the side's JSON; the failed run never formats its medians.
      results.push({ ...rep, interferenceReruns: reruns, hostInterference: interference });
      throw new Error(`the repetition still met host interference after ${reruns.length} reruns: ${interference}`);
    }
    results.push({ ...rep, interferenceReruns: reruns });
  }
  return results;
}

// The resets of the repeated drops: the link is up for upSeconds, then down for downSeconds, until totalSeconds.
// Each down window resets every connection when it starts.
export function flapWindows({ upSeconds, downSeconds, totalSeconds }) {
  const windows = [];
  for (let start = upSeconds; start < totalSeconds; start += upSeconds + downSeconds) {
    windows.push({ at: start * 1000, seconds: Math.min(downSeconds, totalSeconds - start), mode: 'reset' });
  }
  return windows;
}

// The init script of a measured page. It runs before the page's own scripts. It records the navigation's time origin,
// the localStorage record the page starts from, the first time the newest message is in the chat, the first time the
// not-current mark is in the document, the DOMContentLoaded and load times, and the load time's clock. The newest
// message is the first screen's last message, named by its `data-message-id`: the page's own `newest_message_ms`
// definition (web/features/diag/templates/diag/client-events.js). A text marker goes stale: the scenarios that run
// before the open scenario send turns to the same session, so the seed's text no longer sits in the first screen.
function measuredInitScript(newestMessageId) {
  return `(() => {
    const selector = '[data-message-id="' + CSS.escape(${JSON.stringify(String(newestMessageId))}) + '"]';
    const state = {
      timeOrigin: performance.timeOrigin,
      recordAtStart: localStorage.getItem('charliebot_opened_pages'),
      newest: null,
      mark: null,
      domContentLoaded: null,
      load: null,
    };
    window.__pn = state;
    const scan = () => {
      if (state.newest === null && document.querySelector(selector) !== null) {
        state.newest = Date.now();
      }
      if (state.mark === null && document.querySelector('[data-freshness="not-current"]') !== null) {
        state.mark = Date.now();
      }
    };
    new MutationObserver(scan).observe(document, { childList: true, subtree: true, characterData: true });
    document.addEventListener('DOMContentLoaded', () => { state.domContentLoaded = performance.now(); });
    window.addEventListener('load', () => { state.load = performance.now(); });
  })();`;
}

// The page's own case: the mechanism of web/static/js/opened-sessions.js, read at the end of the page's load. A side
// whose client predates the mechanism (origin/main before the telemetry lands) has none, and reports null.
async function pageOpenCase(tab) {
  return tab.evaluate("typeof OpenedSessions === 'undefined' ? null : OpenedSessions.openCase()");
}

// The case the browser's record names for a session, from the record the page starts from.
function caseFromRecord(recordAtStart, sessionId) {
  if (recordAtStart === null) {
    return 'C';
  }
  return JSON.parse(recordAtStart).sessions[sessionId] === undefined ? 'B' : 'A';
}

// The asset version a chat page's body declares. The server folds the whitespace of an inlined script, so the
// declaration arrives as `const STATIC_ASSET_VERSION="…";`, and the match takes either spacing.
export function assetVersionOf(body) {
  const match = /const STATIC_ASSET_VERSION\s*=\s*("[^"]*")\s*;/.exec(body);
  if (match === null) {
    throw new Error('the chat page carries no STATIC_ASSET_VERSION');
  }
  return JSON.parse(match[1]);
}

// The asset version the server serves to a page of `sessionId`.
async function assetVersion(api, sessionId) {
  const { status, body } = await api.request('GET', `/?session=${sessionId}`);
  if (status !== 200) {
    throw new Error(`GET the chat page answered ${status}`);
  }
  return assetVersionOf(body);
}

// A release: the server's static asset version changes. The bench appends a comment to the served config.js of the
// trial's tree, which the server digests into its version, then waits until the version the server answers has moved.
async function release(ctx, sessionId) {
  const root = process.env.CHARLIEBOT_REPO_ROOT;
  if (!root) {
    throw new Error('CHARLIEBOT_REPO_ROOT must name the trial tree that the server serves static files from');
  }
  const before = await assetVersion(ctx.api, sessionId);
  ctx.releases += 1;
  fs.appendFileSync(path.join(root, 'web', 'static', 'js', 'config.js'), `\n${RELEASE_MARK} ${ctx.releases}\n`);
  const deadline = Date.now() + 15000;
  for (;;) {
    const after = await assetVersion(ctx.api, sessionId);
    if (after !== before) {
      return { before, after };
    }
    if (Date.now() > deadline) {
      throw new Error(`the release did not change the static asset version from ${before}`);
    }
    await sleep(250);
  }
}

// A warm profile: a browser that has opened the case's session (A) or another session (B) before, with its CDN files
// in the HTTP cache and its record in localStorage. The record is the browser state the case names. The page writes it
// when its client has the mechanism; the bench writes the same record when the side's client predates it, so both
// sides start from the same browser state. C has no profile: its browser starts empty.
async function buildWarm(ctx, caseName) {
  const dir = path.join(ctx.runDir, `warm-${caseName}-${randomTag()}`);
  if (caseName === 'C') {
    fs.mkdirSync(dir, { recursive: true });
    return dir;
  }
  const session = caseName === 'A' ? ctx.sessions.switchTarget : ctx.sessions.probes[0];
  const link = await startLink(ctx);
  const browser = await launchBrowser({
    binary: chromeBinary(), proxyPort: link.port, userDataDir: dir, logPath: `${ctx.runDir}/chrome.log`,
    trustedSpki: [ctx.trustedSpki, ctx.cdnSpki].filter(Boolean).join(","),
  });
  try {
    const tabs = new Tabs({
      browser, assets: ctx.assets, accessKey: ctx.api.accessKey, origin: ctx.origin, cdn: 'link',
    });
    const tab = await tabs.open('warm');
    await tabs.navigate(tab, `/?session=${session.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_MS);
    const now = Date.now();
    const record = { first_open_ms: now, sessions: { [session.id]: now } };
    await tab.evaluate(`localStorage.setItem('charliebot_opened_pages', ${JSON.stringify(JSON.stringify(record))})`);
    await tabs.close(tab);
  } finally {
    await browser.shutdown();
    await link.proxy.stop();
  }
  return dir;
}

async function startLink(ctx) {
  const proxy = new ShapingProxy({
    target: { host: '127.0.0.1', port: ctx.frontPort },
    routes: cdnRoutes(ctx.cdnPort),
  });
  const port = await proxy.start();
  return { proxy, port };
}

// The byte counter's value at `at` (wall-clock ms): the last sample taken at or before it.
function countersAt(timeline, at) {
  let found = timeline[0];
  for (const sample of timeline) {
    if (sample.t > at) {
      break;
    }
    found = sample;
  }
  return found.sent + found.received;
}

// One measured open: the browser, the link's conditions and the page's timings.
async function measureOnce(ctx, cell, repetition) {
  const session = ctx.sessions.switchTarget;
  const profile = path.join(ctx.runDir, `open-${cell.id.replace(/\W+/g, '-')}-${repetition}-${randomTag()}`);
  fs.cpSync(cell.warmDir, profile, { recursive: true });
  const link = await startLink(ctx);
  const timeline = [];
  const browser = await launchBrowser({
    binary: chromeBinary(), proxyPort: link.port, userDataDir: profile, logPath: `${ctx.runDir}/chrome.log`,
    trustedSpki: [ctx.trustedSpki, ctx.cdnSpki].filter(Boolean).join(","),
  });
  try {
    const tabs = new Tabs({
      browser, assets: ctx.assets, accessKey: ctx.api.accessKey, origin: ctx.origin, cdn: 'link',
    });
    if (cell.case !== 'C') {
      // The browser's previous visit leaves a connection in its pool. A reset in the blackout then ends that
      // connection.
      const warm = await tabs.open('precontact');
      await tabs.navigate(warm, PRECONNECT_PATH);
      await sleep(500);
      await tabs.close(warm);
    }

    if (cell.condition === 'held' || cell.condition === 'reset') {
      // The condition names the cell; the proxy's modes are hold and reset.
      const mode = cell.condition === 'held' ? 'hold' : 'reset';
      link.proxy.blackout({ seconds: LINK.blackoutSeconds, mode });
      await sleep(OPEN.blackoutLeadMs);
    } else if (cell.condition === 'release') {
      link.proxy.blackout({ seconds: LINK.blackoutSeconds, mode: 'hold' });
      await sleep(OPEN.releaseAtMs);
      await release(ctx, session.id);
      await sleep(OPEN.openAfterMs - OPEN.releaseAtMs);
    }

    // The newest message the measured open must show: the first screen's last message, fetched now, after the
    // release condition has landed its turn. The request runs beside the link, so it costs the measurement no bytes.
    const firstScreen = await ctx.api.json('GET', `/api/sessions/${session.id}/bootstrap`);
    const newestMessage = firstScreen.messages[firstScreen.messages.length - 1];
    if (newestMessage === undefined || newestMessage.id == null) {
      throw new Error(`the open target's first screen (${firstScreen.messages.length} messages) names no last message id`);
    }
    const tab = await tabs.open('open', measuredInitScript(newestMessage.id));
    await tab.page.send('Emulation.setCPUThrottlingRate', { rate: cell.rate });
    await tabs.show(tab);
    if (cell.condition === 'offline') {
      await tab.page.send('Network.emulateNetworkConditions', {
        offline: true, latency: 0, downloadThroughput: -1, uploadThroughput: -1,
      });
    }

    if (cell.condition === 'flaps') {
      link.proxy.schedule(flapWindows(OPEN.flaps));
    }
    // The navigation is not awaited: a navigation inside a hold answers when the link does, and the bytes until then
    // must still be sampled. Its answer is read after the loop.
    let navigationError = null;
    const navStart = Date.now();
    timeline.push({ t: navStart, ...link.proxy.counters() });
    const navigated = tabs.navigate(tab, `/?session=${session.id}`).then((answer) => {
      navigationError = answer.errorText ?? null;
    });
    let state = null;
    for (;;) {
      const now = Date.now();
      timeline.push({ t: now, ...link.proxy.counters() });
      state = navigationError === null ? await tab.evaluate('window.__pn ?? null') : null;
      const catchupSeen = tab.frames.some((frame) => frame.type === 'catchup_complete' && frame.at >= navStart);
      const finished = state !== null && state.newest !== null && catchupSeen && state.load !== null &&
        now >= loadWall(state) + OPEN.postLoadWindowMs;
      const offlineDone = navigationError !== null && now >= navStart + OPEN.offlineSettleMs;
      if (finished || offlineDone || now >= navStart + OPEN.capMs) {
        break;
      }
      await sleep(25);
    }
    const end = Date.now();
    timeline.push({ t: end, ...link.proxy.counters() });
    await navigated;

    const pageState = navigationError === null && state !== null ? state : null;
    const catchup = tab.frames.find((frame) => frame.type === 'catchup_complete' && frame.at >= navStart) ?? null;
    const loadWallMs = pageState !== null && pageState.load !== null ? loadWall(pageState) : null;
    const newestWall = pageState !== null && pageState.newest !== null ? pageState.newest : null;
    const responseEnd = navigationError === null
      ? await tab.evaluate("performance.getEntriesByType('navigation')[0]?.responseEnd ?? null")
      : null;
    const openCase = navigationError === null ? await pageOpenCase(tab) : null;
    if (openCase !== null && openCase !== cell.case) {
      throw new Error(`the page of ${cell.id} opened as case ${openCase}, not ${cell.case}`);
    }
    if (navigationError === null && caseFromRecord(pageState.recordAtStart, session.id) !== cell.case) {
      throw new Error(`the record ${cell.id} starts from does not name case ${cell.case}`);
    }
    const timeOrigin = pageState !== null ? pageState.timeOrigin : null;
    const dclMs = pageState !== null && pageState.domContentLoaded !== null ? pageState.domContentLoaded : null;

    const frameTypes = {};
    for (const frame of tab.frames) {
      frameTypes[frame.type ?? 'unparsed'] = (frameTypes[frame.type ?? 'unparsed'] ?? 0) + 1;
    }
    // The page's own account of its socket and its failures: what `ws` the client holds, what it logged, and what it
    // threw. A missing catch-up time reads against these.
    const wsState = navigationError === null
      ? await tab.evaluate("typeof ws === 'undefined' ? 'no-ws' : ws === null ? 'null' : ws.readyState + ' " +
          "<' + ws.url.slice(ws.url.indexOf('//') + 2, 80)")
      : null;
    return {
      rep: repetition,
      navigationError,
      networkChanged: tab.networkChanged,
      newestMs: newestWall === null ? null : newestWall - timeOrigin,
      catchupMs: catchup === null || timeOrigin === null ? null : catchup.at - timeOrigin,
      sockets: tab.sockets.length,
      frames: tab.frames.length,
      frameTypes,
      requests: tab.requests.size,
      wsState,
      consoleTail: tab.consoleLines.slice(-8).map((line) => line.text),
      pageErrors: tab.pageErrors.slice(-4).map((line) => line.text),
      loadMs: pageState === null || pageState.load === null ? null : pageState.load,
      dclMs,
      responseEndMs: responseEnd,
      tailMs: dclMs === null || responseEnd === null ? null : dclMs - responseEnd,
      bytesToNewest: newestWall === null ? null : countersAt(timeline, newestWall) - countersAt(timeline, navStart),
      bytesAfterLoad: loadWallMs === null
        ? null
        : countersAt(timeline, Math.min(end, loadWallMs + OPEN.postLoadWindowMs)) - countersAt(timeline, loadWallMs),
      markSeen: pageState !== null && pageState.mark !== null,
      pageCase: openCase,
      elapsedMs: end - navStart,
    };
  } finally {
    await browser.shutdown();
    await link.proxy.stop();
    fs.rmSync(profile, { recursive: true, force: true });
  }
}

// The wall-clock time of a page's load event. The page's load time counts from its time origin, which is wall-clock.
function loadWall(state) {
  return state.timeOrigin + state.load;
}

async function runCell(ctx, cell, repetitions) {
  cell.reps = [];
  await runRepetitions(async (repetition) => {
    process.stderr.write(`[open] ${cell.id} repetition ${repetition}/${repetitions}\n`);
    // The trials that run beside a repetition share the host with it; the watch names them, as the other scenarios
    // do for their repetitions.
    const from = ctx.trials.mark();
    const rep = await measureOnce(ctx, cell, repetition);
    return {
      ...rep,
      startedAt: new Date(from).toISOString(),
      finishedAt: new Date().toISOString(),
      otherTrials: ctx.trials.since(from),
    };
  }, repetitions, cell.reps);
  return cell;
}

// The open scenario. Fills `result`, the object the JSON keeps, cell by cell: a failure part-way leaves the cells
// measured so far in the side's JSON, as the per-repetition scenarios leave their repetitions. Returns `result`.
// The reference cell always runs: the CPU rate is derived from it. `conditions` selects the other cells.
// `sharedCpuRate`, when the invocation passes one, is the rate the before side derived, which this side's cells
// then run at.
export async function openScenario(ctx, repetitions, result, conditions = OPEN_CONDITIONS, sharedCpuRate = null) {
  ctx.releases = 0;
  result.releases = 0;
  result.cells = [];
  const warm = { A: await buildWarm(ctx, 'A'), B: await buildWarm(ctx, 'B'), C: await buildWarm(ctx, 'C') };
  const reference = { ...REFERENCE, warmDir: warm[REFERENCE.case] };
  result.reference = reference;
  await runCell(ctx, reference, repetitions);
  result.cpu = openCpuRecord(sharedCpuRate, reference.reps.map((rep) => rep.tailMs));
  const cpuRate = result.cpu.rate;

  for (const condition of CONDITIONS.filter((name) => conditions.includes(name))) {
    for (const caseName of CASES) {
      const cell = {
        id: `${caseName} ${condition}`, case: caseName, condition, rate: cpuRate, warmDir: warm[caseName],
      };
      result.cells.push(cell);
      await runCell(ctx, cell, repetitions);
    }
  }
  for (const caseName of conditions.includes('release') ? CASES : []) {
    const warmDir = await buildWarm(ctx, caseName);
    const cell = { id: `${caseName} release`, case: caseName, condition: 'release', rate: cpuRate, warmDir };
    result.cells.push(cell);
    await runCell(ctx, cell, repetitions);
  }
  result.releases = ctx.releases;
  return result;
}

// The open scenario's table: one row per cell. Each time is the median, then the five values in repetition order; a
// value that was not reached is `-`, and the last column counts the repetitions that reached the newest message.
export function formatOpen(open) {
  const ms = (value) => (value === null ? '-' : `${Math.round(value)}ms`);
  const bytes = (value) => (value === null ? '-' : `${Math.round(value)}B`);
  const lines = [
    `open scenario: CPU rate ${open.cpu.rate} (live CPU tail ${open.cpu.liveTailMs} ms from ${open.cpu.liveLoads} ` +
      `loads of ${open.cpu.liveDevice}; the reference cell's tail median ` +
        `${open.cpu.referenceTailMedianMs} ms at rate 1)`,
    'cell                  newest median  newest, five reps                   catchup median  ' +
      'bytes to newest  bytes after load  reached',
  ];
  for (const cell of [open.reference, ...open.cells]) {
    const newest = cell.reps.map((rep) => rep.newestMs);
    const catchup = cell.reps.map((rep) => rep.catchupMs);
    const toNewest = cell.reps.map((rep) => rep.bytesToNewest);
    const afterLoad = cell.reps.map((rep) => rep.bytesAfterLoad);
    const reached = newest.filter((value) => value !== null).length;
    lines.push([
      cell.id.padEnd(21),
      ms(median(newest)).padStart(13),
      `[${newest.map(ms).join(' ')}]`.padEnd(34),
      ms(median(catchup)).padStart(14),
      bytes(median(toNewest)).padStart(16),
      bytes(median(afterLoad)).padStart(17),
      `${reached}/${cell.reps.length}`.padStart(8),
    ].join('  '));
  }
  return lines.join('\n');
}
