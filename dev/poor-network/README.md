# Poor-network browser bench

One command drives the web client in headless Chrome through a shaping proxy at the link profile in `profile.mjs`.
The browser's connections all share that proxy's bandwidth budget. The client talks to a trial server on the mock home.
The bench reports seven scenarios, five repetitions each, as JSON and as a summary table with a pass or fail against each
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
| `--scenarios LIST` | Any of `background,held,reset,stream,tools,starred,threads,open,search` (default the first four). `open` runs its own cells, see below. `search` runs its own repetitions and variants, see "Search view". |
| `--open-conditions LIST` | Open cells: any of `link,held,reset,flaps,offline,release,release-offline,replied-offline`; default all. |
| `--open-cpu-rate RATE` | Run every side's open cells at `RATE` instead of a derived rate (see CPU rate below). |
| `--assets-dir DIR` | Where the CDN copies are kept (default `DIR/assets`). |
| `--lock-wait SECONDS` | How long each side waits for the bench lock (default 7200). An improve round passes 900. |
| `--seed-home DIR` | The view home that the run copies as its seed (default: the shared home under `~/improve/mock-home`). A run with a pinned home names it here; each side still checks the copy's content hash. |
| `--detach` | Start a transient systemd user unit and return; the log is `DIR/pn-<id>.log`. |

