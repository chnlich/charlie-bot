# Poor-network browser bench

One command drives the web client in headless Chrome through a shaping proxy at the link profile in `profile.mjs`.
The browser's connections all share that proxy's bandwidth budget. The client talks to a trial server on the mock home.
The bench reports six scenarios, five repetitions each, as JSON and as a summary table with a pass or fail against each
target.

## Run

The bench runs on the build host through the remote runner, because it builds the server and starts a trial:

```sh
dev/remote-run/run.sh dev/poor-network/run.sh --side after --out-dir "$OUT"
dev/remote-run/run.sh dev/poor-network/run.sh --side both --before-clone "$CLONE" --out-dir "$OUT"
```

| Option | Meaning |
|---|---|
| `--side after` | Measure this tree. |
| `--side before` | Measure `--before-rev` (default `origin/main`) of the clone at `--before-clone`. |
| `--side both` | Measure the before side, then the after side. If the before side is the reference commit (`CALIBRATION_REFERENCE`) and its calibration does not reproduce, stop with exit 3 and do not run the after side. |
| `--out-dir DIR` | Where the run's files go, outside the tree. |
| `--repetitions N` | Repetitions per scenario (default 5). |
| `--scenarios LIST` | Any of `background,held,reset,stream,starred,threads,open` (default the first four). `open` runs its own cells, see below. |
| `--assets-dir DIR` | Where the CDN copies are kept (default `DIR/assets`). |
| `--detach` | Start a transient systemd user unit and return; the log is `DIR/pn-<id>.log`. |
| `--lock FILE` | Hold an advisory lock on `FILE` around each side's trial (not the builds), waiting up to 2 h. |

