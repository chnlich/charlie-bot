# Release lab

The release lab runs CharlieBot's release flow against a stand-in production server and prints one JSON line per run.
It measures the flow from the start of `scripts/release.sh` to the `server_ready` of the server the switch starts, so a
change to the flow, the start or the disk gate can be compared on one machine before it reaches a live host.

## The stand-in production server

The stand-in home is the task-tree mock home that `scripts/release/startup-lab.sh generate` writes (seed 1, a live
process for each live run), so no real session directory is copied. The `--from` release serves it from a tmux pane on
the lab's own socket and port, and the flow's scripts address it through environment overrides, as
`scripts/release/switch-rehearsal.sh` addresses its own home: `CHARLIEBOT_HOME`, `CHARLIEBOT_RELEASES_DIR`,
`CHARLIEBOT_RELEASE_LOG_DIR`, `CHARLIEBOT_LOG_DIR` and `CHARLIEBOT_TMUX_SOCKET`. The trial's live-server probe reads
`127.0.0.1:18498` unless `CHARLIEBOT_TRIAL_LIVE_URL` names another server; the lab sets it to the stand-in.

Everything the lab writes lives under `RELEASE_LAB_DIR`. The lab never reads or writes a live CharlieBot home, release
root or tmux server, and it opens no port another host's server uses.

## Host values

No value in this directory names a host, a user or one machine's path. The lab takes them as environment:

| Variable | Meaning |
|---|---|
| `RELEASE_LAB_DIR` | the lab's state directory, absolute |
| `RELEASE_LAB_REPO_URL` | the charlie-bot clone URL `--prepare` clones the lab's git origin from |
| `RELEASE_LAB_API_BASE` | the model endpoint the stand-in home's backend names |
| `RELEASE_LAB_CREDENTIAL` | the `credentials.yaml` section that holds that endpoint's key |
| `RELEASE_LAB_KEY_FILE` | a mode-600 file holding that key; `--teardown` deletes it |
| `RELEASE_LAB_PORT` | the stand-in server's port (default 18900) |
| `RELEASE_LAB_TMUX_SOCKET` | the lab's tmux socket name (default `release-lab`) |
| `RELEASE_LAB_SCOPE_MEMORY` | the lab's systemd user scope memory cap (default 24G) |
| `RELEASE_LAB_MODEL` | the stand-in backend's model (default `openai/glm-5.3-flash`) |
| `RELEASE_LAB_READER_SECONDS` | the cap on how long the flow's readers run from the gate's start (default 1800); they stop at the gate's first waiting sample |
| `RELEASE_LAB_READER_SLEEP_MS` | the pause between one reader's 1 MiB reads (default 12) |
| `RELEASE_LAB_KEEP` | keep a run's stand-in home and logs for inspection (default 0) |

The lab runs inside a systemd user scope with `RELEASE_LAB_SCOPE_MEMORY`, and it stops every process it started when it
ends, so a lab host's process count returns to where it was.

## Builds

The lab reads release binaries built on the lab host for that host's architecture; it never downloads a CI artifact.
`build.sh` builds the commit of the worktree it runs from through `dev/remote-run/` and installs the files:

```sh
dev/release-lab/build.sh <lab bin dir on the build host>
```

It writes `<lab bin dir>/release/` (the CI artifact's `charliebot-server`, `charliebot`, `release.log` and
`sha256sums.txt`), `<lab bin dir>/tools/` (`mock-home` and `startup-lab-launcher`) and `<lab bin dir>/COMMIT`. The lab's
download stage serves `release/` as the CI artifact and the JSON line marks that stage `"download_source":"lab"`.

## Run

```sh
dev/release-lab/run.sh --prepare --to <commit>
dev/release-lab/run.sh --from <commit> --to <commit> --flow-ref <commit> --runs 5 --readers 2
dev/release-lab/run.sh --mode starts --to <commit> --runs 3 --readers 0,1,4,8
dev/release-lab/run.sh --teardown
```

`--from` is the release the stand-in serves, `--to` the release the flow ships and `--flow-ref` the commit whose
`release.sh` and `switch-server.sh` run. The flow's trial and rehearsal commands are read from the checkout running the
lab (`trial-lab.sh` and `rehearsal-lab.sh` beside the flow's own scripts), so a lab override applies there without
touching the flow's tracked files.

Each flow attempt prints one JSON line with the stages' seconds (`download_s`, `units_s`, `trial_s`, `rehearsal_s`,
`install_s`, `preflight_s`, `gate_s`, `stop_s`, `start_s`), `ready_in_ms`, `read_bytes`, `release_to_live_s`,
`live_runs`, `reattached`, `verdict` (`live`, `switched_back`, `kept_reported`, `flow_stopped`, `start_failed` or
`switch_failed`) and the host's one-minute load at the start. `flow_stopped` and `start_failed` attempts are retried
until `--runs` runs reached the switch, up to `--runs + 2` attempts.

`--mode starts` prints one line per cold start of `--to` on the seed home: `ready_in_ms`, `read_bytes` and the reader
count. Its readers start when the trial creates the server log and stop at `server_ready`.

## Disk readers

The switch's disk gate samples the root disk's read rate over ten seconds and waits while the rate stays above 40 MB/s.
`readers.sh` starts the configured number of readers at a fixed point of each run (the gate's start for a flow run, the
server start for a start-matrix cell), each streaming the seed home's largest file through `O_DIRECT` in 1 MiB blocks,
so no reader file of its own is written. A flow run's readers stop at the gate's first waiting sample: the gate reads
its two process tables before it samples the disk, so the readers must cover that stretch for the sample to be theirs.
The JSON line's `gate_wait_samples` records how many samples the gate waited.
