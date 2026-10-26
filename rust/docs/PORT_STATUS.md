# Port status

One row per route of the Python app (`server.app.routes` at `PORT_BASELINE`), in its route order.

- **Stage** is the port stage that owns the route: `foundation` for the routes this workspace
  serves, otherwise the route family a later stage ports, named by the Python package that registers
  it (`sessions` is `src/runtime/api/sessions.py`, `features/cron` is `src/features/cron/`).
- **Status** is `ported` when `charliebot-server` answers the route as the Python server does, `todo` while the
  port still owes it, or `not ported` for a route that the port leaves out by decision. A `todo` or `not ported`
  route answers as an unknown path: 404 with the key, 401 without it.

Malformed input (a request that no client sends, such as invalid JSON or a wrong field type) keeps Python's
status code, and for a 422 answer the `type` and `loc` of each error; the error text may differ. Parity steps
for such input set `compare: status_and_shape`. Integers outside the signed 64-bit range, integral floats at or beyond plus or minus 2^63, and non-finite floats are undefined input: Python's answer to them depends on the route. The port refuses non-finite floats (`NaN`, `Infinity`, `1e400`), with 400 for a JSON literal and 422 for a string value. The port's answer to an out-of-range integer depends on the reader. The FastAPI body reader (`charliebot-compat/src/fastapi_body.rs`, `integer_value`) accepts a JSON integer from the signed 64-bit minimum to the unsigned 64-bit maximum and answers 422 `int_parsing_size` beyond that range. A route that stores the value as a signed 64-bit integer answers 500 for a value above the signed maximum, as `schedule-trigger` does for a `local_pid` pid of 2^63, where Python answers 200.

The order of the web reply audit's events is undefined. In the requested 20 Python-vs-Python runs, the nudge (`agent_message` from `Web reply`) came before its `run_finished` in all 40 logs. Across 53 kept Python logs from the follow-up runs, it came after once; the no-reply notice (`web_reply_notice`) came before its `run_finished` in all 53. One kept Rust run using the original write order put both events after their respective `run_finished` events. The status reminder (`agent_message` from `Session status`) is the same audit path's write, so it is undefined against the audited turn's `run_finished` the same way. A parity step that compares these events can use the `either_order_of_adjacent_events` rule, which accepts both orders (see `rust/parity/README.md`).

The exit code of an interrupted run is undefined for a related reason. Python's stopper and its launcher race for the
first write of the run's terminal record: the stopper records the interrupted outcome with no exit code
(`runs.py:1161`), the launcher records the signal exit `-9`, and `record_finish_locked` keeps the first write
(`runs.py:1036-1047`). The `interrupted-run-exit-code` rule of `handover-stop-request` accepts `null` and `-9` in that
run's record and compares every other code (see `rust/parity/README.md`).

The order of an interrupted run's `master_done` and `run_finished` is undefined for a related reason. The stop request's
SIGKILL path records the interrupted finish (`runs.py:1225`), which appends `run_finished` at `runs.py:1114-1124`, while
the master queue writes `master_done` after the agent exits (`master_cc_queue.py:298-309`). The
`either_order_of_adjacent_events` rule of `handover-stop-request` accepts either order for that run's pair only, pinned
by the run id its stop request captured (see `rust/parity/README.md`).

| Method | Path | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| GET, HEAD | `/openapi.json` | api-docs | not ported | No client reads this route. The Rust server answers it as an unknown path. |
| GET, HEAD | `/docs` | api-docs | not ported | No client reads this route. The Rust server answers it as an unknown path. |
| GET, HEAD | `/docs/oauth2-redirect` | api-docs | not ported | No client reads this route. The Rust server answers it as an unknown path. |
| GET, HEAD | `/redoc` | api-docs | not ported | No client reads this route. The Rust server answers it as an unknown path. |
| GET | `/api/sessions/chat-threads` | features/chat_threads | ported |  |
| GET | `/api/auth/status` | foundation | ported |  |
| GET | `/sessions/{session_id}/events` | pages | ported |  |
| GET | `/home` | pages | ported |  |
| GET | `/` | foundation | ported |  |
| GET | `/api/sessions/` | sessions | ported |  |
| POST | `/api/sessions/` | sessions | ported |  |
| GET | `/api/sessions/archived` | sessions | ported |  |
| GET | `/api/sessions/starred` | sessions | ported |  |
| GET | `/api/sessions/groups` | sessions | ported |  |
| POST | `/api/sessions/groups/rename` | sessions | ported |  |
| POST | `/api/sessions/groups/delete` | sessions | ported |  |
| GET | `/api/sessions/status` | sessions | ported |  |
| GET | `/api/sessions/tree` | sessions | ported |  |
| GET | `/api/sessions/search` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/pending-triggers` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/bootstrap` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/read` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/usage` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/events` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/transcript` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/fork` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/elone` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/backend` | sessions | ported |  |
| GET | `/api/sessions/{session_id}` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/archive` | sessions | ported | Operator and agent archive requests share one completion owner (a31877580): `rust/crates/charliebot-tasks/src/completion.rs` `archive_task` `:452`. |
| DELETE | `/api/sessions/{session_id}/permanent` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/unarchive` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/star` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/unstar` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/rounds/{round_id}/rate` | sessions | ported |  |
| PATCH | `/api/sessions/{session_id}` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/group` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/events.jsonl` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/runs` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/effective-prompt` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/runs/{run_id}/context` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/retry` | sessions | ported |  |
| GET | `/api/sessions/{session_id}/task-inputs/pending` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/task-inputs/acknowledge` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/cancel` | sessions | ported |  |
| POST | `/api/sessions/{session_id}/runs/{run_id}/cancel` | sessions | ported |  |
| POST | `/api/chat/{session_id}/upload` | chat | ported |  |
| POST | `/api/chat/{session_id}/message` | chat | ported |  |
| POST | `/api/chat/{session_id}/cancel` | chat | ported |  |
| GET | `/api/internal/version` | foundation | ported |  |
| POST | `/api/internal/delegate` | internal | ported | `charliebot-tasks::routes::delegate_task`; creates a worker child and first work Run. |
| POST | `/api/internal/schedule-trigger` | internal | ported |  |
| POST | `/api/internal/triggers/{session_id}/{trigger_id}/cancel` | internal | ported |  |
| POST | `/api/internal/session-message` | internal | ported | `charliebot-tasks::routes::session_message`; admits one `agent_message` into the target and dispatches it. |
| WS | `/ws/sessions/{session_id}` | session-stream | ported |  |
| POST | `/api/internal/plan/present` | features/artifacts | ported |  |
| POST | `/api/internal/plan/amend` | features/artifacts | ported |  |
| POST | `/api/internal/plan/approve` | features/artifacts | ported |  |
| POST | `/api/internal/plan/close` | features/artifacts | ported |  |
| GET | `/api/sessions/{session_id}/plans` | features/artifacts | ported |  |
| GET | `/api/code-server/open` | features/code_server | ported |  |
| GET | `/api/cron/tasks` | features/cron | ported | Broken files are returned after valid tasks with their raw `enabled` value. |
| PUT | `/api/cron/tasks/{name}` | features/cron | ported | Backend switches are refused with 409 before the file is written when the bound node is busy. |
| POST | `/api/cron/tasks` | features/cron | ported | Writes through the PyYAML-compatible core writer. |
| POST | `/api/cron/tasks/{name}/run` | features/cron | ported | Manual fires use the same bound execution path as scheduler fires. |
| DELETE | `/api/cron/tasks/{name}` | features/cron | ported | Unlinks the config and leaves its bound node and history in place. |
| POST | `/api/diag/switch-events` | features/diag | ported |  |
| POST | `/api/internal/slack/reply` | features/slack | ported |  |
| POST | `/api/internal/slack/ack` | features/slack | ported |  |
| POST | `/api/internal/discord/reply` | features/discord | ported |  |
| POST | `/api/internal/discord/read` | features/discord | ported |  |
| POST | `/api/internal/discord/check` | features/discord | ported |  |
| POST | `/api/internal/web/reply` | features/web_reply | ported |  |
| GET | `/api/internal/web/dialog` | features/web_reply | ported |  |
| POST | `/api/internal/web/status` | features/web_reply | ported |  |
| POST | `/api/internal/web/status/confirm` | features/web_reply | ported |  |
| POST | `/api/internal/web/status/unread` | features/web_reply | ported |  |
| POST | `/api/internal/web/ask-check` | features/web_reply | ported |  |
| GET | `/api/internal/web/status` | features/web_reply | ported |  |
| GET | `/host-auth` | features/host_auth | ported | renders through the shared `Templating` handle |
| GET | `/api/host-auth/status` | features/host_auth | ported |  |
| POST | `/api/host-auth/probe` | features/host_auth | ported |  |
| GET | `/api/git/branches` | features/diff_view | ported |  |
| GET | `/api/git/repos` | features/diff_view | ported |  |
| GET | `/api/git/diff/files` | features/diff_view | ported |  |
| GET | `/api/git/diff/file` | features/diff_view | ported |  |
| GET | `/diff` | features/diff_view | ported |  |
| POST | `/api/sessions/{session_id}/explain` | features/explain | ported |  |
| GET | `/api/sessions/{session_id}/explain` | features/explain | ported |  |
| GET | `/api/sessions/{session_id}/explain/status` | features/explain | ported |  |
| POST | `/api/internal/improve/stop` | features/improve | ported |  |
| POST | `/api/internal/improve` | features/improve | ported | Resolves the improve role before reservation; pooled iterations cool quota-failed members and retry with the next member. |
| GET | `/ncu` | features/ncu | ported | a cache miss runs a Python child, `rust/helpers/ncu_report_helper.py`, when the Nsight install has `extras/python`; otherwise the native `ncu --import --csv` path answers. See `rust/docs/python_helper.md`. A CSV integer cell outside the 64-bit range fails the import with a `ValueError` internal error, while Python keeps the exact integer |
| WS | `/ws/terminal` | features/terminal | ported |  |
| GET | `/perfetto` | features/trace | ported |  |
| GET | `/perfetto/merged` | features/trace | ported |  |
| GET | `/api/ext-usage` | features/usage | ported | `{"error": ...}` until the first round writes an entry; HEAD and other methods answer 405 with `allow: GET` |
| GET | `/token-usage` | features/usage | ported | HEAD and other methods answer 405; loading the page reads the ledger alone and never captures |
| POST | `/api/voice/{session_id}` | features/voice | ported | Persists the WAV before backend selection, then transcribes through the selected online backend (`rust/crates/features/voice/src/api.rs:110-177`, `recording.rs:101-143`). |
| WS | `/ws/voice/{session_id}` | features/voice | ported | live preview relay and recording archive |
| POST | `/api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages` | backends/openai_compatible | ported | streaming and JSON answers; parity stage other-backends |
| GET, HEAD | `/absolute_filepath/{path:path}` | features/files | ported |  |
| GET, HEAD | `/static/features/artifacts` | foundation | ported |  |
| GET, HEAD | `/static/features/backup` | foundation | ported |  |
| GET, HEAD | `/static/features/backend_pool` | foundation | ported |  |
| GET, HEAD | `/static/features/chat_threads` | foundation | ported |  |
| GET, HEAD | `/static/features/code_server` | foundation | ported |  |
| GET, HEAD | `/static/features/cron` | foundation | ported |  |
| GET, HEAD | `/static/features/diag` | foundation | ported |  |
| GET, HEAD | `/static/features/slack` | foundation | ported |  |
| GET, HEAD | `/static/features/discord` | foundation | ported |  |
| GET, HEAD | `/static/features/disk_sweep` | foundation | ported | `rust/crates/charliebot-server/src/main.rs:276` mounts every registered package, matching Python's empty `disk_sweep/static` mount. |
| GET, HEAD | `/static/features/web_reply` | foundation | ported |  |
| GET, HEAD | `/static/features/host_auth` | foundation | ported |  |
| GET, HEAD | `/static/features/diff_view` | foundation | ported |  |
| GET, HEAD | `/static/features/explain` | foundation | ported |  |
| GET, HEAD | `/static/features/improve` | foundation | ported |  |
| GET, HEAD | `/static/features/memory` | foundation | ported |  |
| GET, HEAD | `/static/features/ncu` | foundation | ported |  |
| GET, HEAD | `/static/features/router` | foundation | ported |  |
| GET, HEAD | `/static/features/terminal` | foundation | ported |  |
| GET, HEAD | `/static/features/trace` | foundation | ported |  |
| GET, HEAD | `/static/features/usage` | foundation | ported |  |
| GET, HEAD | `/static/features/voice` | foundation | ported |  |
| GET, HEAD | `/static/backends/openai_compatible` | foundation | ported |  |
| GET, HEAD | `/static/backends/antigravity` | foundation | ported |  |
| GET, HEAD | `/static/backends/charlie_code` | foundation | ported |  |
| GET, HEAD | `/static/backends/claude_code` | foundation | ported |  |
| GET, HEAD | `/static/backends/codex` | foundation | ported |  |
| GET, HEAD | `/static/backends/gemini` | foundation | ported |  |
| GET, HEAD | `/static/backends/anthropic_compatible` | foundation | todo | Python answers 500 under this mount: its `static/` directory is absent at the baseline. A `todo` route answers as an unknown path. |
| GET, HEAD | `/static/backends/opencode` | foundation | ported |  |
| GET, HEAD | `/static/features/files` | foundation | ported |  |
| GET, HEAD | `/static` | foundation | ported |  |

## Feature hooks

| Kind | Name | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| config section | `voice` | features/voice | ported | `default_backend` defaults to `gemini`; also accepts `vocabulary` and `languages` (`rust/crates/features/voice/src/config.rs:13-21,30-32`); no legacy keys and unknown-key refusal |
| config check | `check_default_backend` | features/voice | ported | `rust/crates/features/voice/src/config.rs:72-82`; validates against gemini and muse |
| template global | `voice_backends` | features/voice | ported | backend availability in dropdown order |
| template global | `voice_default_backend` | features/voice | ported | |
| turn contribution | `voice` | features/voice | ported | dictated input preamble |
| turn contribution | `chat_threads` | features/chat_threads | ported | names `thread_session.md` for a Slack or Discord thread session |
| let-go step | `artifacts.headless_renderer` | features/artifacts | ported | Closes Chrome after the shutdown log on SIGINT and failed bind. SIGTERM skips the step, like Python's `atexit` hook (`src/features/artifacts/headless_render.py:184`; `uvicorn/server.py:322-339`). |

## Services and lifespan

Baseline: `rust/docs/contract/state/services.md`. Status describes this Rust workspace at this
stage; `ported` means the startup or service behavior is implemented, `todo` means it is not yet
implemented.

