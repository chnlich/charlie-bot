# Improve direction: performance

You run the performance direction of the improve loop.
Follow `dev/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/perf/`.

## Scope

- the server code in `rust/`
- the pages in `web/`

## Topics

Topics rank by the user's wait in seconds only: a page by its slow-network load seconds, and a GET route by its slow-network transfer seconds.
Byte-class and ratio-class scenarios, the `TARGETS` of `dev/poor-network/profile.mjs`, do not rank.
The poor-network bench still decides whether a round whose change can move such a scenario takes that scenario as its topic.

## Poor-network scenarios

A round that takes a scenario as its topic runs it before and after its change, on host3. The bench takes the host's bench lock for each of its sides, so no other bench run on the host measures beside it.
The round's bench run waits for that lock at most 15 minutes, the bench's `--lock-wait 900`. When the wait runs out, the round ends with the stop reason of the Serving rules below.
The bench's entry is `dev/poor-network/run.sh`: it runs a named scenario set on both sides and prints the bench's verdict for each scenario.
The target of a scenario is in `TARGETS` of `dev/poor-network/profile.mjs`, and the bench's verdict decides; this prompt copies no target value.
Run only the scenarios the change can move, with the background scenario at 3 repetitions, so the check stays inside the round's time.
A round that turns a scenario the bench passed on the before side into a failing one on the after side does not land.

## Mock home

The runner supplies the complete shared mock home before the round starts.
The mock home has the production number of sessions, so the numbers show the load that a user meets.
Its directory is `~/improve/mock-home/<version>/`, where `<version>` is the output of `mock-home version`.
Read that directory throughout the round.
The round only reads the shared mock home.
The fixed data keeps the before and after servers on the same bytes, so the two sides compare like with like.

## Binaries

Build each server on the round's host, from source with `cargo build --release --locked -p charliebot-server -p charliebot-cli` in `rust/`.
Build the before server from an export of `origin/main`, and the after server from the branch.
Both sides use one release profile on one machine, so the change under test is the only difference between them.

For a change to web files only, build one server from `origin/main` and start it twice, once per tree.
The server binary then stays equal, so the web files are the only difference.

When the round adds or changes Rust unit tests, run the unit-test-time check with `--alone-limit 0.08` and
`--parallel-limit 0.08` for every crate whose unit tests it changed before opening the pull request.
The parallel flag makes the check rerun alone each test over 0.08 s in the parallel run, so the margin judges it;
with the default parallel limit, a test between 0.08 s and 0.1 s is never rerun alone.
Keep each new or changed test's worst alone time below 0.08 s.
Put each test's worst alone time in the pull request body.
Shrink or move any test over 0.08 s before opening the pull request.

## Serving

Each side is one serve trial of its commit on its server binary, with the mock home as its seed.
The trial runs as `scripts/release/trial.sh --serve`, and the round runs it through `dev/improve/trial-side.sh`, which holds the host's bench lock from the trial's start to its stop.
The tree root is under `/tmp/improve-perf/`, which keeps the two trees apart, and the seed gives both servers the same data.
The round creates the tree root before its first side, so no side writes outside the round's own directory.
The caller's scope already caps memory on the round's host, so the trial adds no scope of its own.
Builds finish before a side starts, so they stay outside the lock.
A side waits at most 15 minutes for the bench lock.
When the wait runs out, the round takes no measurement, opens no pull request, and ends with a stop reason that names the bench lock as held.
The next scheduled fire starts the round again.

Run the mock home check against the trial's server first in the side's command.
Stop the round when the check exits 1, because that server reads data other than the mock home.
The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to `<command>`, so every request and every `/proc` read names this trial's server and no other.

Measure the two sides back to back in one round, in the same order.
Both sides then see the same host load, so the change is the only difference.

## Measures

One latency rule serves every metric: keep the median of the kept requests.
A warm target is pre-warmed, so its warm-up requests are not kept; a cold target's samples are its first requests, and each one counts.

Measure latency this way: send 5 warm-up requests, then 50 requests, and keep the median of the 50.
The warm-up keeps first-request cost out of the sample, and the median keeps one slow request out.

Measure the sidebar search route, `GET /api/sessions/search`, with the words of `SEARCH_WORDS`, the fixed word list of the mock home: one common word, one word that hits 200 rows, and one rare word.
Each word is one query, measured with the latency rule above.
The broadcast window of the search route is kept apart from the other scenarios: 19 events over 60.004 s (source `SS/watch_ws.json`).
The shared constant `TASK_TREE_LOAD` (19 events over 75 s, `dev/poor-network/profile.mjs:38–41`) stays unchanged.

Measure the cold query this way: restart the server before each cold sample, so every content entry is cleared, rather than change a file's signature, which would rewrite mock-home files and move the other samples.
Take 10 cold samples per round and keep their median; a cold sample is a query word that this server has not answered in this round.

Measure slow-network page loading this way: send every load of this measure through the bench's shaping proxy, which holds the bench's link profile, so the loads and the poor-network bench measure one link model.
The link's values live only in `LINK` of `dev/poor-network/profile.mjs`; this measure names none of them and takes them from that file.
The proxy's entry is `dev/poor-network/relay.mjs`, which carries the trial server's tunnels; the round stops it before its trial ends, and when the host cannot run that entry, name the defect instead of fitting another relay in.

A load fetches the page's document and then the GET API responses the page calls and the static files the served document references, in document order, accepting gzip as the latency measure does.
A cold load opens one connection per fetch, and a warm load fetches the whole page on one connection.
Cold is what a first visit pays per fetch, and warm is what a kept-alive connection saves.
The load's time runs from the document request's start to the last response's last byte, so it is the wait until the page is usable in this model.
The load's bytes are the sum of all response bodies, summed at the client, so they count what crossed the link.

The pages are the chat page with the mock home's largest session id open, the events viewer of that session, and the home page.
Send 10 cold loads per page first, then 5 warm-up loads and 10 warm loads, and keep the median of the 10 cold loads and of the 10 warm loads.

Measure startup this way: after the trial exits, read the first line of `server-1.log` in its evidence directory, and the first `http_request` line with `"status":200`; subtract the first timestamp from the second.
The first line is the server's own first log line, written just after the process starts, so the difference is the run's startup time rather than a wall clock the measuring shell reads.
Repeat five runs and keep the median, so one slow start does not set the number.

Measure memory this way: after a fixed 200 requests, read `VmHWM` from `/proc/$CHARLIEBOT_SERVE_PID/status`.
The fixed request count makes the two runs comparable.

Measure CPU this way: read `utime` and `stime` from `/proc/$CHARLIEBOT_SERVE_PID/stat` before and after the same 200 requests.
Sum `utime` and `stime` in each read, subtract the before sum from the after sum, and divide by `getconf CLK_TCK`.
The fixed request count and the unit conversion make the two runs comparable.

## Evidence

Write one line per metric in the pull request's `## Evidence` section.
Each line gives the metric, the before value, the after value, the sample count and the change.
A reviewer checks the numbers from the line alone, without a rerun.

Write that the absolute values differ from production, because the host and the content differ.
Compare the before and after values only within one round, so that no reader sets them against production numbers.

The raw measurement logs stay in the round's evidence directory from the landing protocol; Step 9 of the landing protocol carries them to the user.
