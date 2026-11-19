// Sidebar search under a broadcast window, in headless Chrome over the search plan's link. Each repeat clears the
// search box and times the query's first answer (e2e and encodedBodySize, as browser_search.js does). Then the server
// gets `changes` task-tree changes spread evenly over `window-seconds`, while the search stays open, and the window
// records the page's search counters: requests sent, list paints, and responses discarded as superseded.
//
// The changes are session renames made from this process. Each rename is a task-tree change the server pushes to every
// open tab, so the page refreshes its search as it would under a live turn.
//
// Usage: node browser_measure_slowwatch.js <base-url> <out.json> <reps> <window-seconds> <changes> <query>
// The access key, when the server asks for one, comes from CHARLIEBOT_ACCESS_KEY.
'use strict';

const fs = require('fs');
const {
  SLOW,
  accessKeyFromEnvironment,
  openPage,
  oneSearch,
  readCounters,
  counterDelta,
  firstRow,
  startTaskTreeLoad,
} = require('./measure_common');

async function main() {
  const [baseUrl, out, repsArg, windowArg, changesArg, query] = process.argv.slice(2);
  if (!baseUrl || !out || !repsArg || !windowArg || !changesArg || !query) {
    console.error('usage: node browser_measure_slowwatch.js <base-url> <out.json> <reps> <window-seconds> <changes> '
      + '<query>');
    process.exit(2);
  }
  const reps = Number(repsArg);
  const windowSeconds = Number(windowArg);
  const changes = Number(changesArg);
  const accessKey = accessKeyFromEnvironment();
  // The renames go to the first row of the query's answer. A rename of a row outside the answer leaves the
  // answer's body unchanged, and the page then repaints nothing.
  const target = await firstRow(baseUrl, accessKey, `/api/sessions/search?q=${encodeURIComponent(query)}`);
  const page = await openPage(baseUrl, { accessKey });
  const result = {
    phase: 'slowwatch',
    base_url: baseUrl,
    reps,
    query,
    window_seconds: windowSeconds,
    changes,
    link: SLOW,
    started: new Date(page.started).toISOString(),
    page_load_ms: page.pageLoadMs,
    runs: [],
  };
  try {
    await page.cdp.send('Network.emulateNetworkConditions', SLOW);
    for (let rep = 0; rep < reps; rep += 1) {
      const first = await oneSearch(page.page, query);
      first.rep = rep;
      const countersBefore = await readCounters(page.page);
      const windowStart = Date.now();
      const windowStartPage = await page.page.evaluate(() => performance.now());
      const framesBefore = page.frames.length;
      const searchesBefore = page.searchResponses.length;
      const load = startTaskTreeLoad({
        baseUrl,
        accessKey,
        sessionId: target.id,
        baseName: target.name,
        changes,
        windowMs: windowSeconds * 1000,
      });
      await page.page.waitForTimeout(windowSeconds * 1000);
      const changesSent = await load.stop();
      const countersAfter = await readCounters(page.page);
      // The entries the observer collected since the window opened, on the page's clock.
      const searchEntries = await page.page.evaluate(({ query, since }) => window.__resources
        .filter(e => e.name.includes('/api/sessions/search?q=' + encodeURIComponent(query)) && e.startTime >= since)
        .map(e => ({
          start: performance.timeOrigin + e.startTime,
          duration: e.duration,
          transferSize: e.transferSize,
          encodedBodySize: e.encodedBodySize,
        })), { query, since: windowStartPage });
      const framesByType = {};
      for (const frame of page.frames.slice(framesBefore)) {
        framesByType[frame.type] = (framesByType[frame.type] || 0) + 1;
      }
      const run = {
        rep,
        q: query,
        first,
        window: {
          seconds: (Date.now() - windowStart) / 1000,
          changes_sent: changesSent,
          counters: counterDelta(countersBefore, countersAfter),
          search_responses: page.searchResponses.length - searchesBefore,
          search_entries: searchEntries,
          frames_by_type: framesByType,
        },
      };
      result.runs.push(run);
      console.log('slowwatch', query, rep, 'first_e2e_ms', Math.round(first.e2e_ms), 'encodedBodySize',
        first.encodedBodySize, 'window_fetch', run.window.counters.fetch, 'window_render', run.window.counters.render,
        'window_discard', run.window.counters.discard, 'changes_sent', changesSent);
    }
  } finally {
    result.blocked = page.blocked;
    result.ws = page.wsCounts();
    result.page_frames_total = page.frames.length;
    fs.writeFileSync(out, JSON.stringify(result, null, 1));
    await page.browser.close();
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