| Kind | Service or step | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| pre-loop | `agent_environment.apply_agent_environment` | foundation | ported | `main` replaces the server's process environment with the agent environment before any thread starts and before the config load (`charliebot-server/src/main.rs:232`, `charliebot-agents/src/agent_environment.rs:63`). The web terminal's tmux server and the usage panel's `claude --version` probe inherit it, and the typed `AgentBaseEnvironment` equals the rewritten environment. Python: `server.py:582`, `src/runtime/agent_environment.py:53-63`. Deviation: Rust takes the venv bin from `CHARLIEBOT_VENV_BIN` (`charliebot-core/src/venv.rs`), Python from `sys.prefix`. |
| pre-loop | `config.get_config` | foundation | ported | |
| pre-loop | `config.require_backends` | foundation | ported | |
| pre-loop | `v1_sessions.require_no_v1_sessions` | foundation | ported | Runs before bind. |
| pre-loop | Registered startup checks | foundation | ported | The loop runs registered checks (`charliebot-server/src/main.rs:226`); usage `backend_ids` and cron `cron_backend_refs` both register their checks. |
| pre-loop | Start server on configured host and port | foundation | ported | |
| lifespan | 1. Read config | foundation | ported | |
| lifespan | 2. Acquire home writer fence | foundation | ported | |
| lifespan | 3. Capture build information | foundation | ported | |
| lifespan | 4. Initialize the home | foundation | ported | |
| lifespan | 5. Log and sweep session cgroups | foundation | ported | The `session_cgroups` pre-recovery step logs the cgroup startup line and sweeps empty stale cgroups named for this home's sessions (`charliebot-agents/src/agent_environment.rs:95`, `transport/cgroup.rs:184`, `:214`; Python `server.py:266-268`, `src/infra/process.py:307`, `:334`). Negative `session_memory_max_mb`, `session_swap_max_mb` and `session_tasks_max` start as in Python, and each spawn applies them as `transport/cgroup.rs:143` and `:52` do (Python `src/infra/process.py:272`, `:110`). Parent-death children spawn on a process-lifetime thread; pdeath-free children use Tokio's blocking pool (`charliebot-agents/src/transport/spawn.rs`). Parent-side turn-tree spawns set nice to 10 after exec; the Claude one-shot raises child nice by 10 before exec, matching Python `os.setpriority` and `os.nice` (`src/runtime/agent_process/base.py:858-903`, `src/infra/process.py:192-202`). |
| lifespan | 6. Start task-tree recovery | agent-runtime | ported | `charliebot-tasks` registers `recovery::recovery_step` and reconciles task-tree nodes. The four handover scenarios (`handover-manager-turn`, `handover-admitted-input`, `handover-drain`, `handover-stop-request`) match Python in both directions. The runner spawns the step (`charliebot-registry/src/lifespan.rs:98`). Recovery drops each cached event vector after its node pass; task facts keep shared event payloads, and the run-facts memo retains up to 64 current lists by stable list identity (`charliebot-store/src/events.rs:865-898`, `charliebot-store/src/run_records.rs:196-230`). Python keeps parsed lists in `_events_cache` (`src/runtime/chat_events.py:166-168`, `:226-232`) and its `_RunFactsMemo` retains up to 64 lists keyed by `id(events)` (`src/runtime/runs.py:433-449`). Rust task facts index shared payloads (`charliebot-store/src/task_tree.rs:90-113`). On SIGTERM, recovery checks between nodes, run-directory entries and event-fold blocks; the count tests assert those checks. A client sees a difference only for a live log with malformed lines: after cache eviction, event count and range indices following one use the cold-path physical-line count (`charliebot-store/src/events.rs:176-180`, `:308-313`), matching Python for a session its boot did not load (`src/runtime/chat_events.py:263-265`, `:288-300`). |
| lifespan | 7. Start early services | services | ported | Starts the early services in registration order after the recovery task starts (`charliebot-registry/src/lifespan.rs:91`, `:102-114`; Python `server.py:270-273`). |
| lifespan | 8. Await task-tree recovery | agent-runtime | ported | The runner awaits `recovery::recovery_step` (`charliebot-registry/src/lifespan.rs:108`). On SIGINT and failed bind, `recovery::let_go_step` cancels run launches and then session consumers after `charliebot_shutdown`, matching `asyncio.run` (`src/runtime/master_cc_run.py:648-664`; `asyncio/runners.py:66-75`). SIGTERM skips it (`uvicorn/server.py:322-339`). The four handover scenarios in row 6 cover the await in both directions. Deviation: a deliberate deviation from Python. Python does not stop during startup recovery: the lifespan awaits recovery inside Uvicorn's startup hook, and Uvicorn checks its exit flag only after startup finishes (`server.py:271-288`; uvicorn 0.42.0 `uvicorn/server.py:77-79` and `:94-96`; signal capture at `uvicorn/server.py:322-346`). Rust installs SIGINT and SIGTERM before any startup step and stops startup at safe points between recovery nodes, trigger scans and the cgroup sweep, releases the home writer fence and logs `charliebot_shutdown` with `boot=true` (`charliebot-server/src/main.rs`, `charliebot-registry/src/lifespan.rs`). After `server_ready`, the shutdown sequence matches Python. Deviation: when the let-go cancels a worker launch before the launch records its child's pid, Rust kills that child, and Python leaves it running. In Python the cancel passes the `except Exception` around `on_spawn` (`src/runtime/agent_process/base.py:930-938`), the child runs with `start_new_session` and no parent-death signal (`src/runtime/agent_process/base.py:994-1012`), and the next boot starts a second agent for the run (`src/runtime/task_execution.py:2780`, `:2816`). Rust holds the child stopped before `exec` until its pid is recorded, and a launch dropped before then sends SIGKILL to that one pid, the one from its own spawn, when the pid's start tick still matches (`charliebot-agents/src/runner.rs:592-628`); the run keeps no pid and no terminal fact, and the next boot launches it once. A launch requested after the let-go begins starts no task (`charliebot-tasks/src/execution.rs:338-344`) and leaves the durable state of a Python launch task that `asyncio.run` cancels before its first step (`src/runtime/task_execution.py:522`, `:731`; `asyncio/runners.py:208-216`): the run registered with its input claimed, no pid and no terminal fact, which the next boot dispatches once. |
| lifespan | 9. Recover pending triggers | agent-runtime | ported | |
| lifespan | 10. Start ready services | services | ported | Starts the ready services in registration order after the trigger step (`charliebot-registry/src/lifespan.rs:99`, `:102-114`; Python `server.py:284-285`). |
| lifespan | 11. Log ready and serve | foundation | ported | |
| lifespan | 12. Stop services and close shared clients | services | ported | Python's `charliebot_shutdown` reports `shutdown_ms`, each registered service's `<name>_ms`, `http_client_ms` and `ws_close_ms` (`server.py:293-310`). Rust writes `usage_tally_ms = 0` because it has no `usage_tally` service. Services stop in reverse registration order, then the HTTP client and WebSockets close; Rust releases the fence and logs the line. Let-go steps run after the line on SIGINT and failed bind (`uvicorn/server.py:179-182`); SIGTERM raises its default action and skips them (`uvicorn/server.py:322-339`; `asyncio/runners.py:66-75`). Rust closes WebSockets with code 1000; Uvicorn closes them with code 1012 before lifespan shutdown. The web UI reconnects on any close and does not read the code (`web/static/js/websocket.js`, `socket.onclose`). The usage quota builds its own client, so it is not closed here. |
| lifespan | 13. Release home writer fence | foundation | ported | |
| service | `scheduler` | features/cron | ported | Waits for task-tree recovery, binds each unbound cron task to one manager node, and scans enabled tasks every 60 seconds. A bound-node wake clears a native anchor older than the latest Saturday 01:00 America/Los_Angeles boundary, archives old root chat events, and decides the report prefix from the cleared anchor; recycle errors log `scheduled_session_recycle_failed` and do not stop the wake. Worker-thread directories remain untouched, matching Python. Client-visible cron stamps use Python `datetime.isoformat()` microseconds and the task-zone offset. |
| service | `slack` | features/slack | ported | Starts Socket Mode and the lost-summon backfill when both Slack tokens and an allowed-user id are configured. |
| service | `discord` | features/discord | ported | Starts the Discord gateway and lost-summon backfill when the bot token and an allowed user are configured. |
| service | `host_auth` | features/host_auth | ported | one round at start, then one per 1800 s |
| service | `merge_pool` | features/trace | ported | In-process builds share one cache-key flight and a bounded build gate. |
| service | `usage_tally` | features/usage | not applicable | It only preloads Python modules; Rust has no import cost to hide. |
| service | `ext_usage` | features/usage | ported | one account per 60 s, round-robin; the account set is re-derived every round. Deviation: `ext_usage_poll_error` formats the error with its cause chain (`{error:#}`); Python logs `str(e)` for a fetch error, and the traceback for a round error. |
| service | `headless_renderer` | features/artifacts | ported | Lazy per-process Chrome renderer; the let-go step closes it after the shutdown log on SIGINT and failed bind. SIGTERM skips the step, so the browser stays open as Python's `atexit` hook does not run (`src/features/artifacts/headless_render.py:184`; `uvicorn/server.py:322-339`). Deviation: close sends SIGKILL to Chrome's process group; Python kills only the browser pid (`src/features/artifacts/headless_render.py:159`). |
| registration | scheduled handler `backup` | features/backup | ported | `run_scheduled_backup` creates the archive and applies retention off the async runtime, and returns the archive path as the fire's summary. |
| registration | scheduled handler `usage_ledger` | features/usage | ported | Captures registered log sources and CharlieBot threads, master captures and Runs off the async runtime. |
| registration | scheduled handler `cool_storage` | features/usage | ported | Real run only (no dry run, session scope or vacuum): captures usage into the ledger, deletes cold sessions' raw transport files, then calls each usage source's sweep, off the async runtime. Returns `format_sweep_line` as the summary. The cold rule reads `updated_at` as `models.parse_utc_datetime` does (`utc_datetime.rs`), and a rotation suffix is `str.isdigit` (`charliebot_core::pystr`). Deviations: a metadata document with a NaN or infinite number fails the run, where Python loads it; `updated_at` text with a week date, a stray final character or a fractional offset keeps its session, where Python may read it as cold; `storage_cool_sweep_done` logs `session_id` as Debug text (`None`), not JSON null. |
| registration | backend type `opencode` | backends/opencode | ported | Registers the option model, factory, native-id traits, shared serve manager, usage source attribution and `snapshot` reading limits (`charliebot-agents/src/backends/opencode/mod.rs`). |
| registration | usage source `opencode` | backends/opencode | ported | Registers id prefixes `opencode-` and `oc-`. The log side reads the store database (`usage_logs.rs`); the sweep vacuums it and reports its freelist (`usage_sweep.rs`). |
| registration | diff root `memory` | features/memory | ported | `<home>/memory`. The diff view that reads it is not ported (features/diff_view). |
| registration | turn contribution `memory` | features/memory | ported | Adds the memory segments of every run's managed instructions. No Rust code launches a run yet, so nothing calls it until the run launch path lands. |
| registration | turn contribution `web_reply`: instruction segments and round-end audit | features/web_reply | ported | Adds `prompts/web_reply.md` to a manager run of a web session. The Rust server runs the round-end audit (`after_turn`, `audit.py`) after every clean turn of a web session, background turns included: the store starts each contribution's `after_turn` on `master_done` (`RegistryRuntimeEvents::after_turn` in `charliebot-registry/src/store_hooks.rs`), and the audit posts the status reminder of a turn that recorded no status set or confirm (`audit.rs`), and the reply nudge or the notice, with one combined reminder and one wake when a turn misses both. The run launch path that assembles the segments is not ported yet. |
| registration | router `/api/internal` (`web_reply`) | features/web_reply | ported | The five web reply routes above. |
| registration | turn contribution `slack` | features/slack | ported | Adds the thread reply contract to manager runs, renders `slack_reply`, and runs the round-end audit from `after_turn`. The thread core it calls (`thread_entry.py`) is ported in `features/chat_threads`. |
| registration | turn contribution `discord`: instruction segments and round-end audit | features/discord | ported | Adds the thread reply contract to manager runs and calls `deliver_done` from `after_turn`. The hook runs through the A1 runtime event hook. |
| component | thread core (`thread_entry.py`) | features/chat_threads | ported | Reply posting and retries, the round audit, lost-summon backfill, follow triggers, and file-server link checks. Slack and Discord supply their own `ThreadAdapter` implementations. |
| component | `ChannelService` (`channel_service.py`) | features/chat_threads | ported | Starts the Slack or Discord listener and boot backfill on the server's `ServiceContext`, and stops both by cancelling and awaiting them. |
| component | boot backfill `run_backfill` (`backfill.py`) | features/chat_threads | ported | Waits for crash recovery, then reports the platform's summons lost across the restart. Both channel services run it. |

## State files

Baseline: `rust/docs/contract/state/files.md`. `S1` covers session metadata and chat history. Later
run-record work is `S2`; task-tree runtime and feature-owned files remain with their named stages.

