// Shared page side of the sidebar search runner. A headless Chrome page loads one CharlieBot server over the search
// plan's link: 300 ms round trip and 12,500 B/s each way, set through CDP network emulation. Every non-GET request the
// page makes is aborted, so the page cannot write to the server. The server is named by its base URL; no host is
// named here.
//
// Environment:
//   PLAYWRIGHT_MODULE      the playwright module to load (default: 'playwright', resolved from this directory)
//   CHROME_BIN             the Chrome binary to launch (default: the binary Playwright bundles)
//   CHARLIEBOT_ACCESS_KEY  the server's access key, when the server asks for one. It is sent to the server and never
//                          written to the output.
//   WARM_UP                1 (default) sends one untimed search per query term before its timed repeats. 0 times the
//                          first search after the server starts, which is a cold sample.
//   WAIT_FOR_CLEAR         1 (default): after a clear, the 'after-clear' scenario types once the clear's own requests
//                          (list, row refresh, badge) have ended. 0: it types after the fixed 2,500 ms wait.

'use strict';

const fs = require('fs');
const { LIMIT_T_S, limitFor } = require('./limits');

const SLOW = { offline: false, latency: 300, downloadThroughput: 12500, uploadThroughput: 12500 };
// After a paint, the search's counters settle this long before the stale paints of the search are read. A Workspace
// listing that paints late over the search rows is still a stale paint of that search.
const SETTLE_MS = 1500;
// The requests a clear sends: the Workspace listing, its row refresh and its cron badge.
const CLEAR_CATEGORIES = new Set(['list', 'row-refresh', 'badge']);

function accessKeyFromEnvironment() {
  return process.env.CHARLIEBOT_ACCESS_KEY || '';
}

// Whether the runner warms each query term before its timed searches. Any value but 0 or 1 stops the run.
function warmUpFromEnvironment() {
  const value = process.env.WARM_UP ?? '1';
  if (value !== '0' && value !== '1') {
    throw new Error(`WARM_UP must be 0 or 1, got '${value}'`);
  }
  return value === '1';
}

// Whether an 'after-clear' search waits for the clear's requests. Any value but 0 or 1 stops the run.
function waitForClearFromEnvironment() {
  const value = process.env.WAIT_FOR_CLEAR ?? '1';
  if (value !== '0' && value !== '1') {
    throw new Error(`WAIT_FOR_CLEAR must be 0 or 1, got '${value}'`);
  }
  return value === '1';
}

// The category of a request the page makes, by its path. A path the runner does not name is 'other'.
function categoryOf(url) {
  const { pathname, searchParams } = new URL(url);
  if (pathname === '/api/sessions/') return searchParams.has('ids') ? 'row-refresh' : 'list';
  if (pathname === '/api/sessions/status') return 'status';
  if (pathname === '/api/sessions/search') return 'search';
  if (pathname === '/api/cron/tasks') return 'badge';
  if (/^\/api\/sessions\/[^/]+\/transcript$/.test(pathname)) return 'transcript';
  if (/^\/api\/sessions\/[^/]+\/usage$/.test(pathname)) return 'usage';
  if (pathname.startsWith('/api/concurrency/')) return 'concurrency';
  return 'other';
}

// Every request the page makes once its CDP session is open. Each record keeps the time it was sent, the time it ended
// and how it ended (finished, aborted by the page, or failed), and each response chunk with the wire bytes the network
// reports for it. Times are this process's wall clock, the same clock the page's input events carry.
function trackNetwork(cdp) {
  const byId = new Map();
  const list = [];
  cdp.on('Network.requestWillBeSent', (event) => {
    const record = {
      url: event.request.url,
      category: categoryOf(event.request.url),
      sentWall: Date.now(),
      chunks: [],
      endWall: null,
      outcome: 'pending',
    };
    byId.set(event.requestId, record);
    list.push(record);
  });
  cdp.on('Network.dataReceived', (event) => {
    const record = byId.get(event.requestId);
    if (record) record.chunks.push({ wall: Date.now(), bytes: event.encodedDataLength });
  });
  cdp.on('Network.loadingFinished', (event) => {
    const record = byId.get(event.requestId);
    if (record) {
      record.endWall = Date.now();
      record.outcome = 'finished';
    }
  });
  cdp.on('Network.loadingFailed', (event) => {
    const record = byId.get(event.requestId);
    if (record) {
      record.endWall = Date.now();
      record.outcome = event.canceled ? 'aborted' : 'failed';
    }
  });
  return list;
}