Exit codes: 0 both sides measured; 1 a measurement or build failed (the side's JSON still names the error); 2 a
precondition failed and nothing started; 3 the baseline calibration did not reproduce; 75 a side's wait for the lock
ran out. One lock hold covers one side attempt: a caller that retries a timed-out side runs the command again, which
takes the lock again at the back of the queue. The side JSON is the bench's last artifact; when it is 120 s old and the
bench still runs, the runner ends the trial tree itself, so a bench that hangs after its work never holds the lock, and
the summary is reprinted from the JSON.

Chrome comes from `POOR_NETWORK_CHROME`, or else the newest `~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`.
The trial replaces `HOME`, so the discovery happens in `run.sh`.

Output: `DIR/<run id>/before.json` and `after.json`, one per side, with the link, the targets, the Chrome version, the
asset hashes, the session ids, the view as measured against the live view, and every repetition's raw numbers. The
summary table prints to stdout, with the view line and the live readback line.

## Link profile and targets

The link is in `LINK` and the targets are in `TARGETS` of `profile.mjs`. No other file names them.

- Round trip 300 ms: each direction adds 150 ms to every byte.
- 100 kbit/s per direction (12,500 bytes/s), one budget per direction shared by every browser connection. Bytes move in
  1 KB quanta, round robin across the connections.
- A 1 s delay spike once every 60 s, both directions.
- A scripted 30 s blackout, in `hold` mode (bytes wait) or `reset` mode (every connection is reset when it starts).

| Scenario | Measures | Target |
|---|---|---|
| 1 background, received and sent | KB/min at the proxy over one task-tree window | <= 75 KB/min each direction |
| 2 switch during background | click to rendered, over the floor (300 ms + bootstrap bytes / 12.5 KB/s) | <= 1000 ms over floor |
| 3 send during background | click to echo, over the floor (300 ms + server processing) | <= 500 ms over floor |
| 4 held blackout | restore to the last switch rendered, over that switch's floor | <= 2000 ms over floor |
| 5 reset blackout | restore to `catchup_complete`, over the catch-up bytes / 12.5 KB/s | <= 2000 ms over the byte time |
| 6 long stream | socket bytes of one reply, per reply text byte | <= 1.20 |

Each scenario's median and 90th percentile go in the table, with the raw median next to them for the time metrics.

Runs that measure side by side share the host: overlapping trials put a mode of about 3 s into the switch and send
timings, while byte counts stay the same. Bench runs that pass the same `--lock` file never overlap. Every repetition
also records its start, its end and the other serve trials on the host during it (`trials.mjs`, a sample every 30 s),
and the summary prints them.

Below the table, scenario 1's received bytes are split by endpoint. The visible tab's status polls are counted by the
server's answer, the wire status: Chrome hands a revalidated answer to the page as its cached 200, and only the wire
status says 304. Each 200 on the wire is compared with the body the tab last held for the same URL, so the report names
the fields that changed. A 200 that repeats the previous body means the browser did not revalidate.

## Other sidebar views

`starred` and `threads` measure scenario 1's background with the visible tab on the starred view or the Threads view.
Those views refetch their whole list on every task-tree frame. At its first repetition, the bench fills the view to the
live size in `OTHER_VIEWS`:

- the starred view: the background session and the first mock rows of the Workspace, starred;
- the Threads view: sessions created with a Slack origin, which makes each one a thread root.

The task-tree driver renames a row of the view, so the view's rows change at the rate the Workspace's rows change in
scenario 1. These scenarios have no target. The report prints their received and sent bytes, the endpoint split and the
status polls.

## Capped steps

A switch or a send in scenarios 2 and 3 that has not finished within 180 s is recorded at 180 s, and the report counts it
in the `at cap` column. Its time is then a lower bound: the step took at least that long. The record keeps the page's
state at the cap (the sockets, the frames seen, and whether the target's bootstrap finished), so the reader can see why.
When the target's bootstrap never finished, the step's bytes are the size of the same body on the wire, read from the
server, so the floor still follows the bytes the step needs.

## Scenarios 4 to 6 have no background

Scenarios 1 to 3 share one repetition: the background load runs, the proxy counts its bytes, and the switch and the
send happen inside that background. The calibration's during-refresh switch (see Calibration) is a second switch in the
same repetition, after the send. The targets of scenarios 4 to 6 do not mention a background load, so those
repetitions run without one.

## Calibration

The calibration's condition is the plan's: a session switch clicked while a background list refresh is in flight must
take at least `CALIBRATION_HOLD_MS` (10 s) to render. Each background repetition makes a second switch, whose click the
bench times to land while the list refresh that a `task_tree_changed` frame has just triggered is in flight; the bench
reads the frame and the refresh from its own request records, and checks afterwards that the refresh was still in
flight when the switch's bootstrap request started.

The gate reads one before side: the pre-fix baseline named by `CALIBRATION_REFERENCE` in `profile.mjs`. The fixes
remove the background bursts the hold lived on, so a fix's own before side no longer reproduces the condition; a
`--side both` run stops with exit 3 only when its before side is the reference and the median of its five raw holds
stays under 10 s. Every other before side runs ungated, and both sides print the raw holds in the calibration line, so
the after side's holds show whether the wait a person felt is gone. The scored switch scenario keeps its clicks at
random points of the refresh cycle, so its numbers stay comparable with the baseline.

To exercise the gate, run the before side at that commit: `--side both --before-rev <CALIBRATION_REFERENCE>`, in a
clone that holds the commit.

A second check sets the background scenario's received rate beside the live readback in `LIVE_READBACK`: about 206 KB a
minute, from one visible tab on a live release. The table prints the gap. The check does not gate the run: a gap is a
finding to state with the numbers.

## Sessions and the server

The trial server runs on the mock home, which holds the view's rows that the bench's own sessions do not: `VIEW.rows`
less `BENCH_SESSIONS` (113 sessions with the current profile), generated with `--shape view`. Each carries a task spec of
synthetic acceptance criteria and context references, sized so the rows' median matches the live view's; the default
shape writes no task spec. The mock's sessions carry no task boundary, so the server refuses a message to them. The bench therefore creates its sessions through `POST /api/sessions/`, on the trial's backend
option. Those sessions are rows of the default view too, so the view holds `VIEW.rows` rows.

The view's shape comes from a live release's default "all" view: 124 rows, a raw median row of 3,260 bytes, and a switch
target whose bootstrap content takes a median 26,592 bytes on the wire. The bench checks the row count before any
scenario runs and records the median raw row in the JSON. The switch target and the held targets take turns until their
bootstrap content reaches 26,592 bytes on the wire untrimmed — the size the server names in
`x-bootstrap-untrimmed-bytes`, since the first screen's trim keeps the served body near its 10 KB budget — and the JSON
records each one's turns and bytes. The other bench sessions take one turn. Each session's last turn carries a seed text
unique to it, and the page shows a switch once that text renders. The switch floor adjusts for the bytes the page's
bootstrap actually moved.

The trial's backend is `charlie-code`, which `run.sh` puts on `PATH` as `stub/charlie-code`. Each turn the stub prints the
next of 16 reply pieces, 8 KB each, cut from repository docs (`reply.mjs`), in the charlie-code NDJSON the backend reads.
The pieces differ, so the gzip of a session's history does not fold its replies into one copy. Each reply streams as
about three or four text events of 2.2 KB, and the server keeps each event as one chat message; a bootstrap carries the
last 40 messages, of which the event size and the turns' own messages set how much reply text a switch target carries. The stub calls no model
and reads no credential. A prompt carrying the marker `bench-hold` keeps its turn open for the background load, until
the bench cancels it.

The web client reads its access key from `localStorage` and sends it as the `Authorization` header and as the `token`
of each WebSocket URL. The bench writes the same `localStorage` entry before each page loads, so the page behaves as
after a login.

The TLS front serves a self-signed certificate. Chrome trusts its key by hash (`--ignore-certificate-errors-spki-list`),
so the certificate verifies as Serve's does in a person's browser. Under `--ignore-certificate-errors` Chrome would
store no response in its HTTP cache. It would then never revalidate, and every JSON read would answer 200.

## CDN assets

The page loads scripts and styles from CDNs (jsDelivr, cdnjs, Google Fonts). `assets.mjs` downloads a copy of each URL
once into the assets directory: the raw bytes as the CDN sends them to a browser that accepts brotli, the headers the
browser's cache and Resource Timing read, and the decoded body's SHA-256, which the JSON records. One URL is unpinned
(`marked`), so a changed copy shows as a changed hash. The scenarios other than `open` serve those copies from the
browser's request interception, so the CDN bytes never cross the shaped link and the bench measures the application's
own traffic. Any other external host fails at once, so nothing outside the trial reaches the link.

## Open scenario (`--scenarios open`)

The cases of plan 2 section 3, under the link's conditions. The cases name the browser's state before the open:

- **A**: this browser opened this session before. Its profile holds the session's record and the CDN files in the HTTP
  cache, and its pool holds a connection from that visit.
- **B**: this browser opened a chat page before, but not this session. The record names another session.
- **C**: the browser never opened a chat page. Its profile is empty.

The record is `localStorage` key `charliebot_opened_pages`, written by `web/static/js/opened-sessions.js`, which owns
the case. A side whose client predates that module (origin/main before the telemetry lands) gets the same record from
the bench, so both sides start from the same browser state; the measured page's own case is compared with the cell's
only when its client reports one.

Conditions, each a fresh browser profile behind a fresh proxy, with the CDN files crossing the link:

| Condition | What the link does during the open |
|---|---|
| `link` | Only the link model: 300 ms round trip, 12.5 KB/s per direction, the 1 s stall once a minute. |
| `held` | A 30 s held blackout starts 1 s before the open navigates, so the open is inside it. |
| `reset` | As `held`, but the blackout resets every connection when it starts. The pooled connection of A and B ends there. |
| `flaps` | Up 5 s, then down 5 s, six times (60 s). Each drop resets every connection, so each recovery reconnects. |
| `offline` | Chrome's network emulation is offline for the open. The navigation fails at once and the open is not reached. |
| `release` | A held blackout starts; 5 s in, the server's static asset version changes (the bench appends a comment to the trial tree's served `web/static/js/config.js`, and the version the server answers must move); the open navigates 10 s in, inside the hold. |

The open is `switchTarget` (a session whose bootstrap is the view's 26,592 bytes); B's other session is `probes[0]`.
The CPU rate is `Emulation.setCPUThrottlingRate`. The cells run in this order: a reference cell (A, link, rate 1), the
other conditions at the derived rate, then the release cells, because each release changes the version and each
case's warm profile is built at the version then served.

**CPU rate.** `profile.mjs` holds `LIVE_CPU_TAIL`, the median of Chao's device's page loads with cause `open` (13 loads
on 100.84.122.38 from the live log, 2026-11-06): 297 ms of DOMContentLoaded minus responseEnd. The tail is used instead
of the raw DOMContentLoaded, because the raw value includes the document's transfer, and the live loads ran about ten
times faster than the modeled link (a 340 KB document at 2.5 s). The other device has no `open` loads (one
`stale_asset` load), so it gives no rate. The reference cell's tail median, measured at rate 1 on the same kind of load
(A, cache-warm, the link), gives the rate: `max(1, live tail / reference tail)`. The run prints both inputs.

**Measured per repetition.** Times are ms from the page's time origin (navigation start): `newest_message_ms` is when the
text of the session's newest message is in `#messages`, `catchup_ms` is the first `catchup_complete` frame of the page's
socket, and `load_ms` is the load event. `bytes_to_newest` is the link's bytes (both directions) from the navigation to
that moment, and `bytes_after_load` the bytes in the 30 s after the load event; the proxy's counters are sampled every
25 ms. `dcl_ms`, `response_end_ms` and `tail_ms` come from the page's Navigation Timing, and `mark` is whether the
not-current mark was in the document at any sample. Each repetition also names what the tab and the page saw:
`sockets` (the WebSocket opens), `frames` and `frame_types` (the socket frames by type), `requests` (the requests),
`ws_state` (the client's own socket state), and the last console lines and page errors, so a missing time can be read
against what the page reported.

**The mark's DOM contract (task 3 implements it).** A page marks a stored copy as not current with the attribute
`data-freshness="not-current"` on one element of its document. The bench watches for that element and reports the
first time it appears. Before task 3 no page carries the attribute, so every cell reports the mark as absent.

**Page's own timings.** A page reports `newest_message_ms` and `current_ms` in its `page_load` event
(`web/features/diag/templates/diag/client-events.js`), with the open case, the content source and the render-blocking
files beside their same-origin timings. `dev/client-readback/readback.mjs` reads them from the server log.

**Gaps.** The CDN stand-in serves the webfont stylesheet empty, as the other scenarios do, so the font files it names
are not loaded. The bench's DOMContentLoaded and newest-message times are measured from outside the page, so they are
not compared with the page's own `page_load` numbers in the same run.

**Output.** The JSON's `open` section holds `cpu` (the rate and its inputs), `reference`, and `cells`: each cell's
`reps`, one object per repetition with the fields above. The table prints one row per cell: the median newest-message
time, the five values in repetition order, the catch-up median, the bytes to the newest message and after the load, and
how many repetitions reached the newest message.

## Host requirements

Linux with user systemd, `node` 20 or later, `cargo`, `git`, `openssl`, `flock`, and a Chrome build. The trial needs a
16 GB memory scope and about 1 GB of free space in `/tmp` for its copy of the mock home. Each run deletes its trial
tree when it ends; a run that is killed leaves the tree, which the next person removes by hand.

## Relay for the perf loop

`relay.mjs` runs the same shaping proxy as a command, so a trial server's loads cross the bench's link without the
browser. The perf loop's slow-network measure starts it against the trial's `CHARLIEBOT_SERVE_URL`:

```sh
node dev/poor-network/relay.mjs --target "$CHARLIEBOT_SERVE_URL"
```

It prints its own address, `http://127.0.0.1:<port>`, as its first stdout line, and runs until SIGINT or SIGTERM,
which stops it and every tunnel it holds. Its tunnels reach the target's port only, and they share the link's budget per
direction, as the bench's connections do. A target without an `http` port or with a path exits non-zero and prints no
address.

## Tests

The relay tests are integration tests because they start the relay as a child process. The Integration workflow runs
them when a change touches this directory. To reproduce them through the remote runner:

```sh
dev/remote-run/run.sh node --test dev/poor-network/proxy.integration.test.mjs dev/poor-network/report.test.mjs \
  dev/poor-network/chrome.test.mjs
```

They check the relay's tunnel transfer time at the link, its stop on SIGTERM, and its refusal of a target without a
port. Two tests drive the shaping proxy itself: a route reaches its own server over the same link while the proxy
refuses every other authority, and a scheduled reset window resets the open tunnels when it starts. The report's tests
check the calibration gate: it reads the during-refresh holds, not the scored scenario's random-phase holds, and only
the reference before side's line carries a verdict. The chrome tests check the browser teardown: a closed browser
fails its unanswered DevTools calls and drops its pipe handles.