Exit codes: 0 both sides measured; 1 a measurement or build failed (the side's JSON still names the error); 2 a
precondition failed and nothing started; 3 the baseline calibration did not reproduce; 4 a scenario the bench passed
on the before side fails on the after side; 75 a side's wait for the lock ran out. Every side takes the bench's own
advisory lock that `lock.sh` names, around its trial (not the builds), waiting up to 2 h for it, or the `--lock-wait`
seconds a caller names; no option chooses another lock file or skips it. The remote-wake bench takes the same lock for
its container's whole run, because each start and stop of that container adds or removes a veth interface, which Chrome
reports as a network change and drops the page in transfer. Each improve-ux and improve-perf trial side
(`dev/improve/trial-side.sh`) takes the same lock from its trial's start to its stop, waiting at most 15 minutes; when
the wait runs out, the side runs nothing and exits 75. One lock hold covers one side attempt: a caller that retries a
timed-out side runs the command again, which takes the lock again at the back of the queue. The side JSON is the
bench's final artifact. If the bench is still running 120 s later, the runner ends its trial tree to release the lock
and reprints the summary from that JSON.

Chrome comes from `POOR_NETWORK_CHROME`, or else the newest `~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`.
The trial replaces `HOME`, so the discovery happens in `run.sh`.
The trial points `TMPDIR` into its tree, and Chrome's process-singleton socket path is short, so `run.sh` passes the
caller's own `TMPDIR` to Chrome as `POOR_NETWORK_CHROME_TMPDIR`.

Output: `DIR/<run id>/before.json` and `after.json`, one per side, with the link, the targets, the Chrome version, the
asset hashes, the session ids, the view as measured against the live view, and every repetition's raw numbers. The
summary table prints to stdout, with the view line and the live readback line.

## Link profile and targets

The link is in `LINK` and the targets are in `TARGETS` of `profile.mjs`. No other file names them.

- Round trip 300 ms: each direction adds 150 ms to every byte.
- 100 kbit/s per direction (12,500 bytes/s), one budget per direction shared by every browser connection. Bytes move in
  1 KB quanta, round robin across the connections.
- A 1 s delay spike once every 60 s, both directions.
- A scripted 30 s blackout that drops every packet in both directions, a new connection's SYN included. The userland
  shaper stops established tunnel bytes; a loopback firewall rule drops SYNs to the proxy before its listener accepts
  them. Linux retries those connections on its TCP timer, and the first retry after restore reaches the proxy. Blackout
  scenarios require `sudo -n iptables` access; `run.sh` checks it before building. In `hold` mode established
  connections survive and their bytes travel again at restore. In `reset` mode every established tunnel is reset when
  the blackout starts.

| Scenario | Measures | Target |
|---|---|---|
| 1 background, received and sent | KB/min at the proxy over one task-tree window | <= 75 KB/min each direction |
| 2 switch during background | click to rendered, over the floor (300 ms + bootstrap bytes / 12.5 KB/s) | <= 1000 ms over floor |
| 2 switch to real-time connection | click to the target session's `catchup_complete` | reported, not scored |
| 3 send during background | click to echo, over the floor (300 ms + server processing) | <= 500 ms over floor |
| 4 held blackout | restore to the last switch rendered, over that switch's floor | <= 2000 ms over floor |
| 5 reset blackout | restore to `catchup_complete`, over the catch-up bytes / 12.5 KB/s | <= 2000 ms over the byte time |
| 6 long stream | socket bytes of one reply, per reply text byte | <= 1.20 |
| 7 tool stream | stream frames' tool-row bytes, per final row byte; the turn's socket bytes per minute | <= 2.0; <= 75 KB/min |

The switch to real-time connection number is the before value of the one-socket-per-tab fix: today's page opens a new
socket for the target after the first screen renders, and the fix keeps the page's socket and subscribes the session on
it. The bench reads both the same way — today's `catchup_complete` belongs to the socket the page opened for that
session, and the fix's frame names its session. The send of scenario 3 waits for the same frame: a person types for
longer than a socket's setup takes, and the send floor assumes an open socket.

That wait fixes the send's phase in the repetition, so a link hold would land on it in every repetition. A 1 s delay
spike is the link's time and not the page's, so the send's click waits out a hold that covers the next second
(`holdEndWithin` in `proxy.mjs`).

Each scenario's median and 90th percentile go in the table, with the raw median next to them for the time metrics.

Runs that measure side by side share the host: overlapping trials put a mode of about 3 s into the switch and send
timings, while byte counts stay the same. Bench runs on a host never overlap: every side holds the host's
one bench lock, which the remote-wake bench also holds while its container runs and each improve trial side holds too. Every repetition also records its start, its end and the other serve trials on the host
during it (`trials.mjs`, a sample every 30 s), and the summary prints them.

Below the table, scenario 1's received bytes are split by endpoint. The visible tab's status polls are counted by the
server's answer, the wire status: Chrome hands a revalidated answer to the page as its cached 200, and only the wire
status says 304. Each 200 is applied to the row state the tab held for its row set — the poll's URL without its `since`
token — so the report names the fields that changed; the change's server answers only the rows that moved
(`{"token", "rows", "gone"}`), and the baseline's answers the whole map. A 200 that repeats the state the tab held moved
nothing: its body was resent with no row changed. The report also prints the share of 200 answers beside the live
device's `LIVE_READBACK.status200Share`, and the bytes the tab received per 200 and per 304.

## Status churn

The status body of a live tab changes on its own: the live device answered 200 to 72% of about 20 polls a minute, each
answer about 5.8 KB, so roughly 14 answers a minute differed from the one before. Two kinds of change make that churn,
and both are one row's own state at `STATUS_CHURN`'s rates, per 75 s window:

- a held turn starts and is cancelled (14 flips), moving `thinking_since`, `has_running_tasks` and `work_state`. The
  server broadcasts each change, so the page polls at once and the change also costs an extra poll;
- a delayed trigger appears on the row and is cancelled (51 flips), moving `has_pending_trigger`,
  `pending_trigger_count` and `next_trigger_at`. Nothing broadcasts it, so the page meets it on its next poll — the poll
  whose body a delta now keeps small.

The background scenarios start both with the task-tree driver and stop them with the load: the Workspace's churn row is
the bench's twelfth session, the starred view stars that session, and the Threads view churns one of its thread roots.
The pair is tuned so that the background tab, which polls about 27 times a minute while the held turn keeps its cadence
at 3 s, answers a changed body at the live device's share; consecutive trigger flips cancel one another between two
polls, so about half the flips are caught — 33 a window measured a 0.56 share, 44 a 0.63, and 51 the live one. The
report prints the measured share beside the readback's.

The window also reports two counters of the visible tab's sidebar: the full sidebar repaints (the paints that replace
the session list's rows wholesale) and the spinner restarts (a visible running spinner's rotation jumping against its
steady turn), both per minute. They are findings, not targets: the summary prints them, and a fix that keeps a running
row's spinner through an update reads as restarts near zero.

## Other sidebar views

`starred` and `threads` measure scenario 1's background with the visible tab on the starred view or the Threads view.
At its first repetition, the bench fills the view to the live size in `OTHER_VIEWS`:

- the starred view: the background session, the churn session and the first mock rows of the Workspace, starred;
- the Threads view: sessions created with a Slack origin, which makes each one a thread root.

The task-tree driver renames a row of the view, and the status churn flips another, so the view's rows change at the
rate the Workspace's rows change in scenario 1. A task-tree frame fetches the named row alone on both of the change's
paths — the row refresh and the status delta — so the whole list is fetched only on the page's open, a view change and
a reconnect after a network loss. These scenarios have no target. The report prints their received and sent bytes, the
endpoint split, the status polls and the rows the tab painted.

## Switch clicks

A sidebar group paints its first five roots and keeps the others behind its Show all toggle. The chat page carries only
the rows its first screen paints, so a kept-back row is absent from the document until the toggle fetches the full
listing. Before a switch, the bench puts the target's row on screen as a person does: when the row is not painted, it
clicks the Show all of the session's group and waits for the row. The switch's time starts at the row's click. The held
blackout's five targets are put on screen before the blackout starts, because a Show all inside the blackout would wait
for the link. The bench clicks only painted rows, so a target that no toggle reaches stops the run.

