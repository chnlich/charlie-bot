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
| `--scenarios LIST` | Any of `background,held,reset,stream` (default all). |
| `--assets-dir DIR` | Where the CDN copies are kept (default `DIR/assets`). |
| `--detach` | Start a transient systemd user unit and return; the log is `DIR/pn-<id>.log`. |

Exit codes: 0 both sides measured; 1 a measurement or build failed (the side's JSON still names the error); 2 a
precondition failed and nothing started; 3 the baseline calibration did not reproduce.

Chrome comes from `POOR_NETWORK_CHROME`, or else the newest `~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`.
The trial replaces `HOME`, so the discovery happens in `run.sh`.

Output: `DIR/<run id>/before.json` and `after.json`, one per side, with the link, the targets, the Chrome version, the
asset hashes, the session ids and every repetition's raw numbers. The summary table prints to stdout.

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

## Capped steps

A switch or a send in scenarios 2 and 3 that has not rendered within 180 s is recorded at 180 s, and the report counts it
in the `at cap` column. Its time is then a lower bound: the step took at least that long. The record keeps the page's
state at the cap (the sockets, the frames seen, and whether the target's bootstrap finished), so the reader can see why.
When the target's bootstrap never finished, the step's bytes are the size of the same body on the wire, read from the
server, so the floor still follows the bytes the step needs.

## Scenarios 4 to 6 have no background

Scenarios 1 to 3 share one repetition: the background load runs, the proxy counts its bytes, and the switch and the
send happen inside that background. The targets of scenarios 4 to 6 do not mention a background load, so those
repetitions run without one.

## Calibration

The baseline must reproduce the switch hold: on origin/main, a switch during the background must take at least
`CALIBRATION_HOLD_MS` (10 s) to render. If it does not, the proxy does not model the link, and `--side both` stops before
the after side. The table prints the calibration line.

## Sessions and the server

The trial server runs on the mock home. Its sessions carry no task boundary, so the server refuses a message to them.
The bench therefore creates the sessions it uses through `POST /api/sessions/`, on the trial's backend option. Each one
gets one seeded turn, which leaves one reply of about 8 KB in its history. The seed text is unique to its session, and
the page shows the switch once that session's own message renders. The switch floor adjusts for the bootstrap bytes.

The trial's backend is `charlie-code`, which `run.sh` puts on `PATH` as `stub/charlie-code`. The stub prints the
charlie-code NDJSON of a repository text (`reply.mjs`). It calls no model and reads no credential. A prompt carrying the
marker `bench-hold` keeps its turn open for the background load, until the bench cancels it.

The web client reads its access key from `localStorage` and sends it as the `Authorization` header and as the `token`
of each WebSocket URL. The bench writes the same `localStorage` entry before each page loads, so the page behaves as
after a login.

## CDN assets

The page loads scripts and styles from CDNs (jsDelivr, cdnjs, Google Fonts). `assets.mjs` downloads a copy of each URL
once into the assets directory, and records its SHA-256 in the JSON. One URL is unpinned (`marked`), so a changed copy
shows as a changed hash. Chrome serves those copies locally through the
DevTools Fetch domain. Any other external host fails at once, so nothing outside the trial reaches the link.

The gap: those CDN bytes do not cross the shaped link, so the bench does not count them. The bench measures the
application's own traffic.

## Host requirements

Linux with user systemd, `node` 20 or later, `cargo`, `git`, `openssl`, `flock`, and a Chrome build. The trial needs a
16 GB memory scope and about 6 GB of free space in `/tmp` for its copy of the mock home. Each run deletes its trial
tree when it ends; a run that is killed leaves the tree, which the next person removes by hand.

## Tests

The proxy's tests run through the remote runner:

```sh
dev/remote-run/run.sh node --test dev/poor-network/proxy.test.mjs
```

They check the transfer times within tolerance, two concurrent tunnels sharing one budget, the held and reset
blackouts, and the byte counters.
