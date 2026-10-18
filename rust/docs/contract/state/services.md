# Server process: startup, lifespan, services, session cgroups

Baseline: the Python tree at `d9fdcd7c9`. Entry point: `server.py`. Paths use `<home>` and `/home/<user>` as in
`files.md`. **UNVERIFIED** marks a statement not confirmed from source, with the reason.

## 1. Before the event loop (`server.py::main`)

Steps, in order. Any exception stops the process before it binds a port.

1. `agent_environment.apply_agent_environment()` rewrites `os.environ` once for every later child (`env.md`
   section 3). It raises `RuntimeError("the CharlieBot server must run from a venv; sys.prefix <p> is the base
   interpreter")` outside a venv.
2. `config.get_config()` runs the full `load_config` pipeline (`config.md` section 2).
3. `config.require_backends(cfg)`.
4. `v1_sessions.require_no_v1_sessions(cfg)`.
5. Each `wiring.startup_checks()` in registration order: `src.features.cron.backend_refs.check_backend_refs`, then
   `src.features.usage.backend_ids.check_backend_ids`.
6. `uvicorn.run("server:app", host=server.host, port=server.port, reload=False, loop="asyncio",
   log_level="warning", timeout_graceful_shutdown=5)`.

Importing `server.py` calls `src/app/registrations.py::register_all()` at module level, which imports every
package in `PACKAGES` and calls its `register()`. The route table is then read from `wiring.routers()`.

Logs go to stdout and stderr only; the server writes no log file. `scripts/start-server.sh` tees them to
`${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}/server_<%Y%m%d_%H%M%S>.log` and points `server-latest.log` at it.

## 2. Lifespan (`server.py::lifespan`)

| # | Step | Reads and writes | Failure |
| --- | --- | --- | --- |
| 1 | `config.get_config()` | `config.yaml` (cached) | raises |
| 2 | Home writer fence: `home_writer_fence.acquire_home_writer_fence(home, purpose="server startup")` | creates `<home>/state/home_writer.lock` (flock `LOCK_EX\|LOCK_NB`) and writes `<home>/state/writer_identity.json` | `HomeWriterActiveError` logs `server_startup_refused_home_writer_fence` and re-raises; startup fails |
| 3 | `buildinfo.init_build_info()` | runs `git rev-parse --short HEAD` in the repo root (sha `"unknown"` on failure) and records the UTC start time; served by `GET /api/internal/version` | never raises |
| 4 | `await init_seed.init_charliebot_home()` | mkdirs `<home>`, `sessions/`, `config.d/`; seeds `config.yaml` and `credentials.yaml` when missing (`files.md` 5.1); logs `charliebot_home_ready` | raises |
| 5 | `process.log_session_cgroup_startup(memory, swap, tasks)` then `process.sweep_stale_session_cgroups()` | logs whether session cgroups are on; `rmdir`s every **empty** `charliebot-sess-*` directory under the user's `app.slice` (section 4) | sweep errors are logged, never raised |
| 6 | `app.state.recovery_task = create_task(_run_task_recovery(cfg, task_manager))` | crash recovery (section 3), concurrent with step 7 | logged as `task_tree_recovery_failed`; startup continues |
| 7 | Start `early` services in registration order (`usage_tally`, `speech`) | section 5 | an exception from `start_service` fails startup |
| 8 | `await app.state.recovery_task` | waits until recovery ends, so no scheduler fire or recovered trigger races a recorded run | |
| 9 | `TriggerManager(cfg, task_manager)`, `triggers.set_trigger_manager`, `await trigger_mgr.recover_pending()` | reads every `sessions/*/triggers/*.json`; restarts a wait task for each `status: pending` file. Unparsable files log `trigger_recovery_load_failed` and are skipped. | per-file errors are logged |
| 10 | Start `ready` services in registration order (`scheduler`, `slack`, `discord`, `host_auth`, `merge_pool`, `ext_usage`) | section 5 | an exception fails startup |
| 11 | Log `server_ready` with `ready_in_ms`, then serve | | |
| 12 | Shutdown: every `stop_service` (ready services in reverse order, then early in reverse: `ext_usage`, `merge_pool`, `host_auth`, `discord`, `slack`, `scheduler`, `speech`, `usage_tally`), then `http.close_http_client`, then `streaming.streaming_manager.close_all` (closes WebSockets) | each step timed into `charliebot_shutdown` | an exception propagates and skips the `charliebot_shutdown` line |
| 13 | `finally`: `writer_fence.release()` | unlinks `writer_identity.json`, then unlocks | |

Agent processes are not stopped by the lifespan:

- Backends whose type sets `restart_reattach=True` write their raw output through file descriptors
  (`processes.md`), and a cancelled turn lets the child go (`AgentBackend.detach`) once its run record is
  persisted. The next server re-attaches it during crash recovery.
- `restart_reattach=False` types (antigravity, opencode) use piped children spawned with `PR_SET_PDEATHSIG`; they
  are terminated with the turn.

## 3. Crash recovery (`src/runtime/task_recovery.py::reconcile_task_tree`)