| File kind | Stage | Status | Note |
| --- | --- | --- | --- |
| `config.yaml` | foundation | ported | |
| `credentials.yaml` | foundation | ported | |
| `config.d/cron.d/<name>.yaml` | features/cron | ported | Read and written by the Rust cron loader and API; task writes use the PyYAML-compatible writer. |
| `MASTER_AGENT_PROMPT.md` | agent-runtime | ported | Read only: the host supplement of a manager prompt, with `YOUR_SESSION_UUID` replaced (`charliebot-tasks/src/prompts/sources.rs:91`, `:125-134`); Python `src/runtime/task_prompts.py:233-238`, path `src/infra/config.py:287-289`. |
| `state/home_writer.lock` | foundation | ported | |
| `state/writer_identity.json` | foundation | ported | |
| `state/boot_recovery.json` | tasks | ported | Python-compatible marker: `write_boot_recovery_marker` syncs a temporary file and renames it over the marker after a clean pass, and `boot_skip_reason` keeps a durable file within the one-second mtime slack (`task_recovery.py:87-119`). `read_boot_recovery_marker` reads an unreadable marker as absent with `boot_recovery_marker_unreadable`, so every node gets a full check (`task_recovery.py:58-74`). Rust: `charliebot-tasks/src/recovery.rs`; tests `tests/recovery.rs::clean_boot_writes_marker_and_the_next_boot_skips_a_closed_worker` and `tests/recovery.rs::an_empty_boot_marker_reads_as_absent`. |
| `state/cron_alert_fingerprint.json` | features/cron | ported | Sorted broken-task names; alerts transition once per changed set. |
| `cache/cli_base_url.json` | foundation | not applicable | Written and read by the Python CLI (`src/runtime/cli/common.py:335`), which stays (Trade-off 2). |
| `cache/perfetto_merge/` | features/trace | ported | Cache directory for merged Perfetto traces. |
| `cache/perfetto_merge/<sha256>.json.gz` | features/trace | ported | |
| `/tmp/merge-chunks-*` | features/trace | not applicable | the Rust merge runs in-process (parent-approved); the cache key is unchanged |
| `/tmp/merge-members-*` | features/trace | not applicable | the Rust merge runs in-process (parent-approved); the cache key is unchanged |
| `host_auth.json` | features/host_auth | ported | written atomically with `json.dumps(indent=2)` and a newline |
| `usage/ledger.sqlite3` | features/usage | ported | Rust preserves the Python schema, trigger text, queries and value formats. |
| `prompt_bodies/<sha256>.md` | agent-runtime | ported | Written once by `store_prompt_body` (`charliebot-store/src/task_tree.rs:485-499`); read with the sha256 check (`charliebot-tasks/src/prompts/sources.rs:97-115`). Python: `src/runtime/task_sessions.py:1527-1538`, `src/runtime/task_prompts.py:258-270`. |
| `memory/` | features/memory | ported | Git repository created by `ensure_store` on first use. |
| `memory/topics` | features/memory | ported | Seeded when missing; read by the selection. |
| `memory/.gitignore` | features/memory | ported | Seeded when missing. |
| `memory/entries/<topic>/<slug>.md` | features/memory | ported | Read only: format v2 grammar and selection. |
| `memory/staging/` | features/memory | ported | Created by `ensure_store`; never read by a selection. |
| `memory/staging/<stamp>-<sess8>-<slug>.md` | features/memory | not applicable | Written by `charliebot memory add`: the Python CLI stays (Trade-off 2). |
| `memory-proposal/` | features/memory | not applicable | Created by `charliebot memory proposal open`: the Python CLI stays (Trade-off 2). |
| `models/` | features/voice | not applicable | Startup registers no speech service, so it builds no local speech model and creates no `models/` entry. |
| `vendor/katex/katex.min.js` | foundation | not applicable | Written by `charliebot artifact wrap`, a Python CLI that stays (Trade-off 2). |
| `voice_eval/<ts>/results.json` | features/voice | not applicable | CLI output; the Python CLI stays (Trade-off 2). |
| `voice_eval/<ts>/summary.md` | features/voice | not applicable | CLI output; the Python CLI stays (Trade-off 2). |
| `opencode/` | backends/opencode | ported | The shared server creates the store's `data/`, `state/` and `cache/` roots (`charliebot-core/src/home.rs`); the usage source reads and vacuums `data/opencode/opencode.db`. |
| `sessions/.counter` | agent-runtime | ported | Read, count-dirs fallback and rewrite in `next_session_name` (`charliebot-store/src/task_tree.rs:877-897`); Python `src/runtime/task_sessions.py:1130-1151`. Deviation: Rust logs `session_counter_unparsable` or `session_counter_unreadable` at warn on the fallback; Python falls back silently. |
| `sessions/.task-<id>-<pid>-<uuid>.tmp/` | agent-runtime | ported | Rust names the staging directory `.task-<id>-<pid>-<uuid>.tmp` like Python (`charliebot-store/src/task_tree.rs` `staging_dir_name`, used by `publish_task_node`); the prefix/suffix skippers in `task_tree.rs` `build_tree_index` and `charliebot-core/src/v1_sessions.rs` are unaffected. Covered by the `staging_dir_name_carries_task_id_process_id_and_uuid` unit test. |
| in-memory usage caches | features/usage | ported | Python `src/features/usage/ext_usage.py:28` (`_cached_usage`) and `:31` (`_accounts`); Rust `rust/crates/features/usage/src/ext_usage.rs:17-42` (`Panel`). Not persisted files. |
| `sessions/<sid>/metadata.json` | S1 | ported | Ordered typed metadata, registered slots, unknown keys, atomic writes. |
| `sessions/<sid>/threads/<tid>/metadata.json` | S1 | ported | Typed thread model and registered slots. |
| `sessions/<sid>/data/chat_events.jsonl` | S1 | ported | Append, sync, tolerant NDJSON reads, cache and ranges. |
| `sessions/<sid>/data/archives/chat_events.<year>-W<week>.jsonl` | S1 | ported | Archive rotation and global `archive_offset`. |
| `sessions/<sid>/data/explain_results.json` | features/explain | ported | Written atomically as Python writes it; stale pending entries are reaped on read. The entry's error text is the anyhow root cause. It equals Python's `str(e)` for one-shot failures and differs for OS errors. |
| `sessions/<sid>/data/runs/<run>/task_spec.md` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/metadata.json` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/events.jsonl` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/agent.raw.ndjson` | agent-runtime | ported | Written by the runner (`charliebot-agents/src/runner.rs:195`, name `charliebot-store/src/run_records.rs:31`); Python `src/runtime/runs.py:47`, `src/runtime/agent_process/base.py:969`. |
| `sessions/<sid>/data/runs/<run>/agent.stderr.log` | agent-runtime | ported | Written by the runner (`charliebot-agents/src/runner.rs:196`, name `charliebot-store/src/run_records.rs:33`); Python `src/runtime/runs.py:48`, `src/runtime/agent_process/base.py:970`. |
| `sessions/<sid>/data/runs/<run>/agent.raw.cursor` | agent-runtime | ported | Maintained by the runner (`charliebot-agents/src/runner.rs:197`, name `charliebot-store/src/run_records.rs:34`); Python `src/runtime/runs.py:49`, `src/runtime/agent_process/base.py:971`. |
| `sessions/<sid>/data/runs/<run>/agent.raw.ndjson.<n>` | agent-runtime | ported | Rotated before each fresh child (`charliebot-agents/src/runner.rs:198`, `charliebot-agents/src/transport/rotation.rs:9`); Python `src/runtime/agent_process/base.py:596-618`, `:974`. |
| `sessions/<sid>/data/runs/<run>/agent.stderr.log.<n>` | agent-runtime | ported | Rotated beside the raw output (`charliebot-agents/src/transport/rotation.rs:24`); Python `src/runtime/agent_process/base.py:596-618`. |
| `sessions/<sid>/data/runs/<run>/stdout.log` | backends/antigravity | ported | Written by the final-stdout run, truncated at each run's start (`charliebot-agents/src/runner.rs`). |
| `sessions/<sid>/data/runs/<run>/stderr.log` | backends/antigravity | ported | Written by the final-stdout run (`charliebot-agents/src/runner.rs`). |
| `sessions/<sid>/data/runs/<run>/task.md` | backends/charlie-code | ported | Written by the charlie-code command builder (`charliebot-agents/src/backends/charlie_code/charlie_code_cli.rs`). |
| `sessions/<sid>/data/runs/<run>/harness_instructions.md` | agent-runtime | ported | Written by worker launches when the backend supplies managed instructions. |
| `sessions/<sid>/data/runs/<run>/prompt_snapshot.json` | agent-runtime | ported | Written atomically before launch (`charliebot-tasks/src/execution.rs:319-321`); Python `src/runtime/task_execution.py:1195-1197`. |
| `sessions/<sid>/data/runs/<run>/launch_prompt.md` | agent-runtime | ported | Written atomically by `persist_launch_text` (`charliebot-tasks/src/execution.rs:584-587`, called at `:402`); Python `src/runtime/task_execution.py:1871-1880`. |
| `sessions/<sid>/data/runs/<run>/cold_read.json` | agent-runtime | ported | Written at the cold-read launch and on each failed attempt (`charliebot-tasks/src/execution.rs`, `build_cold_read_context` and `record_cold_read_attempt`); Python `src/runtime/task_execution.py:1736` and `:2521`. |
| `sessions/<sid>/data/runs/<run>/hang_diagnostics.json` | agent-runtime | ported | Written when a worker launch ends with captured hang diagnostics. |
| legacy `sessions/<sid>/data/master_runs/<started_at>/` | agent-runtime | ported | Read-only baseline layout. Readers: the store's `master_run_log_paths_sync` (`charliebot-store/src/run_records.rs` `legacy_log_paths`), usage's cold-storage sweep (`rust/crates/features/usage/src/cool_storage.rs`, tests `cool_storage_tests.rs`) and corpus capture (`rust/crates/features/usage/src/capture.rs`, tests `capture_tests.rs`); covered for the store reader by the `legacy_master_run_and_thread_log_readers_walk_the_python_layouts` test. |
| legacy `sessions/<sid>/threads/<tid>/data/` | agent-runtime | ported | Read-only baseline layout. Readers: the store's `thread_event_log_paths_sync` (`charliebot-store/src/run_records.rs`), usage's cold-storage thread sweep (`rust/crates/features/usage/src/cool_storage.rs`) and capture (`rust/crates/features/usage/src/capture.rs`), and backup's exclusion classifier (`rust/crates/features/backup/src/backup.rs` `should_exclude`, tests `lib_tests.rs`); covered for the store reader by the `legacy_master_run_and_thread_log_readers_walk_the_python_layouts` test. |
| `sessions/<sid>/triggers/<trigger_id>.json` | agent-runtime | ported | |
| `sessions/<sid>/launches/<launch_id>/metadata.json` | foundation | not applicable | Written by the Python CLI (`src/runtime/cli/schedule_trigger.py:309`), which stays (Trade-off 2). |
| `sessions/<sid>/launches/<launch_id>/log` | foundation | not applicable | Written by the Python CLI's local launch (`src/runtime/cli/schedule_trigger.py:326`), which stays (Trade-off 2). |
| `sessions/<sid>/launches/<launch_id>/exit` | foundation | not applicable | Written by the Python CLI's local launch (`src/runtime/cli/schedule_trigger.py:326`), which stays (Trade-off 2). |
| `sessions/<sid>/plans.json` | features/artifacts | ported | The read, the fork copy, and the present, amend, approve and close verbs are ported. |
| session artifact pages | features/artifacts | ported | |
| `sessions/<sid>/uploads/<filename>` | chat | ported | |
| `sessions/<sid>/voice/<stamp>_<id>.wav` | features/voice | ported | 16 kHz mono PCM16 WAV |
| `sessions/<sid>/voice/<stamp>_<id>.txt` | features/voice | ported | final transcript |
| `sessions/<sid>/loops/active.lock` | features/improve | ported | |
| `sessions/<sid>/loops/<n>/state.json` | features/improve | ported | Stores pool selection, quota blockers, iteration progress and the final close state. |
| `sessions/<sid>/loops/<n>/goal.md` | features/improve | ported | |
| `sessions/<sid>/loops/<n>/plan.md` | features/improve | ported | |
| `sessions/<sid>/loops/<n>/iter_<n>.md` | features/improve | ported | |
| `sessions/<sid>/CLAUDE.md` instructions | agent-runtime | ported | Written into the run cwd by `prepare_cwd` (`charliebot-agents/src/runner.rs:600-616`); the claude-code target is `charliebot-agents/src/backends/claude_code/claude_code.rs:281`, the launch content `charliebot-tasks/src/master_run.rs:530`. Python: `src/runtime/agent_process/base.py:728-739`, `src/backends/claude_code/claude_code.py:276-277`. |
| `sessions/<sid>/AGENTS.md` instructions | agent-runtime | ported | Same `prepare_cwd` path (`charliebot-agents/src/runner.rs:600-616`); the codex target is `charliebot-agents/src/backends/codex/backend.rs:218`, charlie-code's `charliebot-agents/src/backends/charlie_code/charlie_code_cli.rs:164-165`. Python: `src/runtime/agent_process/base.py:728-739`, `src/backends/codex/codex.py:55`, `src/backends/charlie_code/charlie_code.py:133`. |
| session cgroup directories | agent-runtime | ported | Created or refreshed as `charliebot-sess-<first 8 session ID chars>` by `prepare_session_cgroup` (`charliebot-agents/src/transport/cgroup.rs:141-154`). Parent-side children move after exec; Claude one-shot and compaction children move before exec. Python `src/infra/process.py:110`, `:272`, `src/backends/claude_code/claude_code.py:300`, `src/backends/claude_code/claude_compaction.py:288-306`. Outside the home. |
| Claude `.credentials.json` | backends/claude-code | ported | Read for the panel and renewed by `claude_code/usage_quota.rs` (atomic write, mode 0600); account health reads in `claude_code/claude_accounts.rs`. Outside the home. |
| Claude and Codex transcript files | backends/claude-code, backends/codex | ported | Read by `claude_code/usage_logs.rs` and `codex/usage_logs.rs`; swept by `claude_code/usage_sweep.rs` and `codex/usage_sweep.rs`; live pool copies trimmed by `claude_accounts::retire_transcript_copies`. Outside the home. |
| worktree directories | agent-runtime | ported | Worker work runs create isolated worktrees under `paths.worktree_dir`. |
| worktree trash | agent-runtime | not applicable | Python's server never writes `<worktree_dir>/.trash/`; only the Python CLI `gc-trash` reads and empties it, and the CLI stays (Trade-off 2). The Rust usage sweep skips the directory by name (`charliebot-agents/src/backends/claude_code/usage_sweep.rs`), covered by `charliebot-agents/tests/claude_usage_cross_impl.rs`. |
| sibling backup archives | features/backup | ported | `create_backup` writes `charliebot-<YYYYMMDD-HHMMSS>.tar.gz` (local time) into the sibling `<home name>_backup` directory, and `apply_retention` prunes it by the tiered policy: under 7 days keep all, 7-30 days keep Sundays, 30-90 days keep the 1st of the month, older delete. The handler fires through the cron scheduler, which the agent-runtime line ports. Deviation: `strptime` also reads one-digit date fields (`charliebot-2026111-120000` is 2026-11-01); here only the 15-character form parses, so such a name is kept. |
| `<publish.dir>/<token>/<basename>` | CLI publish | todo | Outside the home. |
| one-shot backend temporary directories | backends | ported | The runner's transport log dir `charliebot-run-*` (`charliebot-agents/src/runner.rs`) and the one-shot cwd `charliebot-oneshot-*` (`charliebot-agents/src/one_shot.rs`) are removed when the run returns, covered by `agent_one_shot_text_collects_assistant_text_until_result`; the opencode one-shot XDG root `charliebot-opencode-oneshot-*` (`charliebot-agents/src/backends/opencode/backend.rs` `oneshot_root`) is removed on drop, covered by `oneshot_root_uses_the_python_prefix_and_is_removed_on_drop`. |
| explain temporary directories | features/explain | ported | Outside the home. A read-only history copy per run, removed when the run ends. |
| headless-render temporary directories | features/artifacts | ported | Outside the home. Removed when a renderer closes or a measurement fails. |
| memory proposal temporary extraction | features/memory | not applicable | Outside the home. The Python CLI stays (Trade-off 2). |
| SSH ControlMaster sockets | foundation | ported | Outside the home. The control directory (mode 0700), `ControlPath`, `ControlPersist`, `ConnectTimeout` and `BatchMode` match Python (`charliebot-core/src/ssh.rs:17`, `:23`, `:35`; Python `src/infra/ssh.py:17-24`, `:27-53`). The host-auth probe and the sched launcher build their argv through it (`features/host_auth/src/probe.rs:57`, `charliebot-sched/src/triggers.rs:624`). |
| artifact page-height probe files | features/artifacts | ported | Beside an artifact. Removed after every measurement outcome. |
| tmux socket `charliebot` | features/terminal | ported | Outside the home. |
| tmux session `charliebot-<id>` | features/terminal | ported | Detached terminal shell. |
| repo config examples and prompt inputs | foundation | reader-only | Not written by the server. |

## Catch-up f58a345a6 open steps

At this head the all-stage Rust parity run matches all 1179 steps, so no row remains. No row is intermittent.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up f58a345a6 ignored Cargo tests

No Cargo test is ignored for a behavior introduced by this catch-up.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |
## Catch-up f58a345a6 unported commits

Each of the 10 commits in `git rev-list --no-merges 78e0d10e2..f58a345a6 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| 0419cd23a | Restore skill reads after manager compaction | in Rust | `claude_launch.rs::compaction_context_hook_settings` (`rust/crates/charliebot-agents/src/backends/claude_code/claude_launch.rs`) builds the compact-matcher SessionStart hook; `headless_claude_settings` merges it into the manager-turn `--settings` (`claude_code.rs::build_command`, which requires the CharlieBot session id) and `claude_compaction.rs::compaction_command` merges it into the compaction `--settings`. The hook command runs the Python `charliebot` CLI, so `src/runtime/cli/compaction_context.py` stays Python. Tests: `manager_turn_settings_install_six_compaction_context_slots` and `manager_turn_settings_require_the_charliebot_session_id` (`claude_code.rs`), `compaction_command_disables_skills_and_installs_the_compaction_context_hook` (`claude_compaction.rs`), `the_idle_argv_installs_the_compaction_context_hook` (`claude_idle_compaction.rs`). |
| 300890241 | Ignore non-plain skill reads and unresolved paths | no behavior | The hook command runs the Python `charliebot` CLI, which stays (Trade-off 2), so the change lives entirely in `src/runtime/cli/compaction_context.py:51-58,123-134` — the non-plain-path filter and the unresolved-skill skip — and no Rust server code carries a counterpart. |
| 9127ef401 | Close the warm headless renderer in a stop hook | no behavior | Python `src/features/artifacts/headless_render.py:187-199` closes Chrome and removes its temporary profile at stop. This resource cleanup changes no client response, event or home data. |
| f2620375f | Exit after server stop without finalizing the heap | no behavior | Python `server.py:607-616` flushes logs and exits after uvicorn has stopped. This changes teardown only; request answers and persisted state do not change. |
| 52eb05c1c | Deduplicate retried user messages | in Rust | Chat sends require a non-empty `request_id`; a stable session-bound event id replays matching input under the control lock, user frames retain the id, and post-admission dispatch errors return 202 with `dispatch_error`. |
| 2e0daa253 | Bind retried sends to their session and stop thinking only on refusal | in Rust | Rust serves the updated `web/static` unchanged; its chat endpoint requires the `request_id` that the browser keeps bound to the original session across retries. |
| 170eb4dea | Keep nodes whose files were written at pass start | in Rust | Python `src/runtime/task_recovery.py:89-119` applies one second of mtime slack before boot skips a node. Rust `rust/crates/charliebot-tasks/src/recovery.rs` (`boot_skip_reason`, `MARKER_MTIME_SLACK_SECONDS`) keeps a durable file whose mtime is no earlier than the marker minus one second. Test: `recovery.rs::tests::a_file_up_to_the_mtime_slack_before_the_marker_keeps_its_node`; the boot-pass tests `tests/recovery.rs::clean_boot_writes_marker_and_the_next_boot_skips_a_closed_worker` and `recovery_done_logs_one_line_with_marker_and_skip_counters` backdate their files past the slack. No parity step forces a coarse-clock write race. |
| d59e81e8c | Deduplicate agent sends and thread replies | in Rust | Run-scoped request ids, stable event payloads checked under the control lock, per-session reply/summon/audit serialization, Slack metadata-keyed posts and Discord nonce-keyed posts with content lookup. |
| 0b5351c00 | Read the error-row rule from one shared predicate | no behavior | Python `web/static/js/chat/shared.js:204-212` centralizes the existing `Error: ` predicate used by both views. The For You filter keeps the same rows and output. |
| 26d40d04e | Show a failed turn’s full error text in chat | in Rust | Rust serves the changed `web/static` tree through `rust/crates/charliebot-server/src/routes/static_mounts.rs:28-42,55-68`; its already-ported `/ws/sessions/{session_id}` stream supplies the message. No new API behavior is needed. |

