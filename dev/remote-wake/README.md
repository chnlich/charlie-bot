# Remote-wake bench

Two CharlieBot builds serve side by side on one host against one workload of remote waits, and the bench reports how
long each wake took after its job ended. The two builds are a "before" commit and an "after" commit. The bench's
ground truth is the end time each job writes inside a container that acts as the remote host, so the measurement does
not depend on the server code under test. Each wake also gets two counts: the ssh processes its build logged, and the
authenticated connections the container's sshd accepted.

The workload is one seeded list both builds run: the same kinds, the same job lengths, the same start times and the
same server restarts. The kind mix (71 remote pid, 4 `--cmd`, 25 Slurm) and the job lengths come from the 104
completed remote wakes of production window W2 (2026-11-01 to 2026-11-08); the lengths are the plain integers in
`w2-waits.txt`. The probe schedule that the planner and the report both read is in `probe-schedule.mjs`.

## Run

The bench builds the two commits, starts the container and runs the trials, so run it on a build host through the
repository's remote runner. Fill the placeholders from the host you run on:

```sh
dev/remote-run/run.sh bash dev/remote-wake/run.sh \
  --before <before revision> \
  --after <after revision> \
  --source-repo <git clone that holds both commits> \
  --fetch-from <git URL for a commit the clone lacks> \
  --out-dir <directory for run results> \
  --detach
```

A smoke run checks the gates in minutes instead of hours. This one runs ten waits, one restart and both fault cases:

```sh
dev/remote-run/run.sh bash dev/remote-wake/run.sh \
  --before <revision> --after <revision> \
  --source-repo <git clone that holds the revision> --fetch-from <git URL> \
  --out-dir <directory for run results> \
  --waits 10 --seed 20261112 --stagger-seconds 120 --restarts 1 --faults cut,hold \
  --cut-seconds 60 --hold-seconds 120 --length-cap 300 --gate smoke --detach
```

The default seed's ten-wait plan takes no restart: a ten-wait plan crosses 20% of its waits only with exactly two, and
the default seed has no such boundary. The smoke names seed 20261112, the first seed after the default whose plan takes
one restart with both faults inside a running job.

`--detach` starts the work as a transient systemd user unit and returns at once, because a full run takes hours. The
unit writes `<out-dir>/remote-wake-<id>.log`, `--out-dir/.last-run` names the run directory as soon as it exists, and
`--out-dir/.done` receives `pass` or `fail` at the end.

| Option | Meaning |
|---|---|
| `--before <rev>` | The before commit, resolved in `--source-repo` (default `origin/main`). |
| `--after <rev>` | The after commit (default the current tree's `HEAD`). When both name the same commit, one build serves both sides. |
| `--source-repo <dir>` | The clone that holds the commits (default the current tree). |
| `--fetch-from <url>` | A git URL that supplies a commit the source clone lacks. |
| `--out-dir <dir>` | Where the run directory goes, outside the tree. Required. |
| `--waits <n>` | Waits per build (default 100). |
| `--seed <n>` | The workload seed (default 20261108). One seed reproduces one workload. |
| `--stagger-seconds <n>` | Spread of the job starts (default 3600). |
| `--restarts <n>` | Server restarts (default 3; the planner stops early when the crossed share would leave 20-25%). |
| `--faults <list>` | `cut`, `hold`, both, or `none` (default `cut,hold`). |
| `--cut-seconds <n>` | Length of the connection cut (default 60). |
| `--hold-seconds <n>` | Length of the new-connection hold (default 300). |
| `--rtt-ms <n>` | netem round-trip delay (default 309; 708 is the other measured production value). |
| `--length-cap <n>` | Longest sampled job length (default 7200; the smoke run uses 300). |
| `--mock-sessions <n>` | Sessions in each build's generated mock home (default 4). |
| `--image <name>` | Container image (default is built from `container/`). |
| `--gate smoke\|full` | The report's gate (default `full`). |
| `--detach` | Start as a transient systemd user unit and return. |

The host needs docker with `NET_ADMIN` containers, cargo, node, git, and a systemd user manager. The container image
installs `openssh-server`, `iproute2` and `iptables` from Ubuntu's archive.

## What one run does

1. Builds each side from a scratch clone of its commit with the host's cargo, in that side's own target directory.
2. Takes the bench lock (`lock.sh`, `$HOME/remote-wake-runs/.bench.lock`). The lock covers the container and the
   measured segments, so two bench runs never measure side by side.
3. Starts one container from `container/`: an sshd on a published loopback port, a `tc netem` delay of `--rtt-ms`, a
   stub `sacct` that answers the trigger code's query from a per-user job table, and a job script that writes its own
   end time. Each build gets its own user and key, so the sshd log attributes every authenticated connection to one
   build.
4. Generates each build's own mock home with `mock-home generate`. The trial copies it into an isolated trial tree; no
   real session directory is involved.
5. Starts each build's server through `scripts/release/trial.sh --serve` on its own port range (18800-18849 for
   before, 18850-18899 for after) and its own short trial tree. The scripted backend first on `PATH` makes a fired
   wake start a turn with no model call.