// The wire bytes of a request's chunks that arrived after `from` and at or before `to`.
function bytesIn(record, from = -Infinity, to = Infinity) {
  return record.chunks.filter((chunk) => chunk.wall > from && chunk.wall <= to).reduce((sum, chunk) => sum + chunk.bytes, 0);
}

// Waits until every clear request sent after `mark` has ended. The page's clear sends its requests in the click
// handler, and their events reach the runner a moment later, so the wait starts after a short settle.
async function waitForClearRequests(page, net, mark, timeoutMs = 120000) {
  await page.waitForTimeout(100);
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const pending = net.list.slice(mark).filter((r) => CLEAR_CATEGORIES.has(r.category) && r.outcome === 'pending');
    if (pending.length === 0) return;
    if (Date.now() > deadline) {
      throw new Error(`the clear's requests did not end within ${timeoutMs} ms: ${pending.map((r) => r.url).join(', ')}`);
    }
    await page.waitForTimeout(50);
  }
}

// Opens the server's page in a throttled, read-only browser. `sessionId` opens that session; without it the page opens
// the server's first row, as a browser does.
async function openPage(baseUrl, { accessKey = '', sessionId = '' } = {}) {
  const base = new URL(baseUrl);
  const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
  const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || undefined, headless: true });
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  if (accessKey) {
    await context.addCookies([{
      name: 'charliebot_access_key',
      value: accessKey,
      domain: base.hostname,
      path: '/',
      secure: base.protocol === 'https:',
    }]);
  }
  const page = await context.newPage();
  const blocked = [];
  await page.route('**/*', (route) => {
    const request = route.request();
    if (request.method() !== 'GET') {
      blocked.push(`${request.method()} ${new URL(request.url()).pathname}`);
      return route.abort();
    }
    return route.continue();
  });
  const frames = [];
  let wsConnectedCount = 0;
  let wsErrorCount = 0;
  page.on('websocket', (ws) => {
    wsConnectedCount += 1;
    ws.on('framereceived', (frame) => {
      let type = 'binary';
      try { type = JSON.parse(frame.payload).type || 'untyped'; } catch (error) { type = 'unparsed'; }
      frames.push({ t: Date.now(), type, bytes: Buffer.byteLength(frame.payload) });
    });
    ws.on('socketerror', () => { wsErrorCount += 1; });
  });
  const searchResponses = [];
  page.on('response', (response) => {
    if (response.url().includes('/api/sessions/search')) {
      searchResponses.push({ t: Date.now(), status: response.status(), url: response.url() });
    }
  });
  await page.addInitScript((key) => {
    if (key) localStorage.setItem('charliebot_access_key', key);
    // The page's diagnostics clear the resource timing buffer on every traffic flush, so the runner reads each entry as
    // the observer receives it. The observer's list keeps every entry the page loads.
    window.__resources = [];
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) window.__resources.push(entry);
    }).observe({ type: 'resource', buffered: true });
    window.__inputs = [];
    document.addEventListener('input', (event) => {
      if (event.target && event.target.id === 'sidebar-search') {
        window.__inputs.push({ t: performance.now(), wall: Date.now(), v: event.target.value });
      }
    }, true);
  }, accessKey);

  const started = Date.now();
  const path = sessionId ? `/?session=${encodeURIComponent(sessionId)}` : '/';
  await page.goto(new URL(path, base).toString(), { waitUntil: 'load', timeout: 180000 });
  await page.waitForSelector('#sidebar-search');
  await page.waitForTimeout(4000);
  await page.evaluate(() => {
    window.__renders = [];
    const original = window.renderSessionList;
    window.renderSessionList = function (sessions, filter) {
      // A list paint that disagrees with the search box: a search paint into an empty box, or any other paint into a
      // non-empty one. This observes the paints the page makes through the global, whatever the build.
      const box = document.getElementById('sidebar-search');
      const boxText = !!box && box.value.trim() !== '';
      const record = { filter, rows: sessions.length, stale: (filter === 'search') !== boxText, t0: performance.now(),
        wall: Date.now() };
      const out = original.apply(this, arguments);
      record.t1 = performance.now();
      window.__renders.push(record);
      requestAnimationFrame(() => setTimeout(() => { record.paint = performance.now(); }, 0));
      return out;
    };
  });
  const cdp = await context.newCDPSession(page);
  await cdp.send('Network.enable');
  const net = { list: trackNetwork(cdp) };
  return {
    browser,
    page,
    cdp,
    net,
    blocked,
    frames,
    searchResponses,
    started,
    pageLoadMs: Date.now() - started,
    wsCounts: () => ({ connected: wsConnectedCount, errors: wsErrorCount }),
  };
}

