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

'use strict';

const SLOW = { offline: false, latency: 300, downloadThroughput: 12500, uploadThroughput: 12500 };

function accessKeyFromEnvironment() {
  return process.env.CHARLIEBOT_ACCESS_KEY || '';
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
      frames.push({ t: Date.now(), type });
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
    performance.setResourceTimingBufferSize(100000);
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
      const record = { filter, rows: sessions.length, t0: performance.now(), wall: Date.now() };
      const out = original.apply(this, arguments);
      record.t1 = performance.now();
      window.__renders.push(record);
      requestAnimationFrame(() => setTimeout(() => { record.paint = performance.now(); }, 0));
      return out;
    };
  });
  const cdp = await context.newCDPSession(page);
  await cdp.send('Network.enable');
  return {
    browser,
    page,
    cdp,
    blocked,
    frames,
    searchResponses,
    started,
    pageLoadMs: Date.now() - started,
    wsCounts: () => ({ connected: wsConnectedCount, errors: wsErrorCount }),
  };
}

// The search counters the page keeps: requests sent, responses discarded as superseded, and list paints.
function readCounters(page) {
  return page.evaluate(() => ({ ...window.sidebarSearchCounters }));
}

function counterDelta(before, after) {
  return {
    fetch: after.fetch - before.fetch,
    render: after.render - before.render,
    discard: after.discard - before.discard,
  };
}

// One search from the reader's keystroke to the paint of its rows, with the request's Resource Timing. The e2e time is
// the paint minus the input event, and encodedBodySize is the body as it crossed the link.
async function oneSearch(page, query) {
  await page.evaluate(() => clearSidebarSearch());
  await page.waitForTimeout(2500);
  const before = await page.evaluate(() => ({ n: window.__inputs.length, r: window.__renders.length }));
  const countersBefore = await readCounters(page);
  await page.fill('#sidebar-search', query);
  const run = await page.waitForFunction(({ before, query }) => {
    const input = window.__inputs[before.n];
    if (!input) return null;
    const render = window.__renders.slice(before.r).find(r => r.filter === 'search' && r.t0 > input.t && r.paint);
    if (!render) return null;
    const entry = performance.getEntriesByType('resource').find(e =>
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
      overlap: performance.getEntriesByType('resource').filter(e => e !== entry && e.startTime < entry.responseEnd
        && e.responseEnd > entry.responseStart).map(e => ({
        path: new URL(e.name).pathname,
        transferSize: e.transferSize,
        start: e.startTime - entry.responseStart,
        end: e.responseEnd - entry.responseStart,
      })),
    };
  }, { before, query: query }, { timeout: 180000, polling: 50 });
  const r = await run.jsonValue();
  const countersAfter = await readCounters(page);
  r.debounce_ms = r.fetchStart - r.input_t;
  r.queue_ms = r.requestStart - r.fetchStart;
  r.wait_ms = r.responseStart - r.requestStart;
  r.transfer_ms = r.responseEnd - r.responseStart;
  r.parse_facts_ms = r.render_t0 - r.responseEnd;
  r.render_ms = r.render_t1 - r.render_t0;
  r.paint_ms = r.paint - r.render_t1;
  r.e2e_ms = r.paint - r.input_t;
  r.counters = counterDelta(countersBefore, countersAfter);
  return r;
}

// The session id of the server's first row, read from the Workspace move form: its id column is the 16-byte UUIDs in
// base64url, or one cell per row when an id is not a canonical UUID.
async function firstSessionId(baseUrl, accessKey) {
  const response = await fetch(new URL('/api/sessions/?compact=move', baseUrl), {
    headers: accessKey ? { authorization: `Bearer ${accessKey}` } : {},
  });
  if (!response.ok) throw new Error(`the first session lookup failed: ${response.status}`);
  const columns = await response.json();
  const cell = Array.isArray(columns.id) ? columns.id[0] : columns.id.slice(0, 22);
  if (cell.startsWith('~')) return cell.slice(1);
  const hex = Buffer.from(cell, 'base64url').toString('hex');
  return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20, 32)].join('-');
}

// Renames one session `changes` times, at k * windowMs / changes for k = 1..changes: each rename is a task-tree change
// the server pushes to every open tab. Returns {stop}; stop waits for the last rename in flight and resolves to the
// count sent.
function startTaskTreeLoad({ baseUrl, accessKey, sessionId, changes, windowMs }) {
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
    const name = `load rename ${count}`;
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
  openPage,
  oneSearch,
  readCounters,
  counterDelta,
  firstSessionId,
  startTaskTreeLoad,
};