Scope: only directories under this home's `sessions/` that hold a `metadata.json`, and only this home's records.

1. Each registered sequence controller's `reconcile_interrupted(cfg, tree)` runs first: cron step chains and
   improve loops. A controller whose process died is never resumed. Its state file, active lock and chat stream are
   marked interrupted.
2. For every node, in sorted directory order, `task_execution._reconcile_node`:
   - Every non-terminal run converges. A durable stop request wins. A live `(pid, pid_start)` process is
     re-attached and followed from `agent.raw.cursor`. An ended process drains to its result. Queued work that
     never started re-enters through the normal launch checks.
   - Terminal runs replay any missing follow-up once, idempotently: the review chain, the sequence advance, the
     parent report, and the owner-close recheck.
   - Persisted but undelivered parent reports are delivered, and pending inputs dispatch with the deterministic
     `dispatch:` request id.
3. A node whose pass raises logs `task_recovery_node_failed`. A node that hit out-of-space is handed to the
   end-landing retry. Counters `{nodes, resumed, drained, followups}` are logged in `task_tree_recovery_done`.

The liveness rule and stable ids that make this idempotent are in `files.md` sections 3 and 6.4.

## 4. Session cgroups (`src/infra/process.py`)

- Base: `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/app.slice`, built from `os.getuid()` at
  import. It requires cgroup v2 with systemd user delegation.
- Directory per session: `charliebot-sess-<first 8 chars of the session id>`, created lazily by
  `ensure_session_cgroup` on the first spawn of that session. Each later spawn rewrites the limits from the current
  config:
  - `memory.max` = `session_memory_max_mb * 1024 * 1024`
  - `memory.swap.max` = `session_swap_max_mb * 1024 * 1024`
  - `pids.max` = `session_tasks_max`, or `max` when it is 0
- `session_memory_max_mb: 0` disables the whole feature: no directory, no task cap.
- An `OSError` on create or write degrades to "no cgroup" with one warning per failure shape
  (`session_cgroup_unavailable`); a spawn never fails for it.
- Joining:
  - raw-log spawns (no preexec, so the vfork fast path holds) write the child pid into `<cgroup>/cgroup.procs` from
    the parent right after exec (`AgentBackend._apply_turn_tree_limits`), together with a nice raise of 10;
  - spawns with a preexec (`make_session_cgroup_preexec`) write `"0"` to `cgroup.procs` in the child before exec;
  - opencode tool shells join through `BASH_ENV` pointing at `src/backends/opencode/cgroup_join.sh`
    (`echo $$ > "$CHARLIEBOT_SESSION_CGROUP/cgroup.procs" || exit 97`).
- Exit attribution: `memory.events` counters `max` and `oom_kill` are read before the spawn and after the exit. A
  SIGKILL exit whose `max` count grew is reported as a memory-cap kill (`classify_cgroup_exit`).
- Removal: at startup (step 5), every empty `charliebot-sess-*` directory is `rmdir`ed, and a non-empty one is kept
  with `session_cgroup_sweep_retained`. Permanent session deletion `rmdir`s that session's cgroup when it is empty.
- Cross-instance effects:
  - The sweep is not scoped to this home: it removes empty session cgroups of any CharlieBot instance run by the
    same uid, including a live server's. Those are recreated lazily on the next spawn.
  - Two homes whose session ids share the first 8 characters share one cgroup and its limits.

## 5. Registered services

`wiring.register_service(name, module, phase="early"|"ready")`. Each module defines
`async start_service(ctx: ServiceContext)` and `async stop_service()`. The list below is the full registry with
every baseline package registered, in registration order.