// The search counters the page keeps: requests sent, responses discarded as superseded, list paints, and the stale
// paints the page counts itself (absent from a build that predates the count).
function readCounters(page) {
  return page.evaluate(() => ({ ...window.sidebarSearchCounters }));
}

function counterDelta(before, after) {
  return {
    fetch: after.fetch - before.fetch,
    render: after.render - before.render,
    discard: after.discard - before.discard,
    stale_paints: after.stale_paints === undefined ? null : after.stale_paints - (before.stale_paints ?? 0),
  };
}

// The bytes that the search's windows read back: each status poll sent inside the window, and each frame received inside
// it, with the link time those bytes take at the plan's rate and their share of the e2e time.
function readbackOf(session, inputWall, paintWall, e2eMs) {
  const inWindow = (t) => t > inputWall && t <= paintWall;
  const share = (bytes) => {
    const linkMs = (bytes / SLOW.downloadThroughput) * 1000;
    return { bytes, link_ms: linkMs, share: linkMs / e2eMs };
  };
  const statusRequests = session.net.list.filter((r) => r.category === 'status' && inWindow(r.sentWall));
  const statusBytes = session.net.list.filter((r) => r.category === 'status')
    .reduce((sum, r) => sum + bytesIn(r, inputWall, paintWall), 0);
  const frameRecords = session.frames.filter((f) => inWindow(f.t));
  const byType = {};
  for (const frame of frameRecords) {
    byType[frame.type] = byType[frame.type] || { count: 0, bytes: 0 };
    byType[frame.type].count += 1;
    byType[frame.type].bytes += frame.bytes;
  }
  const frameBytes = frameRecords.reduce((sum, f) => sum + f.bytes, 0);
  return {
    status_polls: { count: statusRequests.length, ...share(statusBytes) },
    websocket_frames: { count: frameRecords.length, by_type: byType, ...share(frameBytes) },
  };
}

// The competing traffic classes that sent or received inside the search's window.
function competingOf(session, inputWall, paintWall) {
  const inWindow = (t) => t > inputWall && t <= paintWall;
  const sent = session.net.list.filter((r) => inWindow(r.sentWall));
  const count = (category) => sent.filter((r) => r.category === category).length;
  const classes = {
    status: count('status'),
    transcript: count('transcript'),
    usage: count('usage'),
    concurrency: count('concurrency'),
    websocket_frames: session.frames.filter((f) => inWindow(f.t)).length,
    second_search: Math.max(0, count('search') - 1),
  };
  const active = Object.keys(classes).filter((name) => classes[name] > 0);
  return { classes, active };
}