6. Runs the workload segment by segment. A segment is the stretch between two restarts. At each wait's scheduled
   offset the bench starts the job inside the container and registers the trigger on that build's server. A segment
   ends by copying its trigger records, its server log and its session directories out of the trial tree, and the
   next segment's trial seeds from those directories, so pending triggers recover after the restart exactly as they do
   in production.
7. Applies the fault cases at the plan's single fault window, after every registration and after the last restart:
   a new-connection hold (an `INPUT` rule that drops `SYN` to port 22 for `--hold-seconds`, so established connections
   survive it) and a connection cut (an `INPUT` rule that rejects every packet to port 22 with a TCP reset for
   `--cut-seconds`, which ends the established connections). The server's ssh keeps one ControlMaster connection per
   build, so after a cut it must dial again, and the hold makes that dial wait. The before side applies both, because
   the container's firewall is shared.
8. After the last segment, reads every job's state and end record from the container, and the sshd log's
   `Accepted publickey` lines. Both are the ground truth for the report.

## Results

`<out-dir>/run-<UTC>/` holds:

| File | Content |
|---|---|
| `plan.json` | The workload both builds ran, with the planned restart share. |
| `rows.tsv` | One row per wake: build, wait, kind, length, crossed restart, fire reason, lag, fault allowance, restart offset, the two bounds, the two within-bound flags, faulted, ssh processes, authenticated connections. |
| `summary.json` | The per-build lag statistics, the measured restart share, the parity check and the gate's verdict. |
| `report.txt` | The summary as printed. |
| `registrations-<build>.jsonl` | Each registration's trigger id, the server's creation time and the job it watches. |
| `snapshots/<build>-<segment>/triggers.json` | The trigger records that segment's server wrote. |
| `logs/<build>-<segment>-server-*.log` | The segment's server log. |
| `ground-truth.json` | Each job's own end time and the sshd's authenticated connections. |
| `trial-logs/<build>-<segment>.log` | The serve trial's own output. |

### Lag and bound

A wake's lag is `fired_at - max(job end, trigger creation)`, both on the server's clock (the container shares the host's
clock). The job end comes from the container, not from the server.

The bound is what today's probe schedule allows, measured from the schedule's start. The schedule starts at the
trigger's registration, or at the last server restart after it. The bound adds up these parts:

- one probe interval in force at the fire, counted from the schedule's start;
- the probe noise, 10 s;
- the fault allowance: the length of the union of the fault windows that overlap the wake, so two faults that start
  together count once, at the longer one's length;
- the restart offset: the time from `max(job end, registration)` to the schedule's start. The schedule does not run
  before a restart, so the wait for the restart is part of the bound, and it is counted once.

`within-interval` counts the wakes inside the bound without the restart offset. `within-bound` counts the wakes inside
the full bound, and the gate uses it.

### Restart share

A wait is crossed when a restart falls between its registration and its fire. The planner predicts each fire from
today's probe schedule (the first probe at or after the job's end, with the mean probe noise) and targets 20-25% of
waits crossed. The summary reports each build's measured share, from the rows whose `crossed_restart` is set, next to
the planned share. The smoke gate does not check the share; the 100-wait run reports it.

The summary reports mean, median, p90 and max lag overall, by length bin (0-70, 70-310, 310-630, over 630 s) and by
restart crossing, for each build. Parity checks that every wait the before build wakes with reason `completed` is also
woken `completed` by the after build.

## Gates

Both gates also fail a completed wake whose job wrote no end record, and a wake that fired before `max(job end, registration)`.

- `--gate smoke`: every wait fires `completed` in both builds, parity holds, and every wake lies inside its bound. No
  wake is exempt: a faulted wake gets its fault allowance instead. The smoke run uses `--waits 10 --stagger-seconds 120
  --restarts 1 --length-cap 300 --hold-seconds 120`, so its lengths never exceed 300 s and its fault window fits
  inside the run.
- `--gate full`: every wait fires `completed`, parity holds, and the after build's mean lag is at most 17 s.

Exit codes: 0 the gate passed; 1 a build, trial or measurement failed; 2 a precondition failed and nothing started;
3 the gate failed; 75 the bench lock was not free within its two-hour wait.

## Limits

- The netem delay delays the container's egress, so a round trip costs about the configured delay rather than twice a
  one-way delay. The default and 708 ms are the two production round-trip times the plan names.
- The authenticated connections column counts new connections that reach authentication: one `Accepted publickey`
  line of the container's sshd log per connection. A connection that a fault holds (its `SYN` dropped) or cuts before
  it authenticates has no such line, so it is not counted. A row counts the connections the container saw between that
  wait's registration request and its fire. Overlapping waits of one build share those connections, so a single row's
  count is not exclusive to it. The summary's per-build total counts each connection once, from the sshd log.
- A build that predates the remote-end ruler logs no `remote_ssh_started` lines and no `End` column. The bench's lag
  never reads either: it reads the job's own end record. The ssh process count is reported as `not-logged` for such a
  build.
- A 100-wait paired run takes about three hours: an hour of staggered starts, job lengths capped at 7,200 s, and the
  settle time. A run started with `--detach` survives the ssh that started it.