## Catch-up 24642edf5 open steps

Both all-stage Rust debug runs failed the same 77 of 1176 steps, and each failing step is a row of the
`Catch-up f58a345a6 open steps` section above. No step fails for a `main` change of this range, so this
section has no rows. The Python-vs-Python run passed all 1116 of its steps.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 24642edf5 ignored Cargo tests

No Cargo test is ignored for a behavior introduced by this catch-up.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 24642edf5 unported commits

Each of the 19 commits in `git rev-list --no-merges f58a345a6..24642edf5 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells of the sections above.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| 982bf8cb4 | fsync the boot marker before its rename | in Rust | Python `src/runtime/task_recovery.py:87` fsyncs the tmp marker before `os.replace` at `:88`, so a power loss cannot leave an empty `state/boot_recovery.json` after the rename. Rust `rust/crates/charliebot-tasks/src/recovery.rs` (`write_boot_recovery_marker`) calls `sync_all` on the temporary file before the rename. Test: `recovery.rs::tests::the_marker_write_syncs_the_temporary_file_once_before_the_rename`, which pins the steps to `["sync", "rename"]`. No parity step cuts power. |
| d6fb78845 | read an unreadable boot marker as absent | in Rust | Python `src/runtime/task_recovery.py:58-74` reads a marker that is not UTF-8, not JSON, not an object, has no string `pass_started_at` or has a bad timestamp as absent, logs `boot_recovery_marker_unreadable` and checks every node. Rust `rust/crates/charliebot-tasks/src/recovery.rs` (`read_boot_recovery_marker`, `parse_boot_recovery_marker`) does the same and keeps the other read errors. Tests: `tests/recovery.rs::an_empty_boot_marker_reads_as_absent` and the six sibling `a_boot_marker_…_reads_as_absent` tests. Known difference: a bare `NaN` marker logs `JSONDecodeError` where Python logs `TypeError`; both read it as absent. No parity step writes a corrupt marker. |
| fff75daa7 | restrict every pytest run to the processes it started | no behavior | Adds `src/infra/pytest_sandbox.py`, which only `tests/test_pytest_sandbox.py` imports; no server, route or CLI path reads it. |
| afa5386b3 | cover sandbox signal and startup failures | no behavior | Changes only the test-only `src/infra/pytest_sandbox.py`; no importer outside the tests. |
| f3ac007d5 | keep sandbox check portable and show its ABI | no behavior | Changes only the test-only `src/infra/pytest_sandbox.py`; no importer outside the tests. |
| 19f2f0a1d | count markdown link labels | in Rust | Rust `rust/crates/features/web_reply/src/reply.rs:339-357` counts each `[label](url)` as its label (`visible_reply_chars`, `markdown_link_at` at `:361`), and `reply.rs:393` takes that count as `chars`, which the held log, the stored reply and the readback use. Python: `src/features/web_reply/reply.py:47`, `:78-80`, `:205-236`. Test: `a_markdown_link_counts_as_its_label_and_a_bare_url_counts_in_full` (`reply.rs`). Rust has no replay branch for a repeated `request_id`, so the replay half of the Python test has no Rust counterpart. |
| 88fb1ef79 | give the markdown link pattern its own comment | no behavior | A comment line at `src/features/web_reply/reply.py:46`; no code changes. |
| 75415f483 | drop test-only reset helpers from production modules | no behavior | Removes `_reset_user_agent_for_tests` (`src/backends/claude_code/usage_quota.py`) and `reset_run_state_for_tests` (`src/runtime/thinking_state.py`); no `.py` file under `src/`, `server.py` or `tests/` names either one. |
| 1fed72df6 | exclude cron roots from group previews | in Rust | Client JavaScript only: `web/static/js/sidebar/groups.js` and `src/features/cron/static/cron.js`. Rust serves both from the tree (`rust/crates/charliebot-server/src/routes/static_mounts.rs:30` and `:57-67`). The diff adds no fetch; the `schedule_task` field it reads comes from the session list that `rust/crates/charliebot-session-api/src/listing.rs:194-201` fills. |
| 44ffb4596 | leave the open root unmarked in group previews | in Rust | Client JavaScript only: `web/static/js/sidebar/groups.js`, served from the tree by `rust/crates/charliebot-server/src/routes/static_mounts.rs:30` and `:57-67`. The diff adds no fetch. |
| a44eb81cc | honor configured Claude Code effort | in Rust | Rust `rust/crates/charliebot-agents/src/backends/anthropic_compatible/options.rs:21,57` accepts `effort`, `factory.rs:43` passes it to `AnthropicEndpointBackend::new` (`claude_code/claude_code.rs:381`), and `build_command` adds `--effort` only when it is set (`claude_code/claude_code.rs:228`). Python: `src/backends/anthropic_compatible/options.py:16`, `factory.py:22`, `src/backends/claude_code/claude_code.py:296-297`. Test: `effort_reaches_the_claude_command_only_when_the_option_sets_it` (`anthropic_compatible/factory.rs:82`). |
| e8e5d0739 | print the diff link of the landed range from proposal land | no behavior | Python CLI only: `charliebot memory proposal land` prints one more `key: value` line, `diff_path` (`src/features/memory/memory_proposal.py:508`, `src/features/memory/cli.py:272-273`). It runs local git and calls no route, and the Rust server owns no memory proposal (`rust/docs/contract/state/cli.md`). |
| 208bf0441 | keep non-ASCII characters as written in CLI JSON output | no behavior | Python CLI output encoding (`src/runtime/cli/common.py:433-440`, `ensure_ascii=False`). Every route that the CLI calls answers as before, and the CLI stays Python. |
| 5a446f947 | print only session_id and run_id from delegate | no behavior | `charliebot delegate` prints two keys (`src/runtime/cli/delegate.py:199`). `POST /api/internal/delegate` answers as before, and the CLI stays Python. |
| c5373fc71 | describe JSON output accurately | no behavior | A docstring change at `src/runtime/cli/common.py:434`; no code change. |
| 1edfffde7 | stop comments naming the deleted session-tree migration | no behavior | Comments and docstrings only: `server.py:238`, `src/runtime/home_writer_fence.py:3-4` and `src/features/usage/storage_cool.py:185-215`. |
| 257e1ca1e | fail over temporary backend errors | in Rust | Python `src/runtime/backend_pool.py:61-78` lists the temporary-unavailable patterns, and `src/runtime/worker.py:520-523` raises `QuotaExhaustedError` on a match, so the pool cools the member and fails over. Rust `rust/crates/charliebot-agents/src/backend_pool.rs:59-73` lists the same patterns in Python's order, and `:313-316` classifies an error message. `rust/crates/charliebot-tasks/src/worker.rs:804-808` raises `QuotaExhaustedError` on a match, and `execution.rs` `fail_over_quota` moves the pool-chosen node. Tests: `at_capacity_error_fails_over_on_the_worker_path` and six more `*_fails_over_on_the_worker_path` tests, and `step_limit_error_with_bad_gateway_content_does_not_fail_over` and three more negative cases (`rust/crates/charliebot-tasks/tests/worker_run.rs`). |
| a86a910a7 | honor optional model context limits | in Rust | Rust accepts `context_limit` (`rust/crates/charliebot-agents/src/backends/opencode/options.rs:22,41`) and builds the `providers` block in `model_overrides_config` (`options.rs:69`). The one-shot config adds the block of its own option (`backend.rs:202,875-876`). The shared server adds the block of every recorded option when it starts (`server.rs:154,183,195`). Python: `src/backends/opencode/options.py:14,17-34`, `opencode.py:540-548`, `server.py:83`. Tests: `context_limit_reaches_the_one_shot_and_the_shared_server_configs`, `conflicting_context_limits_fail_the_shared_server_start_naming_both_options`, `headless_providers_merge_equal_limits_and_keep_distinct_models` (`test_backend.rs`). `tools/check_opencode_context_limit.py` is Python and has no Rust port. |
| 24642edf5 | defer config import in server | no behavior | Moves the `config` import from module level into `headless_config` (`src/backends/opencode/server.py:60-64`). Import timing only; the context-limit output this function now adds is the gap of a86a910a7. |

## Catch-up 3c4591bad open steps

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 3c4591bad ignored Cargo tests

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 3c4591bad unported commits

Each of the 7 commits in `git rev-list --no-merges 24642edf5..3c4591bad -- src server.py web` has one class.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| b980e34aa | keep the opencode config import at module top | no behavior | Moves the `config` import to module scope in `src/backends/opencode/server.py`; this changes import timing only and no request or CLI output. |
| c9cc1aef3 | scale snapshot usage to the context window | in Rust | Rust `rust/crates/charliebot-usage/src/usage.rs:364-379` uses `limit.context` as the full scale and derives the compaction line from the registered `snapshot` reserve bounded by `output`, through `snapshot_compact_at` (`usage.rs:411-416`). The input branch, the five-field token sum and the tier order are unchanged. Python: `src/runtime/session_usage.py:301-334`. Tests: `snapshot_tier_uses_the_context_window_and_the_reserve_bounded_by_output` (`usage.rs:841`) and `snapshot_tier_reads_the_registered_compaction_reserve` (`usage.rs:870`). No parity step supplies a context snapshot with this limit shape. |
| 44ecfaed1 | strip gateway env from every cc-claude launch | in Rust | Rust `without_gateway_env` (`rust/crates/charliebot-agents/src/backends/claude_code/claude_code.rs:431`) drops the nine gateway and model variable names (`GATEWAY_ENDPOINT_ENV_NAMES`, `:401-413`) from `ClaudeCodeBackend::prepare_env` (`:243`) and from the one-shot spawn environment (`one_shot_env`, `:257`). Python: `src/backends/claude_code/claude_code.py:112-144,327,362`. Tests: `prepare_env_strips_an_inherited_cc_anthropic_compatible_worker_env`, `one_shot_text_spawns_claude_without_the_gateway_variables`, `endpoint_env_overrides_every_inherited_gateway_variable` (`claude_code/claude_code.rs:791,820,980`). |
| 349aa4d92 | test inherited gateway values during env preparation | no behavior | Refactors the same endpoint and model variable names into one tuple in `src/backends/claude_code/claude_code.py:115-130` and adjusts tests; the env values are unchanged by this refactor. The stripping behavior is the separate 44ecfaed1 change. |
| 2d25e4abb | fail over on temporary Claude result errors | in Rust | Python `src/runtime/backend_pool.py:61-90` classifies temporary result errors, and `src/runtime/worker.py:523-527` raises the pool failover error. Rust `rust/crates/charliebot-agents/src/backend_pool.rs:321-328` classifies a Claude `result` by its `api_error_status`, then by its text when the status is null or absent. `rust/crates/charliebot-tasks/src/worker.rs:811-818` raises `QuotaExhaustedError` for an errored RESULT, and a temporary-status result with no `result` field fails the run with an error, as Python's KeyError does. Tests: `claude_result_502_fails_over_on_the_worker_path` and three more, `claude_result_400_reasoning_error_does_not_fail_over` and three more negative cases (`rust/crates/charliebot-tasks/tests/worker_run.rs`). |
| 064b502ca | route configured Anthropic-compatible request headers | in Rust | Rust `rust/crates/charliebot-agents/src/backends/anthropic_compatible/options.rs:27` accepts `headers` as an ordered name and value list, `factory.rs:48` passes it to `AnthropicEndpointBackend::with_headers` (`claude_code/claude_code.rs:472`), and its `prepare_env` sets `ANTHROPIC_CUSTOM_HEADERS` to newline-separated `name: value` lines (`claude_code/claude_code.rs:516-520`). Python: `src/backends/anthropic_compatible/options.py:17`, `factory.py:23`, `src/backends/claude_code/claude_code.py:404-412`. Tests: `factory_routes_custom_headers`, `factory_routes_multiple_custom_headers`, `factory_omits_custom_headers_when_the_option_has_none_and_the_parent_sets_them` (`anthropic_compatible/factory.rs:102-121`), `claude_model_env_sets_exactly_five_model_variables` (`claude_code/claude_code.rs:1011`). |
| c7fb09d10 | spawn master wakes through one logged-task helper | no behavior | Replaces the repeated `tasks.create_logged_task(trigger_master(...), name=...)` calls in `src/features/chat_threads/thread_entry.py` and `src/features/web_reply/audit.py` with `master_trigger.spawn_master_turn`, which calls the same helper with the same arguments and task name. |

## Catch-up d1f7e3ba6 open steps

This port removes the 99 `4c3bd3fda` rows for model provisioning and online upload and the two `93b427659` rows for the removed confirm route. No row remains, so the all-stage Rust debug run is expected to fail no step.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |


## Catch-up d1f7e3ba6 ignored Cargo tests

No Cargo test fails on a main change, so no `#[ignore = "catch-up d1f7e3ba6: <row>"]` remains. R254 and R255
are gone: the Python reference helper clears the account readings itself. R256 to R260 are gone: the Rust
`metadata.json` writer carries `resource_class`. The other two ignored tests were ignored before this catch-up.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up d1f7e3ba6 unported commits