// One search from the reader's keystroke to the paint of its rows, with the request's Resource Timing and the traffic the
// search's window carries. `scenario` is 'after-clear' (the input follows the clear's requests, or the fixed 2,500 ms
// wait when WAIT_FOR_CLEAR is 0) or 'clear-then-retype' (the input comes 300 ms after the clear). The e2e time is the
// paint minus the input event, and encodedBodySize is the body as it crossed the link.
async function oneSearch(session, query, scenario = 'after-clear') {
  const { page, net } = session;
  const mark = net.list.length;
  const clickWall = Date.now();
  await page.evaluate(() => clearSidebarSearch());
  if (scenario === 'clear-then-retype') {
    await page.waitForTimeout(300);
  } else if (waitForClearFromEnvironment()) {
    await waitForClearRequests(page, net, mark);
  } else {
    await page.waitForTimeout(2500);
  }
  const before = await page.evaluate(() => ({ n: window.__inputs.length, r: window.__renders.length }));
  const countersBefore = await readCounters(page);
  await page.fill('#sidebar-search', query);
  const run = await page.waitForFunction(({ before, query }) => {
    const input = window.__inputs[before.n];
    if (!input) return null;
    const render = window.__renders.slice(before.r).find(r => r.filter === 'search' && r.t0 > input.t && r.paint);
    if (!render) return null;
    const entry = window.__resources.find(e =>
      e.name.includes('/api/sessions/search?q=' + encodeURIComponent(query)) && e.startTime > input.t
      && e.responseEnd <= render.t0);
    if (!entry) return null;
    return {
      q: query,
      input_wall: input.wall,
      input_t: input.t,
      fetchStart: entry.fetchStart,
      requestStart: entry.requestStart,
      responseStart: entry.responseStart,
      responseEnd: entry.responseEnd,
      transferSize: entry.transferSize,
      encodedBodySize: entry.encodedBodySize,
      decodedBodySize: entry.decodedBodySize,
      render_t0: render.t0,
      render_t1: render.t1,
      paint: render.paint,
      rows: render.rows,
      overlap: window.__resources.filter(e => e !== entry && e.startTime < entry.responseEnd
        && e.responseEnd > entry.responseStart).map(e => ({
        path: new URL(e.name).pathname,
        transferSize: e.transferSize,
        start: e.startTime - entry.responseStart,
        end: e.responseEnd - entry.responseStart,
      })),
    };
  }, { before, query: query }, { timeout: 180000, polling: 50 });
  const r = await run.jsonValue();
  const inputWall = r.input_wall;
  const paintWall = inputWall + (r.paint - r.input_t);
  await page.waitForTimeout(SETTLE_MS);
  const countersAfter = await readCounters(page);
  const observedStale = await page.evaluate((t) => window.__renders.filter(x => x.stale && x.t0 > t).length, r.input_t);
  r.debounce_ms = r.fetchStart - r.input_t;
  r.queue_ms = r.requestStart - r.fetchStart;
  r.wait_ms = r.responseStart - r.requestStart;
  r.transfer_ms = r.responseEnd - r.responseStart;
  r.parse_facts_ms = r.render_t0 - r.responseEnd;
  r.render_ms = r.render_t1 - r.render_t0;
  r.paint_ms = r.paint - r.render_t1;
  r.e2e_ms = r.paint - r.input_t;
  r.scenario = scenario;
  r.counters = counterDelta(countersBefore, countersAfter);
  r.stale_paints_observed = observedStale;

  const clearRequests = net.list.slice(mark)
    .filter((q) => CLEAR_CATEGORIES.has(q.category) && q.sentWall <= inputWall);
  const lastClearEnd = Math.max(clickWall, ...clearRequests.map((q) => q.endWall ?? clickWall));
  const L = clearRequests.reduce((sum, q) => sum + bytesIn(q, inputWall), 0);
  // The wait runs from the clear's last request end when the input follows the requests; the fixed and the 300 ms
  // waits run from the click.
  const waitPolicy = scenario === 'clear-then-retype' ? 'retype-300ms'
    : (waitForClearFromEnvironment() ? 'clear-requests' : 'fixed-2500ms');
  r.clear = {
    wait_policy: waitPolicy,
    wait_ms: inputWall - (waitPolicy === 'clear-requests' ? lastClearEnd : clickWall),
    requests: clearRequests.map((q) => ({
      category: q.category,
      path: new URL(q.url).pathname,
      encoded_bytes: q.chunks.reduce((sum, c) => sum + c.bytes, 0),
      outcome: q.outcome,
      bytes_after_input: bytesIn(q, inputWall),
    })),
  };
  r.L_bytes = L;
  r.readback = readbackOf(session, inputWall, paintWall, r.e2e_ms);
  const competing = competingOf(session, inputWall, paintWall);
  r.competing = competing.classes;
  r.competing_active = competing.active;
  r.T_s = limitFor(query);
  r.within_T = r.T_s === null ? null : r.e2e_ms <= r.T_s * 1000;
  return r;
}