## Capped steps

A switch or a send in scenarios 2 and 3 that has not finished within 180 s is recorded at 180 s, and the report counts it
in the `at cap` column. Its time is then a lower bound: the step took at least that long. The record keeps the page's
state at the cap (the sockets, the frames seen, and whether the target's bootstrap finished), so the reader can see why.
When the target's bootstrap never finished, the step's bytes are the size of the same body on the wire, read from the
server, so the floor still follows the bytes the step needs.

## Scenarios 4 to 7 have no background

Scenarios 1 to 3 share one repetition: the background load runs, the proxy counts its bytes, and the switch and the
send happen inside that background. The calibration's during-refresh switch (see Calibration) is a second switch in the
same repetition, after the send. The targets of scenarios 4 to 7 do not mention a background load, so those
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
less `BENCH_SESSIONS` and `DEFAULT_CRON_ROWS` (109 sessions with the current profile), generated with `--shape view`. The
three default cron tasks' sessions are the rows that the server's first scheduler tick adds to every trial home, so the
bench counts them as the mock's rows and waits for that tick before it measures the view. Each carries a task spec of
synthetic acceptance criteria and context references, sized so the rows' median matches the live view's; the default
shape writes no task spec. The mock's sessions carry no task boundary, so the server refuses a message to them. The bench therefore creates its sessions through `POST /api/sessions/`, on the trial's backend
option. Those sessions are rows of the default view too, so the view holds `VIEW.rows` rows.

The view's shape comes from a live release's default "all" view: 124 rows, a raw median row of 3,260 bytes, and a switch
target whose bootstrap content takes a median 26,592 bytes on the wire. The bench checks the row count before any
scenario runs and records the median raw row in the JSON. The switch target and the held targets take turns until their
bootstrap content reaches 26,592 bytes on the wire untrimmed — the size the server names in
`x-bootstrap-untrimmed-bytes`, since the first screen's trim keeps the served body near its 10 KB budget — and the JSON
records each one's turns and bytes. The other bench sessions take one turn. Each session's last turn carries a seed text
unique to it, and the page shows a switch once that text renders. Every background repetition sends the switch target a
fresh turn before its switch, and takes the switch's marker from a plain-text run of the reply the trimmed bootstrap
serves: the trim is the switch's first screen, so the marker sits inside the 40 messages the switch loads first however
far the session has grown, and the stub prints a different reply piece each turn, so the run names this repetition's
turn. A run already on the page cannot time the switch, so the first unseen one wins. The switch floor adjusts for the
bytes the page's bootstrap actually moved.

The trial's backend is `charlie-code`, which `run.sh` puts on `PATH` as `stub/charlie-code`. Each turn the stub prints the
next of 16 reply pieces, 8 KB each, cut from repository docs (`reply.mjs`), in the charlie-code NDJSON the backend reads.
The pieces differ, so the gzip of a session's history does not fold its replies into one copy. Each reply streams as
about three or four text events of 2.2 KB, and the server keeps each event as one chat message; a bootstrap carries the
last 40 messages, of which the event size and the turns' own messages set how much reply text a switch target carries. The stub calls no model
and reads no credential. A prompt carrying the marker `bench-hold` keeps its turn open for the background load, until
the bench cancels it; the status churn sends such a prompt and cancels it, so one row's activity facts move. A prompt
carrying the marker `bench-tools` prints the tool piece instead: 20 Bash call and result pairs whose final rows are
770 bytes each, one call every 5 s, then a short reply. Each NDJSON line states its replay delay in `bench_delay_s`,
which the stub sleeps for; a line that states none keeps 0.05 s. Scenario 7 sends that prompt.

The web client reads its access key from `localStorage` and sends it as the `Authorization` header and as the `token`
of each WebSocket URL. The bench writes the same `localStorage` entry before each page loads, so the page behaves as
after a login.

The TLS front serves a self-signed certificate. Chrome trusts its key by hash (`--ignore-certificate-errors-spki-list`),
so the certificate verifies as Serve's does in a person's browser. Under `--ignore-certificate-errors` Chrome would
store no response in its HTTP cache. It would then never revalidate, and every JSON read would answer 200.

## TLS records

The live front is Tailscale Serve, which terminates TLS in Go. Go's `crypto/tls` picks a connection's record sizes
(`maxPayloadSizeForWrite` in `crypto/tls/conn.go`): records grow in arithmetic progression from one TCP MSS estimate
(1208 bytes less the record's overhead, 1186 payload bytes for a TLS 1.3 AEAD suite) up to 16 KB, and the connection
uses 16 KB records once it has sent 128 KB. A Node TLS record carries whatever one write gives it, so the front writes
each record on its own at the size Go would pick, on both the HTTP/2 responses and the upgraded socket. A body the
server writes in one 11.7 KB piece therefore reaches the browser as records of 1.2 KB, 2.4 KB, 3.6 KB and so on, and
the browser decrypts its first bytes after the first record. `front.integration.test.mjs` reads the record headers the
front writes and checks those sizes.

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
| `offline` | Chrome's network emulation is offline for the open. The navigation fails at once with `net::ERR_INTERNET_DISCONNECTED` and the open is not reached. |
| `release` | A held blackout starts; 2 s in, the server's static asset version changes (the bench appends a comment to the trial tree's served `web/static/js/config.js`, and the version the server answers must move); the open navigates 5 s in, inside the hold. The open's document request waits in the hold and reaches the server after the release. A page that revalidates its document, as main's does, receives the released page in full and does not reload. A page that opens from a stored copy without asking the server is older than the release: when the link returns, its socket closes with 4409 and the page reloads once, inside the open, and the open's time runs from the first navigation through that reload. |
| `release-offline` | Case A only. A release lands; one unmeasured online open activates the release's worker and revalidates the stored shell (the open waits for that page's warm report); the measured open then navigates with the network off. The stored copy answers, so the row's verdict is the open's `reached` count: a release that emptied the stored copies shows no newest message offline. |
| `replied-offline` | Case A only. The served version is settled into the warm profile (as `release-offline` does), so the stored shell matches the server even after an earlier release in the invocation. A page of that profile sends one message; the reply streams back over the socket, and the page's stored-copy refresh reports the copy it re-stored; the measured open then navigates with the network off. The reply is the session's newest message, so the row's verdict is the open's `reached` count: a refresh that did not store the reply shows no newest message offline. The stored-copy rows fail when their measured document moves bytes on the wire. |

