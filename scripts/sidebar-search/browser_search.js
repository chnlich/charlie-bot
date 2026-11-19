// Sidebar search, timed in headless Chrome over the search plan's link (300 ms round trip, 12,500 B/s each way). Scenario
// 'after-clear': each query runs `reps` times. Each repeat clears the search box, waits for the clear's requests (or for
// the fixed 2,500 ms when WAIT_FOR_CLEAR is 0), types the query, and records the search: e2e (the paint of the rows minus
// the input event), the limit T of the query, L (the clear's bytes that arrive after the input), the stale paints, the
// readback of status polls and frames in the search's window, and the competing traffic that was active.
//
// Usage: node browser_search.js <base-url> <out.json> <reps> <query>...
//
// The base URL is the server to measure. Before the timed repeats, each query runs once untimed (WARM_UP, default 1).
// WARM_UP=0 times the first search after the server starts: a cold sample, run with reps 1 and one query.
// Exit code 3 when a gate fails; the record is written first.
'use strict';

const { runScenario } = require('./measure_common');

async function main() {
  const [baseUrl, out, repsArg, ...queries] = process.argv.slice(2);
  if (!baseUrl || !out || !repsArg || queries.length === 0) {
    console.error('usage: node browser_search.js <base-url> <out.json> <reps> <query>...');
    process.exit(2);
  }
  await runScenario({ baseUrl, out, reps: Number(repsArg), queries, scenario: 'after-clear' });
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
