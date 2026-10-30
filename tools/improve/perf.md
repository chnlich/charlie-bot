# Improve direction: performance

You run the performance direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/perf/`.

## Scope

- the server code in `rust/`
- the pages in `web/`

## Topics

Measure each GET route that the pages call, on the mock home described below.
Take the slowest route by median latency as the topic, or take startup, memory or CPU.
The slowest route is the one a user waits on, so cutting it is the change a user feels.
The mock home has the production number of sessions, so the numbers show the load that a user meets.

## Mock home

The mock home directory is `~/improve/mock-home/<version>/`, where `<version>` is the output of `mock-home version`.
When that directory is missing, create it with `mock-home generate`.
The fixed data keeps the before and after servers on the same bytes, so the two sides compare like with like.

## Binaries

Build each server on the round's host, from source with `cargo build --release --locked -p charliebot-server -p charliebot-cli` in `rust/`.
Build the before server from an export of `origin/main`, and the after server from the branch.
Both sides use one release profile on one machine, so the change under test is the only difference between them.

For a change to web files only, build one server from `origin/main` and start it twice, once per tree.
The server binary then stays equal, so the web files are the only difference.

## Serving

Start each side with `scripts/release/trial.sh --serve --tree-root /tmp/improve-perf/ --seed-home <mock home directory> --in-caller-scope <commit> <server binary> -- <command>`.
The tree root keeps the two trees apart, and the seed gives both servers the same data.
Create the tree root first: the preflight refuses a missing one, so a round never writes outside its own directory.
The caller's scope already caps memory on the round's host, so the trial adds no scope of its own.

Run `mock-home check <mock home directory> "$CHARLIEBOT_SERVE_URL"` first in `<command>`.
Stop the round when it exits 1, because that server reads data other than the mock home.
The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to `<command>`, so every request and every `/proc` read names this trial's server and no other.

Measure the two sides back to back in one round, in the same order.
Both sides then see the same host load, so the change is the only difference.

## Measures

Measure latency this way: send 5 warm-up requests, then 50 requests, and keep the median of the 50.
The warm-up keeps first-request cost out of the sample, and the median keeps one slow request out.

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

The raw measurement logs stay in the round's evidence directory from the landing protocol; their links go only into the final message.