// The mechanical checks of a run, over every search: each search's e2e within its query's limit (where the plan sets
// one), no byte of a clear's request after its input (L), and no stale paint counted by the page. A check the build
// cannot make (no stale count) fails rather than passing.
function gatesOf(runs) {
  const failures = [];
  for (const run of runs) {
    const where = { query: run.q, rep: run.rep, scenario: run.scenario };
    if (run.within_T === false) {
      failures.push({ gate: 'e2e_within_T', ...where, e2e_ms: run.e2e_ms, T_ms: run.T_s * 1000 });
    }
    if (run.L_bytes !== 0) failures.push({ gate: 'L_zero', ...where, L_bytes: run.L_bytes });
    if (run.counters.stale_paints !== 0) {
      failures.push({ gate: 'stale_paints_zero', ...where, stale_paints: run.counters.stale_paints,
        observed: run.stale_paints_observed });
    }
  }
  return { ok: failures.length === 0, failures };
}

// Per query: the searches, the limit, the e2e range and the largest share of the readback, so a table reads without the
// runs. The whole run's searches count in `searches`.
function summarize(runs) {
  const byQuery = {};
  for (const run of runs) {
    const entry = byQuery[run.q] || (byQuery[run.q] = { searches: 0, T_s: run.T_s, e2e_ms: [], over_T: 0,
      max_L_bytes: 0, stale_paints: 0, max_status_share: 0, max_frames_share: 0 });
    entry.searches += 1;
    entry.e2e_ms.push(run.e2e_ms);
    if (run.within_T === false) entry.over_T += 1;
    entry.max_L_bytes = Math.max(entry.max_L_bytes, run.L_bytes);
    // A build without the stale count reports null, which the summary keeps rather than reading as zero.
    if (run.counters.stale_paints === null || entry.stale_paints === null) entry.stale_paints = null;
    else entry.stale_paints += run.counters.stale_paints;
    entry.max_status_share = Math.max(entry.max_status_share, run.readback.status_polls.share);
    entry.max_frames_share = Math.max(entry.max_frames_share, run.readback.websocket_frames.share);
  }
  for (const entry of Object.values(byQuery)) {
    entry.e2e_ms = { min: Math.min(...entry.e2e_ms), max: Math.max(...entry.e2e_ms) };
  }
  return { searches: runs.length, per_query: byQuery };
}

// One line per search for the console: what the search took, its limit, the byte and stale checks and its readback.
function describeRun(run) {
  const pct = (share) => (share * 100).toFixed(1) + '%';
  const limit = run.T_s === null ? '-' : Math.round(run.T_s * 1000) + ' ms';
  return [run.scenario, run.q, 'rep ' + run.rep, 'e2e ' + Math.round(run.e2e_ms) + ' ms', 'T ' + limit,
    'within_T ' + run.within_T, 'L ' + run.L_bytes + ' B',
    'stale ' + run.counters.stale_paints + ' (observed ' + run.stale_paints_observed + ')',
    'status ' + run.readback.status_polls.count + ' polls ' + pct(run.readback.status_polls.share),
    'frames ' + run.readback.websocket_frames.count + ' ' + pct(run.readback.websocket_frames.share),
    'competing [' + run.competing_active.join(',') + ']', 'rows ' + run.rows].join('  ');
}

