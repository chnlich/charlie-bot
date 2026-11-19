// Sidebar search, timed in headless Chrome over the search plan's link (300 ms round trip, 12,500 B/s each way). Each
// query runs `reps` times. Each repeat clears the search box, types the query, and records the e2e time (the paint of
// the rows minus the input event), the search body's encodedBodySize, and the page's search counters.
//
// Usage: node browser_search.js <base-url> <out.json> <reps> <query>...
//
// The base URL is the server to measure. Before the timed repeats, each query runs once untimed (WARM_UP, default 1).
// WARM_UP=0 times the first search after the server starts: a cold sample, run with reps 1 and one query.
'use strict';

const fs = require('fs');
const {
  SLOW,
  accessKeyFromEnvironment,
  warmUpFromEnvironment,
  openPage,
  oneSearch,
} = require('./measure_common');

async function main() {
  const [baseUrl, out, repsArg, ...queries] = process.argv.slice(2);
  if (!baseUrl || !out || !repsArg || queries.length === 0) {
    console.error('usage: node browser_search.js <base-url> <out.json> <reps> <query>...');
    process.exit(2);
  }
  const reps = Number(repsArg);
  const warmUp = warmUpFromEnvironment();
  const page = await openPage(baseUrl, { accessKey: accessKeyFromEnvironment() });
  const result = {
    phase: 'slow',
    base_url: baseUrl,
    reps,
    queries,
    warm_up: warmUp,
    link: SLOW,
    started: new Date(page.started).toISOString(),
    page_load_ms: page.pageLoadMs,
    runs: [],
  };
  try {
    await page.cdp.send('Network.emulateNetworkConditions', SLOW);
    if (warmUp) {
      for (const query of queries) await oneSearch(page.page, query);
    }
    for (let rep = 0; rep < reps; rep += 1) {
      for (const query of queries) {
        const run = await oneSearch(page.page, query);
        run.rep = rep;
        result.runs.push(run);
        console.log('slow', query, rep, 'e2e_ms', Math.round(run.e2e_ms), 'encodedBodySize', run.encodedBodySize,
          'fetch', run.counters.fetch, 'render', run.counters.render, 'discard', run.counters.discard,
          'rows', run.rows);
      }
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