`A flaps` opens its page from the HTTP cache and reaches its newest message inside one up window. B's and C's documents
must cross the link in full (about 7.3 s at 12.5 KB/s), longer than one up window, so no window completes the body and
their opens are recorded at the cap. The offline cells fail at once and show no page: main without the offline shell
stores no page to show offline, so it has nothing to serve. With the shell, an opened session's stored copy answers the
offline open. The shell is per release: a release's first online open stores its shell, and activation drops the shell
of the release before it. The copies sit apart from the release, in a cache named for `COPY_FORMAT`
(`web/static/js/offline-shell-worker.js`): a release keeps the copies the release before it stored (`release-offline`
above), and a release that changes the copies' shape bumps the format, which drops them. A reply the socket delivered
re-stores the session's copy once the reply's frames settle, after a 2 s quiet gap (`replied-offline`).
The worker also stores the session page's on-demand files that render stored content: the Status panel with its
comment-post dependency, the For-you worker summary, file links, draft cards and artifact cards, each with its stylesheet
(`ON_DEMAND_FILES` in `web/static/js/offline-shell.js`, eight same-origin files). The page asks once it is current and
idle, after the first screen and the catch-up, so an offline open shows those views without their first online use. A
same-origin loader file whose only function talks to the server is listed in `ON_DEMAND_EXCLUDED` with its reason. The
cross-origin files stay out: the KaTeX loader and the terminal's xterm files (jsdelivr), and the diff page's CDN files.
The server's ETag names a bundled static file's bytes: the package and `web/static` mounts are `StaticDirectory::bundled`
(`static_files.rs`), and other mounts keep their mtime-and-size tag. So a release that leaves a stored file unchanged
revalidates it with a 304 and sends no body. Server change: each bundled file's ETag changes once, at the first request after the
release that carries it. A copy stored under the earlier tag matches no byte tag, so it is fetched whole once; later
releases revalidate.
`offline_shell_page.test.js` fails when a loader names a same-origin file that neither list names.

The release open navigates within 10 s of the last read on A's and B's pooled connection. Chrome sends a request on an
HTTP/2 connection idle that long only after a PING, which the hold leaves unanswered; Chrome then drops the connection
10 s later and retries on a new one, whose CONNECT the blackout drops. With the open 10 s in, every A and B release
repetition failed with `net::ERR_TIMED_OUT` 40.2 s after its navigation, against 30.1 s for C and for the reset cells,
whose new connection is the first attempt; A held, 1.6 s after the last read, loads. With the open 5 s in, all 20 A and
B release repetitions of a main-vs-main bench pair on 00574379c (2026-11-15, five per row on each bench) loaded, each
with the released page in full (90,803 to 90,805 B on the wire), and the bench that records reloads recorded none. C has
no pooled connection, so its first attempt falls inside the blackout in every blackout cell; the loopback filter drops
its SYN, and the client's first retry after the restore, about 31 s in, reaches the proxy. C's blackout cells therefore
reach their newest message about 20 s after the restore: a five-repetition run of this bench on product `ab531974` at
rate 3.8 (2026-11-16) measured `C held` at a 50,633 ms median, `C reset` at 49,800 ms and `C release` at 50,319 ms,
each 5 of 5.

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

