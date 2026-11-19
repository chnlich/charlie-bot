// Sidebar search after a clear and a retype, timed in headless Chrome over the search plan's link (300 ms round trip,
// 12,500 B/s each way). Scenario 'clear-then-retype': each query runs `reps` times. Each repeat clears the search box,
// waits 300 ms, types the query, and records the same fields as browser_search.js. Its L and stale paints are the abort
// test: a search start must stop the clear's requests, so no clear byte arrives after the input and no stale paint is
// counted.
//
// Usage: node browser_clear_retype.js <base-url> <out.json> <reps> <query>...
//
// The base URL is the server to measure. WARM_UP and the output are as in browser_search.js. Exit code 3 when a gate
// fails; the record is written first.
'use strict';

const { runScenario } = require('./measure_common');

async function main() {
  const [baseUrl, out, repsArg, ...queries] = process.argv.slice(2);
  if (!baseUrl || !out || !repsArg || queries.length === 0) {
    console.error('usage: node browser_clear_retype.js <base-url> <out.json> <reps> <query>...');
    process.exit(2);
  }
  await runScenario({ baseUrl, out, reps: Number(repsArg), queries, scenario: 'clear-then-retype' });
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
