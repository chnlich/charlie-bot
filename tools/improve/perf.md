# Improve direction: performance

You run the performance direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/perf/`.

## Scope

- the server code in `rust/`
- the pages in `web/`

## Topics

Read the `duration_ms` median of each `http_request` route in `/tmp/charliebot-logs/server-latest.log`.
Take the slowest route, or the startup time, or the memory, or the CPU as the topic.
The live log names the delay that the user feels.

## Measurement

Measure both sides with `scripts/release/trial.sh --serve <commit> <server binary> -- <command>`.
Before the change, use main's CI artifact `charliebot-server-<main sha>` on the main tree.
After the change, use the pull request's artifact `charliebot-server-pr-<head sha>` on the branch tree.
Serve each artifact at the commit in its name, so the binary and the tree match.
The command reads `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID`; the `/proc` reads use that pid.
Measure the two sides back to back in one round.
The host and the procedure are the same on both sides, so the change is the only difference.

Measure a latency topic this way:
send 5 warm-up requests, then 50 requests, and keep the median.
The warm-up keeps cold-start cost out of the sample.

Measure a startup topic this way:
measure the time from the process start to the first 200 response, 5 times, and keep the median.
Each run of `trial.sh --serve` starts the server once, so five runs make five starts.
The repetition keeps one slow start out of the report.

Measure a memory topic this way:
send a fixed 200 requests, then read `VmHWM` from `/proc/<pid>/status`.
The fixed request count makes the two runs comparable.

Measure a CPU topic this way:
read `utime` and `stime` from `/proc/<pid>/stat` before and after the same 200 requests.
Sum `utime` and `stime` in each read, subtract the before sum from the after sum, and divide by `getconf CLK_TCK` to get seconds.
The fixed request count and the unit conversion make the two runs comparable.

## Evidence

Keep the raw measurement logs in `~/scripts/<YYYYMMDD>_improve-perf/`.
Write one line per metric in the pull request's `## Evidence` section.
A line names the metric, the before value, the after value, the sample count, and the change.
Link each log file with the file-server URL from the landing protocol.
A reviewer checks the numbers without rerunning the measurement.