The tail holds the parse and the blocking scripts only when the document's bytes arrive together. The reference document
does: case A's browser holds the page, the server still serves the same page, and the open revalidates the copy with a
`304` (300 B on the wire, about 324 ms to responseEnd), so the whole page is parsed after responseEnd (an 83.5 ms tail
at rate 1 in an earlier round, rate 3.56). A document that crosses the link in full streams for about 7.4 s and is
parsed while it arrives, so its tail keeps only the last chunk's work: about 10 ms at rate 1, which put the rate at
21.68 to 30.31 in six invocations of 2026-11-14 and 11-15. A reference repetition whose document crossed the link
(`document_transfer_bytes` not below `document_body_bytes`) therefore gives no rate, and the run stops. The one
revalidated open in the live log of Chao's other device (100.103.185.111, 2026-11-14) had a 282 ms tail, within 5% of
`LIVE_CPU_TAIL`. On main 00574379c (2026-11-15), every reference document of nine open-scenario invocations on host3 was
a `304`. Eight gave median tails of 78.2 to 87.8 ms and rates of 3.38 to 3.8; the ninth, in a busier window of the host,
gave tails of 78, 203.8 and 121.9 ms, so its median gave 2.44, and its loads ran two to three times as long as the other
invocations' loads. At rate 3.8, A link's tail was 309 to 412 ms, against the live 297 ms. A median outlasts slow
repetitions only while they are fewer than half, so a comparison across invocations pins the rate with
`--open-cpu-rate`.

Case A's open is a `304` only while the server still serves the entity tag of the page the warm build cached. Two
things would move that tag after the warm build, so the open scenario settles both before any warm profile caches a
page. The page's sidebar rows carry each session's unread mark, and an opened page marks its session read; the bench's
sessions start unread, so the bench marks read the two sessions its opens read (`switchTarget` and `probes[0]`). The
server compresses a chat page whole until a second chat page shares the first one's tail, and as the request's head
plus the shared tail from then on (`compressed_page` in `rust/crates/app/server/src/routes/pages.rs`); the tag hashes
the compressed bytes, so the same page carries a different tag in each form. The bench therefore requests both
sessions' pages before the warm builds, so the warm build's copy is already in the second form. A run that measured
the other scenarios first had settled both; a run of the open scenario alone did not, and every A open downloaded the
whole page.

The rate models the live device's CPU tail against the bench's reference tail on the same kind of load. Today the
document size difference is part of it: the live document is about 340 KB against the bench mock home's 151,545 B, so
the live tail carries more parsing than the reference tail does, and the ratio absorbs the difference. Both sides of
one `--side both` run at one rate: the before side's reference cell derives it, and the after side runs its cells at
that rate, so the sides' numbers compare at one CPU speed. `--open-cpu-rate RATE` pins the rate for every side of an
invocation instead, so the sides of two invocations (two benches on one product) compare at one speed. Each JSON
records the rate its cells ran at as `open.cpu.rate` and the rate its own reference tail gives as
`open.cpu.derivedRate`, so the host's drift between the sides stays readable.