| Name | Module | Phase | Purpose | Cadence | Files and side effects | Writes outside the home |
| --- | --- | --- | --- | --- | --- | --- |
| `scheduler` | `src.features.cron.service` | ready | Runs `config.d/cron.d` tasks (`Scheduler`); published on `app.state.scheduler` for the run-now route | first tick 60 s after start, then every 60 s (`_TICK_INTERVAL`) | Reads cron files and prompt files. Auto-binds each valid task, enabled or not, to a manager node created with request id `scheduled-node:<name>`, then writes `session_id` back into the cron file (`cron_files.write_cron_key`). Updates the node's `last_scheduled_run`, `last_scheduled_cron` and `last_run_status`. Fires runs, appends `scheduled_run_*` chat events, and runs handlers (`backup`, `usage_ledger`, `cool_storage`). | the `backup` handler writes `<home name>_backup/`; `cool_storage` deletes backend transcripts (`files.md` section 7) |
| `slack` | `src.features.slack.service` | ready | Slack Socket Mode listener and backfill | long-lived connection | Starts only when `credentials.slack.bot_token`, `slack.app_token` and `slack.allowed_user_ids` are all set. Creates and updates chat-thread sessions (uuid5 ids, `files.md` section 3) and their `slack_origin` and `slack_watermark_ts`. Calls `https://slack.com/api/apps.connections.open`, `conversations.replies`, `conversations.info`, `chat.postMessage`, `reactions.add` and `reactions.remove`. | none |
| `discord` | `src.features.discord.service` | ready | Discord gateway listener and backfill | long-lived connection | Starts only when `credentials.discord.bot_token` and `discord.allowed_users` are set. Creates and updates chat-thread sessions with `discord_origin` and `discord_watermark_id`. REST base `https://discord.com/api/v10`. | none |
| `host_auth` | `src.features.host_auth.api` | ready | ssh reachability panel | a round at start, then one every 1800 s; each round probes only the hosts whose period has run out (1800 s, or 21600 s for a host in `needs-okta`), so a fresh home probes every host at start | Runs `ssh <alias> true` (non-interactive) for every `Host` alias in `/home/<user>/.ssh/config`; writes `<home>/host_auth.json` | ssh ControlMaster sockets in `/home/<user>/.ssh/controlmasters/` (`files.md` section 7); ssh connections to every configured host |
| `merge_pool` | `src.features.trace.api` | ready | Perfetto trace merge process pool | start does nothing; the pool is built on the first merge | `<home>/cache/perfetto_merge/`; stop calls `shutdown(wait=False, cancel_futures=True)` | none |
| `usage_tally` | `src.features.usage.api` | early | Preloads the usage tally modules on a thread so a later request never imports a mid-deploy tree | once | none | none |
| `ext_usage` | `src.features.usage.ext_usage` | ready | Quota panel poller | one account per 60 s (`EXT_USAGE_ROUND_GAP_SECONDS`), round-robin, re-deriving the account set every round | Accounts: the default Claude login (`$CLAUDE_CONFIG_DIR`, else `/home/<user>/.claude`), each `accounts.claude[].config_dir`, and Codex (`/home/<user>/.codex/sessions`). Calls `https://api.anthropic.com/api/oauth/usage`. Broadcasts `ext_usage` on the sidebar channel; serves `GET /api/ext-usage`. | On a 401 it refreshes the token through `https://platform.claude.com/v1/oauth/token` and rewrites `<login dir>/.credentials.json` (0600, atomic) |
| `speech` | `src.features.voice.service` | early | Provisions and warms the speech models on a thread; the server does not wait | once | Downloads into `<home>/models/` (`files.md` 5.11) | none |

Test-run consequences on a scratch home (`/tmp/...`): `host_auth` still probes every host in the operator's ssh
config. `ext_usage` still reads, and on a 401 rewrites, the operator's Claude login files. `speech` downloads its
models into the scratch home. The cgroup sweep still touches the operator's slice. A Rust port that wants
side-effect-free test runs needs a way to disable these services; the baseline has none, short of
`session_memory_max_mb: 0` for cgroups.

### Registration evidence (acceptance)

Command, run from the worktree root:

```
for pkg in $(sed -n '/^PACKAGES = (/,/^)/p' src/app/registrations.py | grep -o '"src[^"]*"' | tr -d '"'); do
  d=${pkg//.//}
  echo "$pkg: $(grep -ohE 'register_service|register_metadata_fields|register_config_section|register_legacy_keys|register_config_check|register_backend_type' $d/__init__.py | sort | uniq -c | tr '\n' ' ')"
done
```

Packages with `register_service`: `src.features.cron`, `src.features.slack`, `src.features.discord`,
`src.features.host_auth`, `src.features.trace`, `src.features.usage` (two), and `src.features.voice`. All eight
services are in the table above.

Packages with `register_metadata_fields`: `src.features.backend_pool`, `src.features.cron`, `src.features.slack`,
`src.features.discord`, `src.features.web_reply`, and `src.backends.claude_code` (two: one session slot, one
thread slot). Their fields are in `files.md` sections 6.1 and 6.5. In summary:

| Package | Slot | Fields | Placed after |
| --- | --- | --- | --- |
| `src.features.backend_pool` | session | `backend_from_pool: bool = false` | end |
| `src.features.cron` | session | `scheduled_task`, `last_scheduled_run`, `last_run_status`, `last_scheduled_cron` | `backend` |
| `src.features.slack` | session | `slack_origin`, `slack_watermark_ts` | `successor_session_id` |
| `src.features.discord` | session | `discord_origin`, `discord_watermark_id` | `successor_session_id`, after slack's |
| `src.features.web_reply` | session | `web_status` | end |
| `src.backends.claude_code` | session | `claude_account` | `cc_session_started_at` |
| `src.backends.claude_code` | thread | `claude_session_id` | `exit_code` |

Packages that register config sections, checks, legacy keys or option types are listed in `config.md` section 6.

## 6. Other startup registries

- Startup checks: section 1, step 5.
- Setup steps (run by `scripts/setup.sh`, never by the server): `src.features.cron.seed:setup_step`,
  `src.features.voice.voice_setup` and `src.backends.claude_code.setup_check`.
- Diff root: `src.features.memory.store_root:memory_dir` (`<home>/memory`).
- Scheduled handlers (`src/runtime/hooks/scheduled_handlers.py`):
  - `backup` → `src.features.backup.backup:run_scheduled_backup`;
  - `usage_ledger` → `src.features.usage.usage_ledger:run_scheduled_usage_ledger`;
  - `cool_storage` → `src.features.usage.storage_cool:run_scheduled_cool_storage`.
