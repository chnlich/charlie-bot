# Improve direction: performance

You run the performance direction of the improve loop.
Follow `dev/improve/landing.md` for the round protocol.
That file carries the shared round contract.
Your branch prefix is `improve/perf/`.

## Scope

Keep performance changes in:

- the server code in `rust/`
- the pages in `web/`

## Topics

Rank topics by the user's wait in seconds: page topics use slow-network load time, and GET-route topics use slow-network
transfer time. Byte-class and ratio-class scenarios in the poor-network bench do not rank. The bench's verdict decides
whether a round whose change can affect one of those scenarios takes it as a topic.
The poor-network rule sits in its own section, `## Poor-network scenarios`.

## Poor-network scenarios

An improve round measures the serve trial.
A round does not run the poor-network bench.
The bench's shared scenario definitions and target verdicts live in `dev/poor-network/profile.mjs`.
The bench's verdict is authoritative; do not copy target values into this direction.
A scenario that passes before and fails after blocks landing.
When a change needs poor-network rows, the round opens or keeps its pull request.
The round records `poor-network rows pending` in the pull request body.
The round does not merge that pull request.
The poor-network rows for such a change run outside the round, through the route that the parent session names.

Each take of the host's bench lock waits at most 15 minutes. A serve-trial side takes the lock once and waits at most
15 minutes. If a wait expires, the timed-out side takes no measurement. The round reports no result from an incomplete
pair, stops without opening a pull request and names the held bench lock in its stop reason. The next scheduled fire
tries again.
The poor-network bench exposes this limit through its `--lock-wait` contract; an improve round uses 900 seconds.

## Mock home

The runner supplies one complete shared mock home with the production number of sessions. Both sides read the same
data, and the round does not change it. This keeps the workload representative and the comparison fair.

## Binaries

The before and after servers use the same release profile on the same host. When server code changes, the before server
represents `origin/main` and the after server represents the branch. For a web-only change, both trees use the same
server binary. The intended change is the only source difference between the sides.

When Rust unit tests change, the parallel-run and isolated-run limits are both 0.08 seconds. The repository's timing
gate checks tests above the parallel threshold alone and fails tests that remain over the isolated limit. The pull
request evidence reports each changed test's isolated time.

## Serving

Each side runs one serve trial against the shared mock home. The `dev/improve/trial-side.sh` entry holds the host's
bench lock from trial startup through measurements and shutdown. Builds stay outside the lock. The round's scope owns
the memory limit; the trial adds no separate scope.

The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to its measurement
command. Requests and process measurements use that trial's server. The mock-home check is a prerequisite for each
side; if its server reads outside the shared mock home, stop the round without using that side's evidence.

The before and after workload uses the same host, fixture, measurement order and load model. A result is reported only
when both sides complete under comparable conditions.

## Measures

Report latency as the median of 50 requests, after 5 warm-up requests. Keep warm-up requests out of warm samples and
keep cold samples representative of first requests. Apply the same sample treatment to both sides.

Search latency covers a common query, a query that matches 200 rows and a rare query against the mock home's
fixed word list. Measure each query with the latency rule above. Keep its background event load separate from the
shared task-tree load, and preserve both workloads between sides.

Cold-query latency represents the first query with server content uncached, without changing the shared mock home.
Take 10 cold samples per round and report their median.

Slow-network page loads use the poor-network bench's shared link profile and shaping proxy. If the required relay cannot
run on the host, report that defect instead of substituting another link model. A load includes the document, its GET
API responses and its referenced static files in served order. Cold loads use one connection per fetch; warm loads use
one kept-alive connection. Measure elapsed time from the document request until the final response byte and count all
response-body bytes received by the client. The measured pages are the chat page with the largest mock-home session
open, that session's events viewer and the home page. Each page takes 10 cold loads first, then 5 warm-up loads and
10 warm loads, and reports the median of the 10 cold loads and of the 10 warm loads.

Startup latency is the interval from the server's first log timestamp to its first successful HTTP request. Repeat five
runs and summarize them with their median.

Memory reports peak resident memory after the same fixed 200 requests on each side. CPU reports user and system
CPU time spent serving those 200 requests. Keep the workload identical between sides.

## Evidence

The pull request evidence names each metric's before and after values, sample count and change, with enough retained
logs for a reviewer to verify the result. The mock home and host differ from production, so absolute values are
comparable only within one round and do not represent production measurements. The round's retained evidence includes
the raw measurement logs.
