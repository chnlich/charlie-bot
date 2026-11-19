# Sidebar search runner

Times the sidebar search in headless Chrome against one CharlieBot server, over the search plan's link: a 300 ms round
trip and 12,500 B/s each way, set with CDP network emulation. The page cannot write to the server, since every non-GET
request it makes is aborted.

Each entry point takes the server's base URL as its first argument. Nothing in this directory names a host, a path or a
credential.

| Script | Scenario | What it measures |
|---|---|---|
| `browser_search.js` | `after-clear` | Each query, `reps` times: clear the box, wait for the clear's requests, type, and time the search. |
| `browser_clear_retype.js` | `clear-then-retype` | Each query, `reps` times: clear the box, wait 300 ms, type, and time the search. The abort test. |
| `browser_measure_slowwatch.js` | `after-clear` | The first answer, then a broadcast window with its search counters. |

```sh
node browser_search.js <base-url> out/after.json 3 sidebar plan cron hydeogen
node browser_clear_retype.js <base-url> out/retype.json 3 sidebar plan cron hydeogen
WAIT_FOR_CLEAR=0 node browser_search.js <base-url> out/before.json 3 sidebar plan cron hydeogen
WARM_UP=0 node browser_search.js <base-url> out/cold.json 1 hydeogen
node browser_measure_slowwatch.js <base-url> out/slowwatch.json 3 60.004 19 sidebar --confirm-renames
```

Before its timed repeats, each query runs once untimed, so the repeats measure a warm server. `WARM_UP=0` skips that
warm-up: the one timed search is then the first a freshly started server answers for its term, a cold sample.
`--confirm-renames` never targets the live server. Pass it only against an isolated verification server; the
post-release live run is observation only and runs `browser_search.js` without the flag.

## Scenarios and the clear's wait

`after-clear` clears the box and then waits. With `WAIT_FOR_CLEAR=1` (default) it waits until every request the clear
sent has ended: the Workspace listing, its row refresh and its cron badge. With `WAIT_FOR_CLEAR=0` it waits a fixed
2,500 ms, the wait the runs before the abort fix used. `clear-then-retype` types 300 ms after the clear, whatever
`WAIT_FOR_CLEAR` says.

## Output

Each output file carries one entry per repeat under `runs`, with `summary`, `gates` and the `limits_T_s` it was gated
against. A run's fields:

| Field | Meaning |
|---|---|
| `scenario`, `wait_for_clear`, `clear.wait_policy`, `clear.wait_ms` | The scenario, the wait's policy, and the time from the wait's start to the input. |
| `e2e_ms` | The paint minus the input event. |
| `T_s`, `within_T` | The query's limit from `limits.js` (null for a query the plan sets none), and whether `e2e_ms` is within it. |
| `L_bytes`, `clear.requests` | The clear's requests (category, encoded bytes, outcome: finished, aborted or failed) and their bytes that arrived after the input. L is their sum. |
| `counters.stale_paints` | The page's own count, over the search's window and a 1.5 s settle after the paint: a search paint into an empty box, or a non-search paint into a non-empty one. Null when the build has no count. |
| `stale_paints_observed` | The runner's count of the same condition on the paints it sees through the page's global `renderSessionList`. |
| `readback.status_polls` | The status polls sent in the window: `count`, the bytes that arrived in the window, the link time (bytes ÷ 12,500 B/s) and its share of `e2e_ms`. |
| `readback.websocket_frames` | The frames received in the window: `count`, `by_type`, the bytes, the link time and its share. |
| `competing`, `competing_active` | Requests sent in the window by class (status, transcript, usage, concurrency, the frames, a second search) and the classes with any. |

A chunk's bytes are the wire length the network reports for it (CDP `dataReceived.encodedDataLength`). For a body in one
encoding that is the chunk's decoded length times the response's encoded-to-decoded ratio, the segmented report's method.

`gates` lists each failed check: `e2e_within_T` (every search with a limit is within it), `L_zero` (no clear byte after
the input) and `stale_paints_zero` (the page's count is zero; a build without the count fails this check). The process
exits 3 when any check fails, after the record is written. A limit is never changed here: the table in `limits.js` is
the approved plan's.

## Environment

- `WARM_UP`: `1` (default) warms each query once before its timed repeats; `0` times the first search instead. Any
  other value stops the run.
- `WAIT_FOR_CLEAR`: `1` (default) makes `after-clear` wait for the clear's requests; `0` keeps the fixed 2,500 ms wait.
  Any other value stops the run.
- `CHARLIEBOT_ACCESS_KEY`: the server's access key, when the server asks for one. It is sent as the access-key cookie,
  the local storage key, the bearer header and the rename requests. The output files never record it.
- `PLAYWRIGHT_MODULE`: the Playwright module to load. Default: `playwright`, resolved from this directory.
- `CHROME_BIN`: the Chrome binary to launch. Default: the binary Playwright bundles.
- `CHROME_TMPDIR`: the TMPDIR Chrome runs with. Under `trial.sh` the TMPDIR is the trial's own tree, whose path is long, and
  Chrome's socket path must stay under 108 bytes; set this to a short directory. Default: the inherited TMPDIR.

## Broadcast window

`browser_measure_slowwatch.js` makes its broadcasts with renames of one session, which the server pushes to every open
tab as a task-tree change. The session is the first row of the query's answer, and each rename keeps that row's name
and adds a suffix, so the row keeps answering the query and each change repaints the list. It refuses to send rename
PATCH requests without the caller's `--confirm-renames` flag. A window of 60.004 s with 19 changes sends the changes at
k × 60.004 / 19 seconds for k = 1..19.
