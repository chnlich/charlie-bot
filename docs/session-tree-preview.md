# Session tree preview

`charliebot session-tree preview --home DIR --port PORT` starts one isolated,
interactive trial instance of the real application. It is the reviewed way to
try the task-tree UI with real execution before any production deploy —
existing session data is never imported: legacy sessions are read in place by
the new code, never converted.

## Invocation

```bash
# First start (initial setup): the backend comes from the current profile's config
charliebot session-tree preview --home /path/to/preview-home --port 18598 --backend <backend-id>

# First start with more than one explicitly selected model: --backend names the
# default, every further entry arrives as a repeatable --add-backend
charliebot session-tree preview --home /path/to/preview-home --port 18598 \
    --backend <backend-id> --add-backend <second-id> --add-backend <third-id>

# Add further selected entries to an existing preview home (validated from the
# source profile first, applied under the home writer fence)
charliebot session-tree preview --home /path/to/preview-home --port 18598 \
    --add-backend <another-id>

# Restart the same instance (config, catalog and tasks are preserved; --backend
# optional and must match the home's stored default when given)
charliebot session-tree preview --home /path/to/preview-home --port 18598
```

Both `--home` and `--port` are required. The command runs the service in the
foreground; Ctrl-C stops it. When the instance is ready it prints the URL, the
actual home, the source branch and the full source SHA — never a secret or a
provider endpoint. The instance's browser access key lives in
`<home>/credentials.yaml` (`charliebot.access_key`); enter it on the login page.

`--add-backend` entries must all be `charlie-code` entries of the current
profile, must not already be in the home's catalog, and must reference
resolvable settings and credentials; every requested entry validates before
anything is written. A restart without `--add-backend` keeps the stored
catalog, credentials, default, tasks, native history, paths and access key
exactly as they are. The home's configured default (its first option and the
only `backends.preference` entry) never moves, so no Run falls over to another
model silently; the additional entries are explicit choices only.

Because the login cookie's name is shared across ports, open the trial in a
separate browser profile or a private window — a production tab's cookie would
otherwise collide.

The selected backends must be `charlie-code` entries of the current profile's
`config.yaml`; the preview reads only those entries and their referenced
provider credentials, writes them into the preview home's private config, and
routes every native session to `<home>/clc-sessions` through the CLI's own
`--session-dir` override. Other backend families refuse until their native
isolation is proven. A live fence holder (the running instance itself) refuses
the whole launch, additions included, as one structured diagnostic.

## What the entry point guarantees

- **Home boundary.** `--home` must resolve (symlinks included) outside and
  nonoverlapping with the production home, the production workspace dirs, and
  the running checkout. A fresh path is seeded with minimal private config and
  its own random access key; an existing validated preview home keeps its
  config and user-created tasks across restarts. Existing legacy state,
  unrelated configurations, overlapping paths, occupied ports, and unprovable
  instance ownership refuse before any write.
- **Environment selection.** The process switches `CHARLIEBOT_HOME` and clears
  inherited CharlieBot session/Run/credential identities before any cached
  config or singleton can bind to the old home. `HOME` and `CODEX_HOME` are
  never repurposed. Workspace discovery and worker worktrees are scoped to
  `<home>/workspaces` and `<home>/worktrees`, and the launch workspace boundary
  refuses any repo outside them.
- **The real app, scoped lifetime.** The shipped UI, task APIs and websockets
  serve the trial. The preview lifetime recovers only this instance's own task
  nodes and Runs; the scheduler, external trigger recovery, external messaging,
  global cgroup/worktree cleanup, and the other shared provisioners never
  start, and their mutation routes (scheduled tasks, Slack send/reply,
  delayed-trigger creation, the host-global terminal) refuse at the request
  boundary.
- **Writer fence.** The preview holds the normal home writer fence
  (`state/home_writer.lock`) for its whole run and releases it on startup or
  shutdown failure, so a second instance refuses with the holder's identity.
- **Readiness record.** `<home>/state/preview_instance.json` names the serving
  identity (pid, /proc start marker), the URL, the backend, the source branch
  and SHA, and the readiness state. Logs land in `<home>/logs/`.

## What it does not do

- **No import.** No session, thread, memory, schedule, trigger, or native
  session data is copied from the production home; the trial starts empty.
- **No production claim.** Starting the preview never stops, restarts, or
  reconfigures the production runtime, its routing, or its shared services.
- **No rewriting of existing homes.** A preview home is never a source or
  target that touches the production home's sessions; legacy sessions are read
  in place by the new code, never converted.

## Automated trials over a preview

Two repo-owned harnesses drive a preview instance end to end; both are
executable verification recipes, never mocks:

- `scripts/live_preview_task_tree.py` — the live execution trial: a root
  manager's real takeoff turn, a repo-less quick-edit worker, a synthetic-repo
  implement worker with its auto-spawned review, and the workspace boundary.
- `scripts/live_preview_sidebar_status.py` — the sidebar-status trial: real
  Chrome over CDP against the same kind of fresh preview home, asserting the
  sidebar's live work states through `/api/sessions/status` and the
  `GET /api/sessions/` list the sidebar paints from, the DOM icons and
  screenshots — a ~60 s worker Run (spinner on the row, gear on the collapsed
  parent, expanded parent showing only its own state, icons clearing after
  finish), a launch failure before process start (red alert on the row and the
  collapsed parent, the parent's failure report naming the error, the
  worker transcript's Run header reading `failed` with the error beneath it
  and the Run's own `ended_at` as its time, and a held-back retry's header
  reading `queued` with no time at all), a queued Run
  held by a paused node (the clock), goal-derived row names (never a raw
  Markdown heading in any worker-facing title), and list rows that already
  carry each task-tree node's `work_state` on first paint.

Both share the preview's isolation guarantees: the trial home is a fresh
temporary directory, the port a free one (the production port 18498 is
refused explicitly), the harness env is scrubbed of production identity
variables, the production service is never started, stopped, restarted or
contacted, the production homes (`~/.charliebot`,
`~/.charliebot-session-task-tree`) are never written, and an independent
sentinel home plus a host native-store snapshot prove nothing outside the
trial changed. Evidence (screenshots, assertion JSON, the tested commit) goes
to `--evidence-dir`, never into git.
