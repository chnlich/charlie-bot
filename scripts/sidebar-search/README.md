# Sidebar search runner

Times the sidebar search in headless Chrome against one CharlieBot server, over the search plan's link: a 300 ms round
trip and 12,500 B/s each way, set with CDP network emulation. The page cannot write to the server, since every non-GET
request it makes is aborted.

Both entry points take the server's base URL as their first argument. Nothing in this directory names a host, a path
or a credential.

| Script | What it measures |
|---|---|
| `browser_search.js` | Each query, `reps` times: e2e seconds, `encodedBodySize`, and search counters. |
| `browser_measure_slowwatch.js` | The first answer, then a broadcast window with its search counters. |

```sh
node browser_search.js <base-url> out/slow.json 3 sidebar plan cron hydeogen
node browser_measure_slowwatch.js <base-url> out/slowwatch.json 3 60.004 19 sidebar
```

Each output file carries one entry per repeat under `runs`. `e2e_ms` is the paint minus the input event, and the
`counters` of a run are the deltas of `window.sidebarSearchCounters` over that search.

## Environment

- `CHARLIEBOT_ACCESS_KEY`: the server's access key, when the server asks for one. It is sent as the access-key cookie,
  the local storage key, the bearer header and the rename requests. The output files never record it.
- `PLAYWRIGHT_MODULE`: the Playwright module to load. Default: `playwright`, resolved from this directory.
- `CHROME_BIN`: the Chrome binary to launch. Default: the binary Playwright bundles.

## Broadcast window

`browser_measure_slowwatch.js` makes its broadcasts with renames of one session, which the server pushes to every open
tab as a task-tree change. The session is the first row of the query's answer, and each rename keeps that row's name
and adds a suffix, so the row keeps answering the query and each change repaints the list. A window of 60.004 s with
19 changes sends the changes at k × 60.004 / 19 seconds for k = 1..19.