// Runs the searches of one scenario over the queries and writes the run record to `out`. Each query runs once untimed
// first when WARM_UP is 1. The record is written before the gates decide the exit code: a gate that fails exits 3.
async function runScenario({ baseUrl, out, reps, queries, scenario }) {
  const warmUp = warmUpFromEnvironment();
  const session = await openPage(baseUrl, { accessKey: accessKeyFromEnvironment() });
  const result = {
    phase: 'slow',
    scenario,
    base_url: baseUrl,
    reps,
    queries,
    warm_up: warmUp,
    wait_for_clear: waitForClearFromEnvironment(),
    limits_T_s: LIMIT_T_S,
    link: SLOW,
    started: new Date(session.started).toISOString(),
    page_load_ms: session.pageLoadMs,
    runs: [],
  };
  try {
    await session.cdp.send('Network.emulateNetworkConditions', SLOW);
    if (warmUp) {
      for (const query of queries) await oneSearch(session, query, scenario);
    }
    for (let rep = 0; rep < reps; rep += 1) {
      for (const query of queries) {
        const run = await oneSearch(session, query, scenario);
        run.rep = rep;
        result.runs.push(run);
        console.log(describeRun(run));
      }
    }
  } finally {
    result.blocked = session.blocked;
    result.ws = session.wsCounts();
    result.page_frames_total = session.frames.length;
    result.summary = summarize(result.runs);
    result.gates = gatesOf(result.runs);
    fs.writeFileSync(out, JSON.stringify(result, null, 1));
    await session.browser.close();
  }
  if (!result.gates.ok) {
    console.error(`${result.gates.failures.length} gate check(s) failed; see ${out}`);
    process.exitCode = 3;
  }
  return result;
}

// The id and name of the first row of the answer at `path`. The id column holds the 16-byte UUIDs in base64url, or one
// cell per row when an id is not a canonical UUID; a cell that starts with `~` is the id itself.
async function firstRow(baseUrl, accessKey, path) {
  const response = await fetch(new URL(path, baseUrl), {
    headers: accessKey ? { authorization: `Bearer ${accessKey}` } : {},
  });
  if (!response.ok) throw new Error(`the first row lookup of ${path} failed: ${response.status}`);
  const columns = await response.json();
  const cell = Array.isArray(columns.id) ? columns.id[0] : columns.id.slice(0, 22);
  const name = Array.isArray(columns.name) ? columns.name[0] : null;
  if (typeof name !== 'string') throw new Error(`the answer of ${path} has no name for its first row`);
  if (cell.startsWith('~')) return { id: cell.slice(1), name };
  const hex = Buffer.from(cell, 'base64url').toString('hex');
  const id = [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20, 32)].join('-');
  return { id, name };
}

// Renames one session `changes` times, at k * windowMs / changes for k = 1..changes: each rename is a task-tree change
// the server pushes to every open tab. Each rename keeps `baseName` and adds a suffix, so the row still matches the
// query that the page is showing. Returns {stop}; stop waits for the last rename in flight and resolves to the count
// sent.
function startTaskTreeLoad({ baseUrl, accessKey, sessionId, baseName, changes, windowMs }) {
  const intervalMs = windowMs / changes;
  const started = performance.now();
  const headers = { 'content-type': 'application/json' };
  if (accessKey) headers.authorization = `Bearer ${accessKey}`;
  const url = new URL(`/api/sessions/${sessionId}`, baseUrl);
  let count = 0;
  let stopped = false;
  let inFlight = Promise.resolve();
  let timer = null;
  const tick = () => {
    if (stopped) return;
    count += 1;
    const name = `${baseName} load ${count}`;
    inFlight = fetch(url, { method: 'PATCH', headers, body: JSON.stringify({ name }) }).then((response) => {
      if (!response.ok) throw new Error(`the rename failed: ${response.status}`);
    });
    if (count < changes) {
      const due = started + (count + 1) * intervalMs;
      timer = setTimeout(tick, Math.max(0, due - performance.now()));
    }
  };
  timer = setTimeout(tick, intervalMs);
  return {
    async stop() {
      stopped = true;
      clearTimeout(timer);
      await inFlight;
      return count;
    },
  };
}

module.exports = {
  SLOW,
  accessKeyFromEnvironment,
  warmUpFromEnvironment,
  waitForClearFromEnvironment,
  openPage,
  oneSearch,
  gatesOf,
  summarize,
  describeRun,
  runScenario,
  readCounters,
  counterDelta,
  firstRow,
  startTaskTreeLoad,
};