Each of the 30 commits in `git rev-list --no-merges 3c4591bad..d1f7e3ba6 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| d5b194146 | add safe cleanup for old task paths | in Rust | Ports the scheduled `disk_sweep` handler and `paths.scratch_dir`; the Python CLI stays in place. The package mount remains empty like Python's. No parity scenario drives the sweep. Rust implementation: `rust/crates/features/disk_sweep/src/lib.rs:16` and `rust/crates/features/disk_sweep/src/sweep.rs:129`. |
| b3a121bf0 | drop two module constants nothing reads | no behavior | Removes two unread module constants from `src/features/memory/memory_proposal.py` and `src/infra/pytest_sandbox.py`. |
| 7fa53b91b | drop the deleted speech service from the preload docstring | in Rust | Rust registers no speech service in `rust/crates/features/voice/src/lib.rs:22-29`; startup has no voice service to preload. |
| 8cbc40276 | drop the dead transcription registry setter | in Rust | Backend ids and selection have one owner in `rust/crates/features/voice/src/recording.rs:9-10,118-137`; no mutable registry setter is needed. |
| 224e4e5cf | report a missing session slice as unavailable at startup | in Rust | `log_session_cgroup_startup` warns `session_cgroup_unavailable` with "spawns with a session fail" (`rust/crates/charliebot-agents/src/transport/cgroup.rs:312`); `ensure_session_cgroup` fails the spawn on a missing or unwritable slice (`rust/crates/charliebot-agents/src/transport/cgroup.rs:75`). Python source: `src/infra/process.py:114-119,142-179,408-416`. |
| 3530285d8 | drop the unused session_cgroup_path helper | in Rust | Python dropped the helper. The Rust `session_cgroup_path` (`rust/crates/charliebot-core/src/cgroup.rs:78`) is the join that `ensure_session_cgroup` and `cleanup_session_cgroup` use, so no unused helper remains and no behavior changes. |
| 8fccbda46 | give the session join script the slice's OOM priority | in Rust | The turn's environment carries `CHARLIEBOT_SESSION_OOM_SCORE_ADJ` (`rust/crates/charliebot-agents/src/backends/opencode/backend.rs:553`, set in `put_environment` at `:535`). The join script `src/backends/opencode/cgroup_join.sh` reads it. No parity step reads the priority. |
| df7920bd7 | isolate server, agent and repair session shares | in Rust | `ResourceClass` and the slice names (`rust/crates/charliebot-core/src/cgroup.rs:18`); the metadata field (`rust/crates/charliebot-store/src/metadata.rs:429`); the class walk (`rust/crates/charliebot-store/src/resource_class.rs:23`); the PUT route (`rust/crates/charliebot-session-api/src/writes.rs:51`); the spawn placement and OOM writes (`rust/crates/charliebot-agents/src/transport/cgroup.rs:180`, `:217`; `rust/crates/charliebot-agents/src/transport/spawn.rs:829`). The all-stage run passes its 12 single-commit rows; its 17 two-commit rows stay open on `9b83e3b1f`'s raw-log cursor. Python source: `src/runtime/resource_class.py`, `src/runtime/api/sessions.py:1357`. |
| 1043df01b | single-home the archive operator refusal | no behavior | Moves the archive operator refusal into `ARCHIVE_OPERATOR_REFUSAL` (`src/runtime/task_sessions.py`). The 403 detail keeps its text. |
| 4c3bd3fda | transcribe uploads through the online backend | in Rust | Rust removes the local engine and speech service (`rust/crates/features/voice/src/lib.rs:22-29`), persists and dispatches uploads through online backends (`api.rs:110-177`, `recording.rs:101-143`), and links no speech runtime. |
| 93b427659 | drop the browser's confirm probe and local-engine note | in Rust | `rust/crates/features/voice/src/api.rs:37-40` registers only the upload path; `matches` also refuses the removed confirm route (`api.rs:43-48`). |
| 2bb5b3057 | stabilize vfork inputs across GIL release | no behavior | Stabilizes the vfork inputs in `src/runtime/agent_process/_vfkspawn.c`. Rust spawns with its own `clone_vfork_exec` (`rust/crates/charliebot-agents/src/transport/spawn.rs:171`), which has no GIL to release. |
| 127f1e315 | release the GIL across the vfork spawn stub's exec wait | no behavior | Releases the GIL across the vfork stub's exec wait in `src/runtime/agent_process/_vfkspawn.c`. Same as `2bb5b3057`: Rust spawns with its own `clone_vfork_exec`. |
| 55c59af46 | single-home the pool walk behind backend_pool.iter_members | no behavior | Single-homes the pool walk behind `backend_pool.iter_members` (`src/runtime/backend_pool.py`, `autonamer.py`, `cold_read.py`). Refactor only. |
| 6356179b7 | run cc-anthropic-compatible one-shots on the gateway | in Rust | An endpoint one-shot launches with the environment of the entry's runs (`AnthropicEndpointBackend::one_shot_env`, `rust/crates/charliebot-agents/src/backends/claude_code/claude_code.rs:485`) and replaces `--model` with `--effort` (`one_shot_model_args`, `claude_code.rs:490`); the shared launch and process handling is `run_one_shot_process` (`claude_code.rs:289`). A cc-claude one-shot keeps its stripped environment and `--model` (`claude_code.rs:257,262`). Python: `src/backends/claude_code/claude_code.py:367-419,477-483`. Tests: `one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag`, `one_shot_text_omits_effort_when_the_entry_has_none`, `one_shot_text_spawns_claude_without_the_gateway_variables` (`claude_code.rs`). |
| 8db76cdc5 | single-home the claimed-batch input composition | no behavior | Single-homes the claimed-batch input composition (`src/runtime/task_execution.py`). Refactor only. |
| d1a7faccc | use v2 model body overlay | in Rust | The model's request-body fields ride the entry key `body` (`rust/crates/charliebot-agents/src/backends/opencode/options.rs:94`). Python: `src/backends/opencode/options.py:35-37`. Test: `model_overrides_config_carries_extra_body_at_the_model_body_key` (`options.rs`). |
| 5d8cbc408 | let an option add fixed fields to every request body | in Rust | `extra_body` sits beside `context_limit` on `OpenCodeOption` (`rust/crates/charliebot-agents/src/backends/opencode/options.rs:24,49`); `model_overrides_config` writes it under the model's `body` (`options.rs:69,94`), and `declare` names both options on a conflict (`options.rs:105`). The shared server (`server.rs:195`) and the one-shot config (`backend.rs:202`) carry the block. Python: `src/backends/opencode/options.py:16,20-51`, `server.py:60-82`, `opencode.py:540-548`. Tests: the five `model_overrides_config_*` cases (`options.rs`), `config_loads_extra_body_on_an_endpoint_and_an_opencode_option` (`mod.rs`). |
| bd29a7b7a | single-home the created-report announce-and-wake | no behavior | Single-homes the created-report announce-and-wake (`src/runtime/session_dispatch.py`, `task_execution.py`). Refactor only. |
| f39a200a3 | let cc-anthropic-compatible entries send a fixed extra body | in Rust | `CcAnthropicCompatibleOption` accepts `extra_body` (`rust/crates/charliebot-agents/src/backends/anthropic_compatible/options.rs:29,87`), the factory forwards it (`factory.rs:49`), and `CLAUDE_CODE_EXTRA_BODY` joins the gateway names (`claude_code.rs:401-415`) so the gateway strip removes it for cc-claude. `prepare_env` writes it when non-empty (`claude_code.rs:523`). Python: `src/backends/anthropic_compatible/options.py:18`, `factory.py:24`, `src/backends/claude_code/claude_code.py:117,131-134,439-440,462-463`. Tests: `factory_routes_extra_body`, `factory_omits_extra_body_when_the_option_has_none_and_the_parent_sets_it` (`factory.rs`), `config_loads_extra_body_on_an_endpoint_and_an_opencode_option` (`mod.rs`). |
| d3ba6657c | single-home the kill-and-reap helper | no behavior | Single-homes the kill-and-reap helper (`src/features/trace/`). Refactor only. |
| 7b067fe51 | drop the stale derived-value count from the row-memo comment | no behavior | Drops a stale count from a comment in `src/runtime/api/sessions.py`. |
| 2989c6b9b | single-home the pending-report recovery scan | no behavior | Single-homes the pending-report recovery scan (`src/runtime/session_dispatch.py`). Refactor only. |
| d0a6ea6a9 | declare the freshness wrappers' returned thread read | no behavior | Rewrites the return annotation of `assert_thread_fresh` in the Slack and Discord listeners. Annotation only. |
| 1d9169512 | drop two code-construct counts that drift | no behavior | Drops two drifting counts from comments in `server.py` and `src/features/memory/cli.py`. |
| 72cf7dbf6 | single-home the reply's caller-claims derivation | no behavior | Single-homes the reply's caller-claims derivation (`src/features/chat_threads/thread_entry.py`). Refactor only. |
| fb4529dc3 | single-home the backend pool's skip check | no behavior | Single-homes the backend pool's skip check (`src/runtime/backend_pool.py`). Refactor only. |
| fa4ac13b9 | move the remaining test resets out of production modules | no behavior | Moves the remaining test resets out of three production modules (`src/backends/claude_code/claude_accounts.py`, `src/runtime/backend_pool.py`, `src/runtime/sidebar_state.py`). The Rust claude-pool reference helper now clears the same four account registries itself (`rust/crates/charliebot-agents/tests/claude_pool_reference.py:23-28`), so R254 and R255 run again. |
| b03c38579 | scan the echoed raw log off the loop | in Rust | Scans the echoed raw log on a thread (`src/runtime/midturn_delivery.py`). The scan result is unchanged. Rust `echoed_input_ids` (`rust/crates/charliebot-tasks/src/midturn_delivery.rs:80`) parses the raw lines and `claim_finish_inputs` (`:127`) rides `tokio::task::spawn_blocking` (`:139`), so the scan never runs on an async worker thread. Test: `manager_finish_claims_only_echoed_pending_user_ids` (`rust/crates/charliebot-tasks/tests/dispatch.rs`). |
| 9b83e3b1f | deliver messages during manager turns | in Rust | Delivers messages during manager turns through stream-json stdin with the replay echo (`src/runtime/midturn_delivery.py`, `src/backends/claude_code/claude_code.py`). The delivery state lives in `rust/crates/charliebot-agents/src/backends/claude_code/midturn_delivery.rs`: `launch_kwargs` names the gated Run (`:93`), `write_initial_prompt` writes the first stream-json user line (`:160`), `offer` writes each composed input under the stdin write lock (`:199`), `translate_replay` turns a replay into the non-rendered `input_delivered` event (`:231`), and `on_backend_event` closes stdin at the first result (`:258`). `rust/crates/charliebot-agents/src/backends/claude_code/claude_code.rs:256` adds `--input-format stream-json --replay-user-messages`, and `rust/crates/charliebot-agents/src/runner.rs:289` defers the initial write until the active process exists. The dispatcher half lives in `rust/crates/charliebot-tasks/src/midturn_delivery.rs` (`offer` `:32`, `record_ack` `:57`, `claim_finish_inputs` `:127`, `fresh_translate` `:153`) and `claim_echoed_user_inputs` (`rust/crates/charliebot-tasks/src/dispatch.rs:314`) claims the echoed pending ids. `record_ack` also appends the acknowledged id to the queue's running-id binding (`rust/crates/charliebot-tasks/src/master_queue.rs:338`), so a web reply posted while the turn runs answers the delivered message, as Python's live `_current_items` item does; `running_turn_inputs_returns_all_running_inputs_in_arrival_order` guards it. Tests: `midturn_offer_writes_only_web_user_messages_and_stops_at_result`, `replay_becomes_non_rendered_input_delivered_event`, `replay_after_result_resets_post_result_timeout`, `pooled_relay_turn_uses_the_same_midturn_launch_arguments`, `thread_turn_keeps_plain_text_stdin_and_queues_later_messages`, `manager_finish_claims_only_echoed_pending_user_ids`, `input_admitted_during_an_active_run_dispatches_after_its_finish_for_every_session_kind`. |

## Catch-up d113f2c4d open steps

The status reminder (`a98ee7ad0`), web status `since` fields (`a2b835107`, `a98ee7ad0`), and archive behavior (`a31877580`, `a1c996ec4`) are ported, and `task-completion-routes` declares the `either_order_of_adjacent_events` rule for the status reminder and the audited turn's `run_finished`, so no row remains. Python does not order that pair, and the scenario compares it in either order as the other scenarios with scripted reminder starts do. No row is intermittent.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up d113f2c4d ignored Cargo tests

No ignored Cargo tests remain for catch-up `d113f2c4d`: R337 is covered by the Gemini tool-call port, and R338-R339 are covered by the archive port.

## Catch-up d113f2c4d unported commits

Each of the 23 commits in `git rev-list --no-merges d1f7e3ba6..d113f2c4d -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above and in the ignored table.
Constraint 2 covers `7e66ebb12`, `36333d884`, `dd081ea57` and `71e25b4ae`. `36333d884` was the only unported one: its row
names its Rust count tests and the one part it leaves open, the web reply's cold read. `dd081ea57` names its Rust guard.
The other two are in Rust without a count note.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| ad7ae0556 | drop the stale call-site count from the metadata funnel | no behavior | Comment only. |
| 1322d45a0 | state behavior without a "today" anchor in tests, tools and web | no behavior | Comment and text only. |
| a2b835107 | close status review gaps | in Rust | The plan-link lookup reads a registry that exists but does not read as a failure (`rust/crates/features/web_reply/src/plan_links.rs:86`), the item's own link field joins the scanned links (`plan_links.rs:235`), and the unread query checks the prospective items' plan links (`rust/crates/features/web_reply/src/status.rs:399`). Python: `src/features/web_reply/plan_links.py:66`, `:136`, `src/features/web_reply/status.py:325`. |
| a98ee7ad0 | keep Waiting on you items current | in Rust | The status reminder and the combined reminder (`rust/crates/features/web_reply/src/audit.rs:30`, `:36`, posted at `:236`), the `since` and `since_exact` fields and their pairing (`rust/crates/features/web_reply/src/status.rs:71`, `:208`, `:246`), the stale-plan-link refusal (`rust/crates/features/web_reply/src/plan_links.rs:225`, `status.rs:233`), and the confirm route (`rust/crates/features/web_reply/src/api.rs:88`). Python: `src/features/web_reply/audit.py`, `status.py`, `plan_links.py`, `api.py`. Tests: the reminder, plan-link, since and confirm cases of `status.rs`, `audit.rs` and `turn_contribution.rs`. |
| a1c996ec4 | keep archive run evidence optional | in Rust | A run already judged success or failure satisfies the archive evidence rule: `evidence_blockers_with_success` (`rust/crates/charliebot-tasks/src/completion.rs:240`) skips the run-evidence blocker when the close carries a succeeding run id, and `close_now_inner` passes the approved run ids for an archived close. Tests: `own_run_archive_closes_without_takeoff_and_reports_to_ancestor` (`rust/crates/charliebot-tasks/tests/completion.rs`). |
| a31877580 | make archive the only manual close action | in Rust | The completion owner holds the archive writes: `archive_task`, `archive_subtree`, and `archive_replaced_source_locked` (`rust/crates/charliebot-tasks/src/completion.rs:452,541,682`), plus `archive_unsuccessful_worker` (`:714`). The store routes elone source closure through `ReplacedSourceArchive` (`rust/crates/charliebot-store/src/session_fork.rs:46`). The route is `POST /api/sessions/{session_id}/archive` (`rust/crates/charliebot-tasks/src/routes.rs:53,1019`), `POST /complete` answers 404 and `DELETE /api/sessions/{session_id}` answers 405. |
| 20a051cda | deliver a close's parent report through one helper | no behavior | Refactor: `_announce_close_and_wake_parent` keeps the behavior. |
| 5a3a38f6d | state current behavior instead of "today" in comments | no behavior | Comment only. |
| 70e150753 | drop change narration from seven comments and docstrings | no behavior | Comment and docstring only. |
| 532535644 | drop three stale citations of deleted test coverage | no behavior | Comment only. |
| 5196ea224 | drop the move narration from three docstrings | no behavior | Docstring only. |
| c29fb681c | name the Run directory, not the deleted thread data directory | no behavior | Docstring only. |
| 6778ffd9f | drop docstring references to deleted code | no behavior | Docstring only. |
| b23e14170 | drop the test-only work_state_of forwarder | no behavior | Removes a forwarder with no production caller. |
| fb31d1360 | delete two zero-reference helpers | no behavior | Deletes helpers with no references. |
| 71e25b4ae | release the GIL inside the tree walks and between rows | in Rust | The Rust tree and event walks hold no GIL (`rust/crates/charliebot-session-api/src/tree.rs:34`). Constraint 2 (chat-event cache). The commit adds no count test. |
| dd081ea57 | keep an append exactly once when a cold fill runs on a thread | in Rust | The append holds the session's cache lock across its write (`rust/crates/charliebot-store/src/events.rs:198`), and a cold fill takes the same lock (`events.rs:360`). Constraint 2 (chat-event cache). Its count test `test_cold_events_fill_on_a_thread_keeps_an_append_exactly_once` has the Rust guard `cold_events_fill_on_a_thread_keeps_an_append_exactly_once` (`rust/crates/charliebot-store/src/events.rs:1662`); it passes on this branch. |
| 0769b0c85 | slot Gemini's index-less stream tool calls at index 0 | in Rust | `tool_delta` assigns index 0 for Gemini at `rust/crates/charliebot-agents/src/backends/openai_compatible/translate.rs:525` and splits reused indexes by tool id at `:536`. Tests: `test_stream_tool_call_without_index_uses_zero_for_idless_continuations`, `test_gemini_stream_index_less_delta_with_signature_yields_one_tool_block`, `test_gemini_stream_two_index_less_calls_split_by_id`. No parity step drives it. |
| 0b4e7b337 | preserve tool input schemas | in Rust | Tool schemas pass through (`rust/crates/charliebot-agents/src/backends/openai_compatible/translate.rs:258`). |
| 72f3ed8d2 | reject stream tool calls without an index | in Rust | The stream translator rejects a tool call without an index (`translate.rs:526`). |
| ee62f6dbb | support Gemini Claude Code tool calls | in Rust | Request translation is at `rust/crates/charliebot-agents/src/backends/openai_compatible/translate.rs:24`, non-stream responses at `:307`, and streaming at `:381`. Rust tests named after the Python counterparts pass, and the formerly ignored route matrix now passes all 516 cases. |
| 36333d884 | compute session-tree rows on a worker thread from bounded reads | in Rust | Session-tree rows and detail read each node through one bounded pass: `Store::task_tree_rows` (`rust/crates/charliebot-store/src/task_tree.rs:1144`, under `tree_rows_lock` at `store.rs:213`) is called by `tree_page` (`rust/crates/charliebot-session-api/src/tree.rs:47`). The history tail is read by `visit_chat_facts_from_end` (`rust/crates/charliebot-store/src/events.rs:298`), and every memo keys on the history identity. Python's count tests and their Rust counterparts, all in `task_tree.rs`: `test_tail_without_lifecycle_fact_reads_the_deep_fact_without_caching_history` → `task_state_tail_reads_the_deep_fact_without_caching_history` (4168); `test_tail_without_lifecycle_fact_reads_an_archived_fact_without_caching_history` → `task_state_tail_reads_an_archived_fact_without_caching_history` (4185); `test_queued_run_with_a_stop_request_is_idle_and_caches_no_history` → `queued_run_with_a_stop_request_is_idle_and_caches_no_history` (4221); `test_launched_run_with_a_live_pid_and_no_terminal_fact_is_running_and_caches_no_history` → `launched_run_with_a_live_pid_and_no_terminal_fact_is_running_and_caches_no_history` (4237); `test_run_with_ended_at_is_settled_without_reading_its_history` → `run_with_ended_at_is_settled_without_reading_its_history` (4255); `test_tree_rows_computed_across_an_append_equal_the_rows_before_or_after_it[False]` and `[True]` → `tree_rows_computed_across_an_append_equal_the_rows_before_or_after_it_before_stat` (4327) and `_after_stat` (4332). Not ported: the web reply's cold read, `read_fact_history` in `src/features/web_reply/reply.py`. `post_web_reply_with_request_id` (`rust/crates/features/web_reply/src/reply.rs:417`) still loads the full history on the request task under the control lock, so a cold read holds a runtime worker. |
| 7e66ebb12 | walk typed facts from the end without caching them | in Rust | Rust tail reads seek from the end of the file (`rust/crates/charliebot-store/src/events.rs:348,1161-1188`). Constraint 2 (bounded reads). |

