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
| `--side both` | Measure the before side, then the after side. If the before side's calibration does not reproduce, stop with exit 3 and do not run the after side. |
| `--out-dir DIR` | Where the run's files go, outside the tree. |
| `--repetitions N` | Repetitions per scenario (default 5). |
| `--scenarios LIST` | Any of `background,held,reset,stream,starred,threads` (default the first four). |
| `--assets-dir DIR` | Where the CDN copies are kept (default `DIR/assets`). |
| `--detach` | Start a transient systemd user unit and return; the log is `DIR/pn-<id>.log`. |
| `--lock FILE` | Hold an advisory lock on `FILE` around each side's trial (not the builds), waiting up to 2 h. |

Exit codes: 0 both sides measured; 1 a measurement or build failed (the side's JSON still names the error); 2 a
precondition failed and nothing started; 3 the baseline calibration did not reproduce; 75 a side's wait for the lock
ran out. One lock hold covers one side attempt: a caller that retries a timed-out side runs the command again, which
takes the lock again at the back of the queue.

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

The calibration's condition is the plan's: on the before side, a session switch clicked while a background list refresh
is in flight must take at least `CALIBRATION_HOLD_MS` (10 s) to render. Each background repetition makes a second
switch, whose click the bench times to land while the list refresh that a `task_tree_changed` frame has just triggered
is in flight; the bench reads the frame and the refresh from its own request records, and checks afterwards that the
refresh was still in flight when the switch's bootstrap request started. If the median of the five raw holds stays
under 10 s, the proxy does not model the link, and `--side both` stops before the after side with exit 3.

Both sides print the raw holds in the calibration line. The gate reads the before side only, so the after side's holds
show whether the wait a person felt is gone after the fixes. The scored switch scenario keeps its clicks at random
points of the refresh cycle, so its numbers stay comparable with the baseline.

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
once into the assets directory, and records its SHA-256 in the JSON. One URL is unpinned (`marked`), so a changed copy
shows as a changed hash. Chrome serves those copies locally through the
DevTools Fetch domain. Any other external host fails at once, so nothing outside the trial reaches the link.

The gap: those CDN bytes do not cross the shaped link, so the bench does not count them. The bench measures the
application's own traffic.

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
port. The report's tests check the calibration gate: it reads the during-refresh holds, not the scored scenario's
random-phase holds, and only the before side's line carries a verdict. The chrome tests check the browser teardown: a
closed browser fails its unanswered DevTools calls and drops its pipe handles.
