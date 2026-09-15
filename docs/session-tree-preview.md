# Session tree preview

`charliebot session-tree preview --home DIR --port PORT` starts one isolated,
interactive trial instance of the real application (plan 1 v4: the independent
instance carries the session-tree trial; real migration waits for an explicit
request). It is the reviewed way to try the task-tree UI with real execution
before any production cutover — existing session data is never imported.

## Invocation

```bash
# First start (initial setup): the backend comes from the current profile's config
charliebot session-tree preview --home /path/to/preview-home --port 18598 --backend <backend-id>

# Restart the same instance (config and tasks are preserved; --backend optional)
charliebot session-tree preview --home /path/to/preview-home --port 18598
```

Both `--home` and `--port` are required. The command runs the service in the
foreground; Ctrl-C stops it. When the instance is ready it prints the URL, the
actual home, the source branch and the full source SHA — never a secret or a
provider endpoint. The instance's browser access key lives in
`<home>/credentials.yaml` (`charliebot.access_key`); enter it on the login page.

Because the login cookie's name is shared across ports, open the trial in a
separate browser profile or a private window — a production tab's cookie would
otherwise collide.

The selected backend must be a `charlie-code` entry of the current profile's
`config.yaml`; the preview reads only that entry and its referenced provider
credential, writes them into the preview home's private config, and routes
every native session to `<home>/clc-sessions` through the CLI's own
`--session-dir` override. Other backends refuse until their native isolation is
proven.

## What the entry point guarantees

- **Home boundary.** `--home` must resolve (symlinks included) outside and
  nonoverlapping with the production home, the production workspace dirs, and
  the running checkout. A fresh path is seeded with minimal private config and
  its own random access key; an existing validated preview home keeps its
  config and user-created tasks across restarts. Legacy/migrated state,
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
  shutdown failure, so a concurrent migration apply or second instance refuses
  with the holder's identity.
- **Readiness record.** `<home>/state/preview_instance.json` names the serving
  identity (pid, /proc start marker), the URL, the backend, the source branch
  and SHA, and the readiness state. Logs land in `<home>/logs/`.

## What it does not do

- **No import.** No session, thread, memory, schedule, trigger, or native
  session data is copied from the production home; the trial starts empty.
- **No production claim.** Starting the preview never stops, restarts, or
  reconfigures the production runtime, its routing, or its shared services.
- **No migration.** The `session-tree migrate` verbs are separate; a preview
  home is never a migration source or target.