## Catch-up 085f024a5 open steps

The port fixes the run-miss row (R340), and the catch-up's other behavior gap, the pool refusal line of `7ce86c7a2`, reaches no parity step. No row remains, so the all-stage Rust parity run is expected to fail no step.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 085f024a5 ignored Cargo tests

No Cargo test fails because Rust lacks a behavior of this catch-up: the full `cargo-slot.sh test --locked` passes at the merged head.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 085f024a5 unported commits

Each of the 28 commits in `git rev-list --no-merges d113f2c4d..085f024a5 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| 085f024a5 | give the platform line one owner | no behavior | `ThreadPlatform.platform_line` (`src/features/chat_threads/thread_entry.py:133-137`) renders the same sentence the deleted `_PLATFORM_LINE` constants spelled: both records feed the same `display_name`, `reply_command` and `max_post_chars`. |
| 596abd578 | give the duplicate-registration refusal one owner | no behavior | `registration.already_registered` (`src/runtime/hooks/registration.py:11-12`) renders the same `"{kind} {name!r} is already registered by {owner}"` sentence every registry spelled inline; a double registration stays a startup-time programmer error no client reads. |
| 269322f70 | give the run-miss sentence one owner | in Rust | `run_not_found_in_task_text` (`rust/crates/charliebot-store/src/run_records.rs:1236`) owns `run <run id> not found in task <session id>`; the Python sites (`src/runtime/runs.py:681`, `src/runtime/task_sessions.py:1489`, `src/runtime/session_dispatch.py:422,448`) map to `require_run` (`rust/crates/charliebot-store/src/run_records.rs:1240`), `rust/crates/charliebot-tasks/src/task_sessions.rs:25`, `rust/crates/charliebot-tasks/src/dispatch.rs:289,330`, and the same-owner Rust sites `rust/crates/charliebot-tasks/src/execution.rs:140,247,809,1344,2071,2321,2379,2502`, `rust/crates/charliebot-tasks/src/recovery.rs:865`, `rust/crates/charliebot-tasks/src/completion.rs:1830`, `rust/crates/charliebot-tasks/src/prompts/run_context.rs:45`. The retry route's 404 detail is covered by the parity step `manager-turn-execution-and-routes | retry an unknown run`, which now passes. |
| 7ce86c7a2 | give the pool and run-record refusal lines one owner | in Rust | `BackendPool::no_member_message` (`rust/crates/charliebot-agents/src/backend_pool.rs:309`) owns lowercase `no backends.pool member is available: <recovery report>`; the worker failure report site (`src/runtime/task_execution.py:2202-2203`) maps to the quota-failover summary (`rust/crates/charliebot-tasks/src/execution.rs:1432`), and the same-owner Rust sites are subagent resolution (`rust/crates/charliebot-tasks/src/spawner_backends.rs:145`) and the improve loop's per-iteration pick (`rust/crates/charliebot-sched/src/improve/sequence.rs:421`). The failover test `failover_with_every_member_cooling_reports_each_recovery_time` asserts the full sentence. Rust's run-record readers keep their inline `run metadata unreadable at` refusals beside the readers (`rust/crates/charliebot-store/src/run_identity.rs:46,51,60,87`, `rust/crates/charliebot-store/src/run_records.rs:1209`); no parity step reaches them. |
| eb85728c2 | give the reply budget one owner | no behavior | `constants.REPLY_BUDGET_CHARS = 500` (`src/infra/constants.py:59-62`) replaces the two identical module constants; every `over_budget` comparison reads the same 500. |
| c604edf66 | give the task's session lookup one owner | no behavior | `Scheduler._cron_sessions` (`src/features/cron/scheduler.py:228-244`) returns the same cached-or-fresh list the two callers spelled inline; the cache edit-through semantics carry over unchanged. |
| 9081c1c97 | give the failed-run report text one reader | no behavior | `TaskExecutionAdapter._failure_text` (`src/runtime/task_execution.py:2503-2519`) keeps each caller's reader order: error-then-summary for a cold-read attempt, summary-then-error for the worker failure summary. |
| ba581c72c | drop the parameters three callees ignore | no behavior | `next_run_iso` already computed from the wall clock, and `_delegate_cold_read` and `check_setup` never read their config; the dropped arguments changed no answer. |
| 5b81faa7f | align cold read question count in comments and docstrings | no behavior | Comments and docstrings only; the questions ride `prompts/cold_read.md`, which both servers read from the repo. |
| e773f00ea | require a separate run variable block | no behavior | The changed `run-stated` assertion (`_check_run_stated`, `src/features/artifacts/artifact_check.py:545-551`) belongs to the `sitrep` set, which the Python CLI runs (`src/features/artifacts/cli.py:141`); the Python server runs only the `plan` set (`src/features/artifacts/plans.py:306`). Rust's `run_assertions_sync` serves the `plan` set only (`rust/crates/features/artifacts/src/artifact_check.rs:151-168`), so no server behavior changes. |
| 6f2070b51 | require a sitrep to state its run's variables and gloss requirement chips | no behavior | The `run-stated` and req-chips assertions (`_check_req_chips`, `src/features/artifacts/artifact_check.py:436-457`) belong to the `sitrep` set, which the Python CLI runs (`src/features/artifacts/cli.py:141`); the Python server runs only the `plan` set (`src/features/artifacts/plans.py:306`). Rust's `run_assertions_sync` serves the `plan` set only (`rust/crates/features/artifacts/src/artifact_check.rs:151-168`). The cold-read prompt rides `prompts/cold_read.md`, which Rust's `cold_read::load_texts` reads from the repo, so that part already carries over. |
| cf62c85e6 | cite the 3.14 forkserver preload instead of a removed spawn hook | no behavior | Module docstring comment only (`src/features/trace/trace_merge_child.py`). |
| 0d16ce6ef | one owner for the review context's inputs | no behavior | `_review_inputs` (`src/runtime/task_execution.py:1776-1790`) returns the same task record, work summary and two log paths the two callers spelled inline. |
| eac1c2ffe | annotate the parameters and returns src left bare | no behavior | Annotations only, across eight modules. |
| 19f682e1c | find the leaf slices inside charliebot.slice and enable their controllers | in Rust | `user_slice_dir` nests each leaf slice inside `charliebot.slice` (`rust/crates/charliebot-core/src/cgroup.rs:61`), and `session_slice_dir` feeds both the spawn path and the startup sweep (`:72`, `:77`). `ensure_session_cgroup` writes `+memory +pids` to the slice's `cgroup.subtree_control` before it creates the session directory (`rust/crates/charliebot-agents/src/transport/cgroup.rs:94`). Rust tests `user_slice_dir_nests_under_each_dash_prefix` (`cgroup.rs:128`) and `ensure_session_cgroup_names_the_failed_subtree_control_write` (`transport/cgroup.rs:397`) cover it. No parity step runs a session cgroup (the sandbox has no cgroup fs). |
| 4f4313b5d | one owner for the refusal's HTTP translation | no behavior | `deps.http_refusal` (`src/runtime/api/deps.py:120-125`) raises the same status and detail each web-reply route spelled inline. |
| 5d2620c0d | one owner for the stat signature spelling | no behavior | `usage_sources.stat_signature` (`src/runtime/hooks/usage_sources.py:58-64`) returns the same `<mtime_ns>:<size>` string the four call sites spelled inline. |
| a587c607e | drop the snapshot parameters audit_round re-reads | no behavior | The body never read `chat_events` or `target`; it re-read the log and re-derived the target before and after. |
| 6c675221d | one home for the no-reviewer blocked report | no behavior | `_no_reviewer_summary` (`src/runtime/task_execution.py:2781-2789`) spells the same sentence the live finish path and the startup replay spelled inline. |
| 10c352392 | one owner for the (module, attr) registration refusal | no behavior | `wiring._register_pair` (`src/runtime/hooks/wiring.py:81-86`) raises the same sentence the four registries spelled inline. |
| a7512488b | name the completion owner for the close facts | no behavior | Comments and docstrings only (`src/infra/event_types.py`, `src/runtime/task_sessions.py`). |
| 72ae80098 | name the real writers of the two tee transport names | no behavior | Comment only (`src/features/usage/storage_cool.py:54-60`). |
| 8ef5d9b5d | drop the stale compaction-reserve comment from opencode | no behavior | Comment removal only (`src/backends/opencode/opencode.py`). |
| 391807d53 | one home for the reply verbs' post-and-print | no behavior | The `charliebot` CLI stays Python; `common.post_reply` (`src/runtime/cli/common.py:493-507`) builds the same `{session_id, text[, keep_language]}` body and prints the same one-line readback for the slack, discord and web reply verbs. |
| b94eaaf97 | drop the test-only audit reply entry point | no behavior | `audit_reply` had no production caller; its steps already ran inside `audit_turn`, and the deleted forwarder changed no client-observable path. |
| 745212d16 | drop the elone params the task-tree owner ignores | no behavior | `run_elone_replacement` never read `event_index` or `backend`; the owner derives both from the source's durable facts. |
| 33e2b27ac | one home for the chat platforms' turn contribution | no behavior | `ThreadTurnContribution` (`src/features/chat_threads/turn_contribution.py:32-70`) composes the same after-turn audit, manager-round contract segment and reply renderer from each platform's record; Slack and Discord subclass it with the same facts. |
| 373969100 | name the worker summary card, not the retired leaf card | no behavior | Comments only (`src/runtime/worker_transcript.py`, `src/runtime/task_execution.py`). |

## Catch-up 4521a51b2 open steps

Both all-stage Rust debug runs failed the same 168 of 1167 steps, and each failing step is a row below: 164 fail because Rust still writes and serves `resource_class` (`637df0572`), and four are the WebSocket cursor steps of `b3dc9a015`. Every failing step failed in both runs, so no row is intermittent. The Python-vs-Python run passed all 1107 of its steps.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |
| manager-turn-cancellation-routes | wait at the chat cancel gate | agent-orchestration-cancel | 637df0572 | R341. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-cancellation-routes | wait for the chat-cancelled run to finish | agent-orchestration-cancel | 637df0572 | R342. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-cancellation-routes | wait at the retried manager run cancel gate | agent-orchestration-cancel | 637df0572 | R343. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-cancellation-routes | wait for the run-cancelled manager retry to finish | agent-orchestration-cancel | 637df0572 | R344. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-cold-read | delegate a cold read of the synthetic page | agent-orchestration | 637df0572 | R345. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-cold-read | let the manager finish the cold-read report | agent-orchestration | 637df0572 | R346. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-cold-read | delegate a cold read whose first reader fails and second answers | agent-orchestration | 637df0572 | R347. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-cold-read | delegate a cold read whose two readers fail | agent-orchestration | 637df0572 | R348. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| task-completion-routes | replay the successful archive request | agent-orchestration | 637df0572 | R349. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| task-completion-routes | cancel the stopped queued worker to deliver its report | agent-orchestration | 637df0572 | R350. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| task-completion-routes | cancel the task after its run finishes | agent-orchestration | 637df0572 | R351. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-delegate | delegate one repo-less script run | agent-orchestration | 637df0572 | R352. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-delegate | delegate a failing repo-less worker | agent-orchestration | 637df0572 | R353. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-delegate | delegate a failing implement worker on the local repository | agent-orchestration | 637df0572 | R354. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-execution-and-routes | wait for the manager turn to finish | agent-orchestration | 637df0572 | R355. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-execution-and-routes | wait at the first manager gate | agent-orchestration | 637df0572 | R356. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-execution-and-routes | release the first manager turn | agent-orchestration | 637df0572 | R357. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-execution-and-routes | wait for the retried manager run to finish | agent-orchestration | 637df0572 | R358. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| manager-turn-opencode | wait for the OpenCode manager run to finish | agent-orchestration | 637df0572 | R359. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| implement-review-chain | delegate a repo task whose unchanged branch already landed on origin main | agent-orchestration | 637df0572 | R360. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| implement-review-chain | delegate a repo-less implement task | agent-orchestration | 637df0572 | R361. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| implement-review-chain | delegate a task whose reviewer exhausts the configured pool | agent-orchestration | 637df0572 | R362. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-session-message | a relay to a live session is accepted | agent-orchestration | 637df0572 | R363. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| agent-orchestration-session-message | let the target manager answer the relay | agent-orchestration | 637df0572 | R364. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| cron-routes-and-ticks | wait for the manual cron firing report | triggers-cron-improve | 637df0572 | R365. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| cron-routes-and-ticks | wait for a scheduler tick to fire the same task again | triggers-cron-improve | 637df0572 | R366. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-artifacts-plan-fork | fork of the plan worker at a null index | features | 637df0572 | R367. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-artifacts-plan-present | session listing after the presentation | features | 637df0572 | R368. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| features-artifacts-plan-verbs | session listing before the approval shows the pending plan | features | 637df0572 | R369. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| features-artifacts-plan-verbs | session listing after the approval clears the pending plan | features | 637df0572 | R370. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| features-artifacts-plans | session listing, pending plan approval flags | features | 637df0572 | R371. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| features-discord-routes | read marks the thread's unread messages | features | 637df0572 | R372. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-harness-discord | server startup | features | 637df0572 | R373. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-harness-discord | allowed user mention starts a Discord thread session | features | 637df0572 | R374. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-harness-discord | release the silent summon result | features | 637df0572 | R375. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-harness-slack | allowed app mention starts its Slack thread session | features | 637df0572 | R376. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-harness-slack | release summon agent after its result | features | 637df0572 | R377. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-slack-ack | acknowledge every eligible follow message | features | 637df0572 | R378. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-slack-backfill | server startup | features | 637df0572 | R379. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply-audit | wait for the audit nudge | features | 637df0572 | R380. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply-language | wait for the audit nudge | features | 637df0572 | R381. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply | a reply to a web session with no running turn | features | 637df0572 | R382. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply | a status set of now alone | features | 637df0572 | R383. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply | a status set with a null goal | features | 637df0572 | R384. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| features-web-reply | a status set stores a waiting item after its cold read | features | 637df0572 | R385. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-admitted-input | release the first manager turn · python-to-candidate | agent-orchestration | 637df0572 | R386. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-admitted-input | wait for the first turn at the gate · candidate-to-python | agent-orchestration | 637df0572 | R387. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-admitted-input | hand over the live manager run · candidate-to-python | agent-orchestration | 637df0572 | R388. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-admitted-input | release the first manager turn · candidate-to-python | agent-orchestration | 637df0572 | R389. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-drain | stop both servers and let the agent exit while none runs · python-to-candidate | agent-orchestration | 637df0572 | R390. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-drain | wait for the raw log cursor at the gate · candidate-to-python | agent-orchestration | 637df0572 | R391. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-drain | stop both servers and let the agent exit while none runs · candidate-to-python | agent-orchestration | 637df0572 | R392. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-improve | release the iteration's second gate so its agent exits · python-to-candidate | triggers-cron-improve | 637df0572 | R393. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-improve | wait for the iteration at its first gate · candidate-to-python | triggers-cron-improve | 637df0572 | R394. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-manager-turn | release the manager agent · python-to-candidate | agent-orchestration | 637df0572 | R395. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-manager-turn | wait for the raw log cursor at the gate · candidate-to-python | agent-orchestration | 637df0572 | R396. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-manager-turn | release the manager agent · candidate-to-python | agent-orchestration | 637df0572 | R397. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-opencode | hand over the terminated OpenCode turn to server B · python-to-candidate | agent-orchestration | 637df0572 | R398. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-opencode | wait until the OpenCode API fake reaches its turn gate · candidate-to-python | agent-orchestration | 637df0572 | R399. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-opencode | hand over the terminated OpenCode turn to server B · candidate-to-python | agent-orchestration | 637df0572 | R400. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-review-run | release the adopted reviewer · python-to-candidate | agent-orchestration | 637df0572 | R401. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-review-run | delegate an implement task whose reviewer remains alive through handover · candidate-to-python | agent-orchestration | 637df0572 | R402. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-stop-request | wait for the stop target at its first gate · candidate-to-python | agent-orchestration | 637df0572 | R403. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-stop-request | hand over the durable interrupted outcome · candidate-to-python | agent-orchestration | 637df0572 | R404. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| handover-worker-run | wait for the worker agent at its live gate · candidate-to-python | agent-orchestration | 637df0572 | R405. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| improve-loop-reports | wait for the parent manager at its gate | triggers-cron-improve | 637df0572 | R406. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| improve-loop-reports | wait for both iteration reports to reach the busy parent | triggers-cron-improve | 637df0572 | R407. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| improve-loop-reports | release the parent manager after all three reports arrive | triggers-cron-improve | 637df0572 | R408. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| skeleton | page with a session, bearer key | server-skeleton | 637df0572 | R409. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| skeleton | page with a session, no key | server-skeleton | 637df0572 | R410. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| skeleton | page with a worker session, cookie | server-skeleton | 637df0572 | R411. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| skeleton | page with a missing session | server-skeleton | 637df0572 | R412. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| skeleton | page with an empty session | server-skeleton | 637df0572 | R413. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | detail fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R414. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | bootstrap fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R415. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | usage fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R416. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | active listing fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R417. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | starred listing fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R418. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | chat-thread listing fills defaults for absent legacy metadata slots | session-read-api | 637df0572 | R419. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | manager ignores a retired newest-run backend after restart | session-read-api | 637df0572 | R420. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | manager usage ignores a retired newest-run backend after restart | session-read-api | 637df0572 | R421. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | worker displays its retired newest-run backend after restart | session-read-api | 637df0572 | R422. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-metadata-defaults-backend-display | worker usage displays its retired newest-run backend after restart | session-read-api | 637df0572 | R423. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, manager root, starred, grouped | session-read-api | 637df0572 | R424. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, manager root, starred, grouped | session-read-api | 637df0572 | R425. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, manager root, starred, grouped | session-read-api | 637df0572 | R426. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, manager child with child reports | session-read-api | 637df0572 | R427. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, manager child with child reports | session-read-api | 637df0572 | R428. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, manager child with child reports | session-read-api | 637df0572 | R429. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, worker, retried, closed | session-read-api | 637df0572 | R430. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, worker, retried, closed | session-read-api | 637df0572 | R431. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, worker, retried, closed | session-read-api | 637df0572 | R432. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, worker, interrupted, archived | session-read-api | 637df0572 | R433. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, worker, interrupted, archived | session-read-api | 637df0572 | R434. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, worker, interrupted, archived | session-read-api | 637df0572 | R435. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, worker, queued run stopped | session-read-api | 637df0572 | R436. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, worker, queued run stopped | session-read-api | 637df0572 | R437. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, worker, queued run stopped | session-read-api | 637df0572 | R438. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, archived root, starred | session-read-api | 637df0572 | R439. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, archived root, starred | session-read-api | 637df0572 | R440. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, archived root, starred | session-read-api | 637df0572 | R441. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, archived child | session-read-api | 637df0572 | R442. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, archived child | session-read-api | 637df0572 | R443. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, archived child | session-read-api | 637df0572 | R444. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, retired backend, pending input, unread | session-read-api | 637df0572 | R445. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, retired backend, pending input, unread | session-read-api | 637df0572 | R446. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, retired backend, pending input, unread | session-read-api | 637df0572 | R447. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, chat-thread session | session-read-api | 637df0572 | R448. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, chat-thread session | session-read-api | 637df0572 | R449. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, chat-thread session | session-read-api | 637df0572 | R450. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, recycled history, restored | session-read-api | 637df0572 | R451. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, recycled history, restored | session-read-api | 637df0572 | R452. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, recycled history, restored | session-read-api | 637df0572 | R453. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, plans and triggers, unread | session-read-api | 637df0572 | R454. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, plans and triggers, unread | session-read-api | 637df0572 | R455. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, plans and triggers, unread | session-read-api | 637df0572 | R456. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | detail, fork of atlas | session-read-api | 637df0572 | R457. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | bootstrap, fork of atlas | session-read-api | 637df0572 | R458. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-detail | usage, fork of atlas | session-read-api | 637df0572 | R459. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | active list | session-read-api | 637df0572 | R460. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | active list, gzip accepted | session-read-api | 637df0572 | R461. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | archived, every group | session-read-api | 637df0572 | R462. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | archived, group Writing | session-read-api | 637df0572 | R463. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | archived, first page of one | session-read-api | 637df0572 | R464. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | archived, second page of one | session-read-api | 637df0572 | R465. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | starred | session-read-api | 637df0572 | R466. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | search, a name | session-read-api | 637df0572 | R467. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | search, chat content | session-read-api | 637df0572 | R468. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | search, blank query | session-read-api | 637df0572 | R469. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | search, no query | session-read-api | 637df0572 | R470. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-listings | chat threads | session-read-api | 637df0572 | R471. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-stream | cursor at the total sends only catchup_complete | session-stream | b3dc9a015 | R472. Rust ignores the `cursor` query and replays events that `main` skips at this cursor (`server.py:361-375`); Rust reads the cursor from the first frame (`websocket.rs:113`). |
| session-stream | cursor past the total sends only catchup_complete | session-stream | b3dc9a015 | R473. Rust ignores the `cursor` query and replays events that `main` skips past the total (`server.py:361-375`); Rust reads the cursor from the first frame (`websocket.rs:113`). |
| session-stream | a missing cursor closes the socket with 4400 | session-stream | b3dc9a015 | R474. Rust accepts a handshake with no `cursor` query and sends `catchup_complete`; `main` closes with 4400 (`server.py:373-375`). Rust reads the replay cursor from the first frame (`rust/crates/charliebot-session-api/src/websocket.rs:113`). |
| session-stream | a cursor that is not decimal closes the socket with 4400 | session-stream | b3dc9a015 | R475. Rust accepts a non-decimal `cursor` query and sends `catchup_complete`; `main` closes with 4400 (`server.py:373-375`). Rust reads the replay cursor from the first frame (`rust/crates/charliebot-session-api/src/websocket.rs:113`). |
| session-stream | the trigger turns reach the live socket on the session channel | session-stream | 637df0572 | R476. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-stream | the trigger turns reach the live socket on the sidebar channel | session-stream | 637df0572 | R477. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-stream | wait for the last live trigger to fire before shutdown | session-stream | 637df0572 | R478. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-usage-anthropic-compatible | usage, manager on Anthropic-compatible option | session-read-api | 637df0572 | R479. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-usage-anthropic-compatible | usage, worker on Anthropic-compatible option | session-read-api | 637df0572 | R480. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-write-agent | wait at the agent caller gate | session-write | 637df0572 | R481. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-agent | create under the run's own manager task | session-write | 637df0572 | R482. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-write-agent | release the agent caller turn | session-write | 637df0572 | R483. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-cron-listing | read a bound row with cron listing fields | session-write | 637df0572 | R484. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-write-cron-listing | read the session listing with cron fields | session-write | 637df0572 | R485. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-write-fork | fork of the source at a null index, the whole log | session-write | 637df0572 | R486. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-fork | fork of the source at its first line | session-write | 637df0572 | R487. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-fork | fork of the source at a middle line | session-write | 637df0572 | R488. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-fork | fork of the source at its last line | session-write | 637df0572 | R489. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-fork | elone of the source at a middle line takes over and archives the source | session-write | 637df0572 | R490. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | mark unread Cobalt read | session-write | 637df0572 | R491. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | star a session | session-write | 637df0572 | R492. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | unstar a session | session-write | 637df0572 | R493. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | set a session group | session-write | 637df0572 | R494. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | clear a session group with the Python default | session-write | 637df0572 | R495. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | rename a group across both sessions | session-write | 637df0572 | R496. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | delete a group across both sessions | session-write | 637df0572 | R497. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | rate a round thumbs up | session-write | 637df0572 | R498. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-meta | clear a round rating | session-write | 637df0572 | R499. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | legacy create opens a manager root | session-write | 637df0572 | R500. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | v2 create under the manager root | session-write | 637df0572 | R501. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | switch to the held backend | session-write | 637df0572 | R502. Rust's response carries `resource_class: "agent"`; `main` no longer has the key. |
| session-write-task | switch to a second option of the same continuation domain | session-write | 637df0572 | R503. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | rename a session | session-write | 637df0572 | R504. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | set a worker's prompt rule | session-write | 637df0572 | R505. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| session-write-task | clear that prompt rule | session-write | 637df0572 | R506. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| wait-trigger-delay | wait until the trigger file is fired | triggers-cron-improve | 637df0572 | R507. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |
| wait-trigger-local-pid | end the synthetic watched process | triggers-cron-improve | 637df0572 | R508. Rust writes `resource_class: "agent"` into the session's `metadata.json`; `main` no longer has the key. |

## Catch-up 4521a51b2 ignored Cargo tests

Five tests in `rust/crates/charliebot-store/tests/cross_implementation.rs` fail for the same reason as the `resource_class` rows. Each compares Rust's file bytes with Python's and differs only in that key. Each is ignored with its row ID, and `cargo-slot.sh test -- --ignored` fails each of them.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |
| R509 | python_and_rust_store_operations_write_identical_files_and_read_each_others_output | `charliebot-store` `cross_implementation` | 637df0572 | Rust writes `resource_class` into the session's `metadata.json`; `main` drops the key. |
| R510 | python_and_rust_task_trees_share_nodes_gates_and_mutation_bytes | `charliebot-store` `cross_implementation` | 637df0572 | Rust writes `resource_class` into the session's `metadata.json`; `main` drops the key. |
| R511 | python_and_rust_name_an_unnamed_node_from_a_corrupt_counter_the_same_way | `charliebot-store` `cross_implementation` | 637df0572 | Rust writes `resource_class` into the session's `metadata.json`; `main` drops the key. |
| R512 | python_and_rust_fork_and_elone_write_the_same_session_files_at_every_index | `charliebot-store` `cross_implementation` | 637df0572 | Rust writes `resource_class` into the session's `metadata.json`; `main` drops the key. |
| R513 | python_and_rust_delegate_a_task_spec_into_the_same_node_run_and_spec_bytes | `charliebot-store` `cross_implementation` | 637df0572 | Rust writes `resource_class` into the session's `metadata.json`; `main` drops the key. |

## Catch-up 4521a51b2 unported commits

Each of the 43 commits in `git rev-list --no-merges 085f024a5..4521a51b2 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above and in the ignored table.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| 4521a51b2 | give the thread read paging one definition (#3366) | no behavior | discord_client.read_all_messages pages from "0" with limit 100 and continues after the last id until a short page, the same requests the two callers spelled inline; the DiscordAPIError to ThreadReplyError 502 mapping wraps the same call. |
| 43cde9d8f | give the session page size one definition (#3365) | no behavior | constants.SESSION_PAGE_LIMIT_MIN/MAX/DEFAULT (1, 500, 100) equal the old literals in the query bounds, the archived default and the clamp. The only text change is the Python CLI's "charliebot session tree --help" line, now "Page size, 1..500 (default 100)"; the routes it calls answer as before. |
| c9cf037a3 | read the last turns after a context reset and tighten delegate --help | unported | CONTEXT_RESET_INSTRUCTION changes (src/runtime/session_anchors.py:23-35): agents read the reset note, which now tells them to run `charliebot session dialog --turns 2`. Rust keeps the old sentence in rust/crates/charliebot-tasks/src/session_anchors.rs:6-12 (used by execution.rs:541). No parity step reads the reset note. The web_reply.md deletion carries over: Rust reads prompts/web_reply.md at run time (rust/crates/features/web_reply/src/turn_contribution.rs:193). The `session dialog --turns` and `delegate --help` changes are Python CLI only (src/runtime/cli/session.py, delegate.py) and call no changed route. |
| 6627d5c17 | give the thread read page size one definition (#3364) | no behavior | constants.DISCORD_READ_LIMIT_MIN/MAX/DEFAULT (1, 100, 50) equal the old module constants and the request model's bounds; the --limit help text is unchanged. |
| 40f9c8357 | give the chat-event wire names one home at their readers (#3363) | no behavior | Chat-event wire names moved to event_types constants whose values equal the old literals: USER "user", INPUT_EVENT_ID "input_event_id", INPUT_EVENT_IDS "input_event_ids", TASK_CLOSED "task_closed", TASK_CLOSE_REQUESTED "task_close_requested" (src/infra/event_types.py:11,45-46,181-182). |
| 1b8e2c9e9 | give the duplicate-registration refusal one definition (#3362) | no behavior | registration.already_registered (src/infra/registration.py:11) renders "{kind} {name!r} is already registered[ by {owner}]", the sentence each registry spelled. One sentence changes: the (module, attr) refusal in wiring._register_pair (src/runtime/hooks/wiring.py:88) gains repr quotes around "module:attr". It runs only at startup registration, so a double registration fails the import and no HTTP client reads it. |
| 57f5abdb7 | give the context-reset note opener one definition (#3361) | no behavior | CONTEXT_RESET_OPENER (src/runtime/session_anchors.py:37) is "[Context reset: ", the literal both reset notes spelled (session_anchors.py:63, task_execution.py:154). |
| 9b872bf51 | give the chain's step-report format one definition (#3360) | no behavior | step_result_block and completed_prefix (src/features/cron/cron_sequence.py) render the same "**{name} result ({backend}):**" blocks and "Scheduled task '{name}' completed all {n} step(s)" opener at both report sites; the "(no result)" fallback is kept. |
| 5170e67a4 | give the unknown launch kind refusal one definition (#3359) | no behavior | backend_lifecycle.unknown_launch_kind renders "unknown launch kind {kind!r}" (src/runtime/hooks/backend_lifecycle.py), the sentence each of the three sites spelled, all raised as ValueError. |
| c6d8a9a1b | resolve a session's data segments from their owners (#3358) | no behavior | chat_events.chat_event_archives_dir returns data/archives (src/runtime/chat_events.py:42-44) and the glob patterns are unchanged; the data-segment names resolve to the same paths. |
| 0498dad3b | give the finished-run refusal one definition (#3357) | no behavior | runs.run_already_finished_text renders "run {id} already finished; it claims no new inputs", the sentence both dispatch sites spelled (src/runtime/runs.py, session_dispatch.py). |
| 49c49b9a7 | name the session-layout path segments through their owners (#3356) | no behavior | The named constants equal the spelled segments: SESSION_METADATA_NAME "metadata.json" (run_identity.py:29), DATA_DIR_NAME "data" (runs.py:59), RUNS_DIR_NAME "runs", RUN_METADATA_NAME "metadata.json" (runs.py:374-375); the session-sidebar signature keys are the same strings. |
| baad23744 | give the module:attr refusal one definition (#3355) | no behavior | deferred.not_a_spec renders "{subject}: {path!r} is not a 'module:attr' string"; the turn-contribution subject spells the same prefix (src/infra/deferred.py, src/runtime/hooks/turn_contributions.py:107). |
| 882c34c80 | give the ancestor-chain overrun refusal one definition (#3354) | no behavior | task_errors.ancestor_chain_too_long renders "ancestor chain of {id} exceeds {limit} hops" with the same limit at both sites (src/runtime/task_errors.py, task_completion.py, task_sessions.py). |
| 637df0572 | size one agents' slice from host memory and drop the repair share | red-list | Causes R341–R471 and R476–R508 and the ignored tests R509–R513. Rust still writes `resource_class` into session metadata (`rust/crates/charliebot-store/src/metadata.rs:429,594,763,815`), still answers the removed PUT route (`rust/crates/charliebot-session-api/src/writes.rs:46,51-82`), and keeps the AGENTS and REPAIR slice pair with the old OOM sentence (`rust/crates/charliebot-agents/src/transport/cgroup.rs`, `transport/spawn.rs`). Rust owners: `charliebot-store` (`resource_class.rs`, `metadata.rs`) and `charliebot-agents` (`runner.rs`, `transport/spawn.rs`). |
| 53f2c6728 | list rows in server order so a mid-turn input stays above later replies | in Rust | web/static/js/chat/for-you.js and turn-engine.js are served unchanged by the /static mount (server.py:569; static_mounts.rs:40). The server order they read is the store's order, which the server already returns. |
| 0b92bfe43 | give the entry index line one definition (#3353) | no behavior | MemoryEntry.index_line renders "{id} · {title}", and Entry.id is f"{topic}/{slug}" (src/features/memory/memory.py:112), so the index and the CLI print the same line. |
| 7d1f3b2cb | give the router-failure line one definition (#3352) | no behavior | backend_pool.router_failure_line (src/runtime/backend_pool.py) renders "backend router failed: {error}", the sentence the 502 detail (src/runtime/api/internal.py) and the thread error (src/features/chat_threads/thread_entry.py) spelled inline. |
| 69264d495 | give the task-relation cycle refusal one definition (#3351) | no behavior | task_errors.task_relation_cycle renders "task relation cycle through {id}", the same sentence at its three sites (src/runtime/task_sessions.py). |
| 6048b0eec | give the not-open close refusal one definition (#3350) | no behavior | _require_open_for_close (src/runtime/task_completion.py) renders "task {id} is no longer open" through self._tree, the tree each close site aliases (task_completion.py:835, :1033). |
| c1afcfea6 | give the task-miss refusal one definition (#3349) | no behavior | task_errors.task_not_found (src/runtime/task_errors.py) renders "task {id} not found", the sentence each of the three sites spelled. |
| 4ef8d4d6b | give the not-found refusal one definition per wording (#3348) | no behavior | _require_task and _require_session produce the same "task {id} not found" and "session {id} not found" sentences the sites spelled inline (src/runtime/task_sessions.py:502-512); each caller keeps its wording. |
| a8781f62e | reject cursors whose digits int() cannot parse | unported | Adds a 19-digit cap to the cursor in the same check (`server.py:373`), so an oversized cursor closes with 4400 instead of raising out of the handler. Rust's `parse_cursor` (`rust/crates/charliebot-session-api/src/websocket.rs:137`) has no cap. No parity step sends a cursor of 20 or more digits. Rust owner: `websocket.rs` `parse_cursor`. |
| b3dc9a015 | prevent duplicate message draws after reconnect | red-list | Moves the replay cursor from the first frame to the `cursor` query; a missing or non-decimal value closes with 4400 (`server.py:361-375`). Rust still reads the first frame (`rust/crates/charliebot-session-api/src/websocket.rs:113`). Causes R472–R475. |
| 536ebd47b | the sidebar row moves only when the server's list repaints | in Rust | web/static/js/chat/input.js is served unchanged by the /static mount (server.py:569; rust/crates/charliebot-server/src/routes/static_mounts.rs:40). The sidebar row moves on the server's list refetch, which 66a2120cd changes. |
| 66a2120cd | a message moves only its own row, before the refetch | unported | User-message lift (`src/runtime/session_dispatch.py:249-272`): the lift moves only the node it was sent to and runs before the append. Rust still lifts the unarchived ancestor chain after its append: `lift_user_message_branch` (`rust/crates/charliebot-tasks/src/dispatch.rs:216-240`, call `:181`). No scenario sends a user message into an archived ancestor chain, so no step reads the difference. Rust owner to change: `dispatch.rs`. |
| 05093d83b | give the line-scoped format refusal one definition (#3347) | no behavior | _line_error keeps the same "{path}: line {n}: {detail}" sentence for each refusal in memory.py (lines 83-86, 154-206); each detail string is unchanged. |
| 6483d0796 | show cron roots in place outside the 5-row preview limit | in Rust | Cron sidebar: src/features/cron/static/cron.js is served by the /static/features/cron mount (server.py:564; rust/crates/charliebot-server/src/routes/static_mounts.rs:34) and web/static/js/sidebar/groups.js by the /static mount (server.py:569; static_mounts.rs:40). Both files reach Rust unchanged. |
| 35fc4919a | give the not-open cancel refusal one definition (#3346) | no behavior | _require_open_for_cancel reads self._tree, which is the tree both cancel paths alias (task_completion.py:1462, :1516); the sentence is unchanged. |
| 7f17fdcf9 | give the skipped-row state one definition (#3345) | no behavior | SweepRow.skip (src/features/disk_sweep/sweep.py:65-71) sets the same action, reason and freed_bytes; the unsafe-root branch sets freed_bytes before any bytes are assigned (sweep.py:771-775, default 0 at :49). |
| 0de880426 | give the unknown transcription backend refusal one owner (#3344) | no behavior | select_backend's unknown-id refusal now comes from registry.build_transcription_backend; backend_ids() is tuple(_FACTORIES), so the "known:" list and sentence are unchanged, and the voice route still answers 400 (src/features/voice/api.py:114). |
| 9ed12336f | give the fork and elone refusal translation one definition (#3343) | no behavior | _fork_refusal_as_http maps FileNotFoundError to 404, TaskConflictError to _task_http_error and ValueError to bad_request, the same three branches each route spelled inline (src/runtime/api/sessions.py:1219-1232). |
| 3d3b6e89a | give the reminder asks one definition each (#3342) | no behavior | STATUS_ASK_TEXT, REPLY_ASK_TEXT and the reminder texts concatenate to the same sentences; REPLY_BUDGET_CHARS is 500 (src/features/web_reply/audit.py:28-42, reply.py:52-63). |
| e67bf9afd | give the ancestor-walk bound one definition (#3341) | no behavior | takeoff_gate uses task_errors.ANCESTOR_HOP_LIMIT, which is 1000 (src/runtime/task_errors.py:13), the value the local constant held. |
| 34eb8a1f9 | give the ledger's repeated aggregate SQL one definition each (#3340) | no behavior | The ledger's stored trigger SQL, the agg_backfilled key and the INSERT text are the same strings, now named once (src/features/usage/usage_ledger.py:264, :357, :527). |
| 354643b26 | give the missing-binary refusal one owner (#3339) | no behavior | OpenCodeServer._start and the one-shot run call server.require_binary (src/backends/opencode/server.py:62-70); the RuntimeError sentence and the check's place are unchanged. |
| 614316af7 | give the repair-share toggle an English reserved memory label | in Rust | Web-only: the repair-share label lives in web/static/js/sidebar/resource-class.js and web/templates/index.html. Both are superseded at HEAD: 637df0572 removes the toggle and resource-class.js. Rust reads web/templates (rust/crates/charliebot-registry/src/templating.rs:38) and serves /static from web/static (rust/crates/charliebot-server/src/routes/static_mounts.rs:40), so both servers serve the HEAD files. |
| ca93f91b6 | give the verb parsers one owner for the help formatter (#3337) | no behavior | Python CLI only: each verb builds through help_formatter.CliArgumentParser (src/infra/help_formatter.py:112), which sets the same CliHelpFormatter; the commit's own evidence says the help output is byte-identical; the Rust server ships no CLI. |
| cafecb6c9 | give the missing-signing-key refusal one owner (#3336) | unported | 401 detail changes for a run token presented with no signing key: "run token presented (CHARLIEBOT_RUN_TOKEN) but no signing key is configured" becomes "invalid run token: run token presented but no signing key is configured" (src/runtime/api/deps.py:156, run_token.py:121-122). Rust: rust/crates/charliebot-registry/src/caller.rs:63 (its test at :242 asserts the old text). No parity step sends a run token with no key. |
| 398c00eeb | give the session chat-log path one owner (#3335) | no behavior | chat_events.chat_events_path (src/runtime/chat_events.py:37-39) returns session_dir/data/chat_events.jsonl, the path each site spelled inline. |
| 34db00a9c | give the module:attr spec one owner (#3334) | no behavior | deferred.split_attr (src/infra/deferred.py:62-70) keeps the partition-and-check rule; the refusal sentences are unchanged (src/app/setup.py:65, metadata_slot_registration.py:43, turn_contributions.py:107). |
| 7d11e82c9 | give the operator timestamp format one owner (#3333) | no behavior | constants.LOCAL_TIMESTAMP_FORMAT (src/infra/constants.py:71-75) holds the same "%Y-%m-%d %H:%M %Z" string at the five sites (host_auth/api.py, usage/api.py). |
| b74495ff3 | state the page cold read's eight questions (#3332) | no behavior | Docstrings and comments only: "seven" becomes "eight" for the cold-read questions (src/runtime/cold_read.py:1-10, src/infra/models.py:135, src/features/artifacts/artifact_check.py:932). The prompt file prompts/cold_read.md is not in this commit. |