**Host interference.** A start or stop of a Docker container on the host adds or removes a veth interface, and Chrome
treats the machine's network as changed: it drops every connection and fails the requests in flight with
`net::ERR_NETWORK_CHANGED`, a document cut mid-body included (the b401 runs' netlogs, 2026-11-08). A repetition whose
tab records that error is host interference, not the page's result: the cell reruns it, at most
`OPEN.interferenceReruns` times, and a repetition that still meets the error at the bound fails the run. Each rerun
is recorded in the repetition's `interferenceReruns` with its cause, so no interfered repetition enters a median.
A stall of the bench host spoils a release repetition the same way. When the open navigates later than `OPEN.openByMs`
(8 s) into the hold, its pooled connection has been idle past Chrome's 10 s check, which the cell does not name, and the
cell reruns it under the same bound. In D2 (run 20261115T095852Z) the bench's own requests to the server came 1.4 s and
3.6 s late in the two B release repetitions. The second open navigated about 8.7 s into the hold and failed with
`net::ERR_TIMED_OUT` 40.3 s later. Every other load failure stays the repetition's record, as before.

**The newest-row target.** The target of a repetition is the newest row the For you view paints on the first screen:
the last message of the bootstrap whose role the view paints (`user`, `web_reply`, `web_reply_notice`, `draft`, or a
`system` row whose text starts with `Error: `) and that carries an id. Separators and the other roles have no node in
the view, so a first screen whose last message is a separator names an earlier row. The rule lives in
`newest-row.mjs`, beside the view's filter it copies (`web/static/js/chat/for-you.js`, `showsInForYou`). Each
repetition records its target as `newestTargetId`. A run that measures both sides passes the before side's JSON to the
after side (`--expect-targets`): a repetition whose target differs from the before side's stops the after side with
both ids, before it measures. Rows run on only one side still use the same target rule, but have no cross-side target
to compare. A single-side run has no expectation and checks nothing.

**The seed.** `run.sh` copies the mock home the run reads (`~/improve/mock-home/<version>-<shape>-<sessions>`) into
`<run dir>/seed/` and records its content hash in `<run dir>/seed.sha256`. Both sides read the copy. Each side checks
the copy's hash when it queues for the bench lock, and stops the run on a change. The shared home is the improve
trials' seed too, so a rewrite of it cannot move a run's targets.

**Stored-copy rows.** The shell cache is named for the release. The session-copy cache is named for its copy format,
so activation keeps copies that the new page can read and drops copies of an older format. A page that receives a reply
over its socket refreshes that session's copy. `release-offline` and `replied-offline` run on the after side only;
main has no offline-copy client, so it cannot produce a meaningful before-side result for those rows. The after-side
acceptance still requires every repetition to show the stored copy, including the reply delivered over the socket.

**Measured per repetition.** Times are ms from the tab's first navigation: the first document's time origin. A reload
inside the open stays part of it. `newest_message_ms` is when the newest-row target is first in the chat of any
document of the open, named by its `data-message-id` — the page's own `newest_message_ms` definition
(`web/features/diag/templates/diag/client-events.js`); a text marker goes stale, because the scenarios before the open
scenario send turns to the same session. `catchup_ms` is the first `catchup_complete` frame of the page's socket after
the navigation, and `load_ms` is the load event of the last document, the one whose load ends the open.
`bytes_to_newest` is the link's bytes (both directions) from the navigation to that moment, and `bytes_after_load` the
bytes in the 30 s after that load event; the proxy's counters are sampled every 25 ms. `dcl_ms`, `response_end_ms`,
`tail_ms`, `document_transfer_bytes` (the document's bytes on the wire) and `document_body_bytes` (its encoded body)
come from the last document's Navigation Timing, and `mark` is whether the not-current mark was in any document of the
open at any sample. The page reports through a binding that survives a reload, so each top-level document's reports
reach the bench (the page's frames do not report): `pageCase` and the record the open starts from (`recordAtStart`) are
the first document's, since the first document is the one that finds the browser's state. `reload_causes` lists each
reload the open made, one entry per reload, with the cause the reloading document reported (`stale_asset` when the
server's static asset version moved past the page's), or `null` when the document named none; the reload count is its
length. Each repetition also names what the tab and the page saw: `sockets` (the WebSocket opens), `frames` and
`frame_types` (the socket frames by type), `requests` (the requests), `ws_state` (the client's own socket state), the
last console lines and page errors, the requests the host's network change dropped (`network_changed`, empty on a clean
repetition), and the interference reruns, so a missing time can be read against what the page reported. A release
repetition records when its open navigated into the hold (`openInHoldMs` in the JSON).

**The mark's DOM contract.** A page marks a stored copy as not current with the attribute
`data-freshness="not-current"` on the chat column's mark element (`web/templates/index.html`; `sidebar/last-state.js`
sets the attribute while the messages are a copy). The bench watches for that element and reports the first time it
appears. A cell whose page shows only current messages reports the mark as absent.

**Page's own timings.** A page reports `newest_message_ms` and `current_ms` in its `page_load` event
(`web/features/diag/templates/diag/client-events.js`), with the open case, the content source and the render-blocking
files beside their same-origin timings. `dev/client-readback/readback.mjs` reads them from the server log.

**Gaps.** The CDN stand-in serves the webfont stylesheet empty, as the other scenarios do, so the font files it names
are not loaded. The bench's DOMContentLoaded and newest-message times are measured from outside the page, so they are
not compared with the page's own `page_load` numbers in the same run.

**Output.** The JSON's `open` section holds `cpu` (the rate and its inputs), `reference`, and `cells`: each cell's
`reps`, one object per repetition with the fields above. The table prints one row per cell: the median newest-message
time, the five values in repetition order, the catch-up median, the bytes to the newest message and after the load, how
many repetitions reached the newest message, and the page reloads across its repetitions.

## Search view (`--scenarios search`)

The search view types each query of the sidebar's search box at the bench link and records the answer to each. It is a
named scenario: the default run does not include it, and it takes its own repetitions (three per query), not
`--repetitions`. It runs two variants, each repetition in a fresh browser profile behind a fresh proxy:

- **idle**: the box is cleared and the query typed, and nothing else changes on the server. Recorded per query: `e2e_ms`,
  the answer's paint minus its input event on the page's clock; `body_bytes`, the answer's `encodedBodySize`, the body
  compressed as it crosses the link; `wire_bytes`, its `transferSize`, headers included; and `overlap`, the other
  requests in flight from the search request's start to the answer's response end, by count and `transferSize`, with
  the status polls counted apart. A request still in flight when the answer is read is not in the resource log yet, so
  it is not counted.
- **broadcast**: as idle, and after each query's first paint one window of `broadcast.windowMs` runs. The bench renames
  its background session at `k × windowMs / changesPerWindow` for k = 0 … 18, so the server broadcasts 19 changes in the
  window. Each broadcast makes the page call `handleSidebarSearch` for the open query (`refetchSessionList` in
  `web/static/js/websocket-handlers.js`), and an answer whose generation a newer refetch has overtaken is discarded
  unpainted (`web/static/js/sidebar/filters.js`). The window's accounting span runs from the first paint until the page
  is idle and `windowSettleMs` has passed. Its fields:
  - `task_tree_changed_frames`: the frames the bench's socket proxy received in the window; `renames`: the renames sent.
  - `refetch_calls`: the page's search refetches that began in the span; `search_fetches`: the search requests that began
    in it.
  - `answers`: the search responses that ended in the span, and `answer_wire_bytes`, their `transferSize`. A response
    the server answers `304` (unchanged body, `rust/crates/app/server/src/etag.rs`) has its headers as its `transferSize`
    and its body from the browser's cache, so the wire bytes are the `transferSize`, not the `encodedBodySize`.
  - `answer_paints`: the search renders in the span that painted an answer. A repaint redraws the list's last paint with
    the same `sessions` object, so it is counted as `repaints` and not as a paint.
  - `discarded` = `answers − answer_paints`: the responses that ended and did not paint. The run stops if the paints
    exceed the answers, since every paint belongs to an answer that ended.
  - `status_polls`, `status_bytes`, `bytes_by_path`: the requests that began in the span, the status polls counted apart.

The view reads each request's resource entry from an observer's log (`window.__resources`), not from the browser's
resource timing buffer. The page's diagnostics clear that buffer every 60 s (`TRAFFIC_FLUSH_MS` in
`web/features/diag/templates/diag/client-events.js`), so a buffer read misses the answers that ended before a clear.

The constants are `SEARCH` in `profile.mjs`. Each row states its source:

| Constant | Value | Source |
|---|---|---|
| `queries` | sidebar, plan, cron, hydeogen | the queries the search view types |
| `repetitions` | 3 | per query and variant |
| `clearSettleMs`, `loadSettleMs` | 2500, 4000 | the waits browser_search.js takes between searches and after the load |
| `windowSettleMs` | 1000 | the view's own wait: the last answer's render and paint follow its response's end, and the drain's idle check does not wait for them |
| `liveBeforeBodyBytes` | sidebar 116645.5, plan 66574, cron 60003, hydeogen 1887 | the median `encodedBodySize` of each query's search answer in the live before captures on the live release e8d6bc82, in `~/scripts/20261119_sidebar_search_slow/`: sidebar 10 runs (`browser_fast.json` 5, `browser_slow.json` 2, `browser_slow2.json` 3), plan 7 (`browser_fast.json` 5, `browser_slow.json` 2), cron 3 (`browser_slow2.json`), hydeogen 9 (`browser_fast.json` 5, `browser_slow.json` 2, `browser_slow_hydeogen.json` 2) |
| `bodyToleranceShare` | 0.1 | the view's acceptance: a checked query's median body lies within 10 % of its live body |
| `checkedQueries` | sidebar, plan, cron | hydeogen is reported without a check |
| `broadcast` | 19 changes, 60004 ms | `watch_ws.json`: 19 `task_tree_changed` frames in a 60.004 s window |

The live numbers are the bench's own capture method (`encodedBodySize` from Resource Timing). A cross-check with the
curl capture of the same release (`plan/shapes_run2.json`, `before_wire_gzip`: sidebar 116265, plan 64647, cron 63140)
lies within the same 10 % for each checked query.

**How the mock rows produce the match sets.** A run that names `search` takes the search home (`<data version>-search-<sessions>`
under `~/improve/mock-home/`), the view home plus the match sets of `SEARCH_MATCH_SETS` in `rust/crates/app/mock-home/src/generate.rs`.
The sets are archived sessions, so the active list the bench counts is unchanged: 124 rows, as the view. Each set's
sessions are named `<query> match NNN` and stamped 30 days after the view's own sessions, so the answer's newest-200 cap
(`SEARCH_RESULT_LIMIT`, 200 rows) takes the set and no view session:

| Query | Sessions | Goal characters per session |
|---|---|---|
| sidebar | 200 | 229 |
| plan | 200 | 119 |
| cron | 200 | 105 |
| hydeogen | 2 | 250 |

A search row's size is its metadata, and the page's own `task` spec does not reach it, so the row's large field is the
session's `web_status` goal. The goal is a run of seeded Han characters; each takes three bytes, and gzip keeps about
2.3 bytes of each per row, so the character count sets the answer's compressed body. sidebar, plan and cron were tuned on
a trial serving the search home: their gzip wire bytes came to 117226, 66948 and 60474, against the live medians above.
hydeogen is reported only: its count, 250, was estimated from a trial at 290 characters (2187 bytes), and the run's
hydeogen row is its measure. The run's own JSON has the measured medians and the check, and the `check` column of the
table prints the same comparison.

**Output.** `side.search` in the side JSON holds `records` (one per query, variant and repetition), `summary` (medians over
the repetitions, the body check, and for broadcast the discards, renames, frames, search fetches and status bytes of the
windows), and the
constants the run measured against. The run prints one table, `search view:`, after the side's table.

The search view's tests are `search.test.mjs`: the window's offsets (19 renames, all inside the window), the e2e and overlap
sums, the body check at its boundary, the summary's medians and discard totals, the discard guard, and the page scripts'
accounting (repaints apart from paints, and the span's counts) run in a sandbox whose globals stand in for the page's.

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

## Perf loop

The perf direction runs a named scenario set on both sides when the round's change can move it. `--side both` prints
each side's verdict and the cross-side regression line, and exits 4 when a scenario that passed on the before side
fails on the after side. The caller passes a scenario set and a repetition count; the bench takes its own lock, shared
with the other bench runs on the host. Each side's trial tree goes under `$TMPDIR` when the caller sets it, so a round
inside a writable sandbox keeps the trees in its scratch; Chrome keeps its own temp files there too.

## Tests

The relay tests are integration tests because they start the relay as a child process. The Integration workflow runs
them when a change touches this directory. To reproduce them through the remote runner:

```sh
dev/remote-run/run.sh env POOR_NETWORK_CHROME=/path/to/chrome node --test \
  dev/poor-network/proxy.integration.test.mjs dev/poor-network/report.test.mjs \
  dev/poor-network/front.integration.test.mjs dev/poor-network/chrome.test.mjs \
  dev/poor-network/scenarios.test.mjs dev/poor-network/tabs.test.mjs dev/poor-network/open.test.mjs \
  dev/poor-network/lock.test.mjs dev/poor-network/search.test.mjs
```

They check the relay's tunnel transfer time at the link, its stop on SIGTERM, and its refusal of a target without a
port; a blackout drops a connection attempt without answering it at the restore, keeps an established tunnel's bytes for
the restore, and resets the tunnels in reset mode. Two tests drive the shaping proxy itself: a route reaches its own
server over the same link while the proxy refuses every other authority, and a scheduled reset window resets the open
tunnels when it starts. The front tests read the TLS record sizes the front writes and check them against Go's. The
report's tests check the calibration gate: it reads the during-refresh holds, not the scored scenario's random-phase
holds, and only the reference before side's line carries a verdict. The chrome tests check the browser teardown: a
closed browser fails its unanswered DevTools calls and drops its pipe handles. The scenario tests check the status
counting against synthetic poll records of both answer shapes: the baseline's whole row map and the change's `{token,
rows, gone}`. The tab tests check the switch's row handling against a stub sidebar that keeps rows back: a row absent
from the document or hidden in it is clicked only after its group's Show all paints it, and a row that no toggle reaches
fails the step. The open tests check the CPU rate's sharing (a shared rate stands, beside the one the side's own
reference derives), the reference's document (a reference repetition whose document crossed the link gives no rate) and
the host interference (a dropped request, a failed navigation or a release open the host pushed past `OPEN.openByMs`
names the interference; any other load failure stays the repetition's own record; a repetition the host spoiled is
rerun, bounded, and its reruns recorded). The open record's tests check that a reload inside an open keeps the first
navigation as its start, that the newest message counts at the first document that shows it, that the case and its
starting record come from the first document, and that each reload is recorded with its cause. One of them drives a
Chrome against a page that reloads once and holds an iframe, through a forward proxy: the cause reaches the tab from the
reloaded document, and the frame adds no document. The lock tests check that one concurrent side attempt holds the bench
lock and the next waits for it, and that the bench lock's file name and `flock` call live only in `lock.sh`. They also
check that an improve trial side stops with the bench lock named when the lock stays held, runs its command when the
lock is free, and passes through a command exit of 75 without reporting a lock timeout. The search tests check the
search view's arithmetic (see "Search view").
