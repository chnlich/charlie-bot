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

The order of the web reply audit's events is undefined. In the requested 20 Python-vs-Python runs, the nudge (`agent_message` from `Web reply`) came before its `run_finished` in all 40 logs. Across 53 kept Python logs from the follow-up runs, it came after once; the no-reply notice (`web_reply_notice`) came before its `run_finished` in all 53. One kept Rust run using the original write order put both events after their respective `run_finished` events. A parity step that compares these events can use the `either_order_of_adjacent_events` rule, which accepts both orders (see `rust/parity/README.md`).

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
| PUT | `/api/sessions/{session_id}/resource-class` | sessions | ported | Operator credentials only. Sets the `resource_class` metadata field that places the session's cgroup (df7920bd7). Rust: `rust/crates/charliebot-session-api/src/writes.rs` `set_resource_class`. |
| GET | `/api/sessions/{session_id}` | sessions | ported |  |
| DELETE | `/api/sessions/{session_id}` | sessions | ported |  |
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
| POST | `/api/sessions/{session_id}/complete` | sessions | ported |  |
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
| POST | `/api/voice/{session_id}` | features/voice | todo | Rust still accepts the local backend; Python at this baseline uploads only through live online backends (`rust/crates/features/voice`). |
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
| config section | `voice` | features/voice | ported | five fields, legacy keys and unknown-key refusal |
| config check | `check_default_backend` | features/voice | ported | validates against local, gemini and muse |
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
| lifespan | 8. Await task-tree recovery | agent-runtime | ported | The runner awaits `recovery::recovery_step` (`charliebot-registry/src/lifespan.rs:108`). On SIGINT and failed bind, `recovery::let_go_step` cancels run launches and then session consumers after `charliebot_shutdown`, matching `asyncio.run` (`src/runtime/master_cc_run.py:648-664`; `asyncio/runners.py:66-75`). SIGTERM skips it (`uvicorn/server.py:322-339`). The four handover scenarios in row 6 cover the await in both directions. Deviation: a deliberate deviation from Python. Python does not stop during startup recovery: the lifespan awaits recovery inside Uvicorn's startup hook, and Uvicorn checks its exit flag only after startup finishes (`server.py:271-288`; uvicorn 0.42.0 `uvicorn/server.py:77-79` and `:94-96`; signal capture at `uvicorn/server.py:322-346`). Rust installs SIGINT and SIGTERM before any startup step and stops startup at safe points (between recovery nodes, trigger scans, the cgroup sweep and the speech provisioning download), releases the home writer fence and logs `charliebot_shutdown` with `boot=true` (`charliebot-server/src/main.rs`, `charliebot-registry/src/lifespan.rs`). After `server_ready`, the shutdown sequence matches Python. Deviation: when the let-go cancels a worker launch before the launch records its child's pid, Rust kills that child, and Python leaves it running. In Python the cancel passes the `except Exception` around `on_spawn` (`src/runtime/agent_process/base.py:930-938`), the child runs with `start_new_session` and no parent-death signal (`src/runtime/agent_process/base.py:994-1012`), and the next boot starts a second agent for the run (`src/runtime/task_execution.py:2780`, `:2816`). Rust holds the child stopped before `exec` until its pid is recorded, and a launch dropped before then sends SIGKILL to that one pid, the one from its own spawn, when the pid's start tick still matches (`charliebot-agents/src/runner.rs:592-628`); the run keeps no pid and no terminal fact, and the next boot launches it once. A launch requested after the let-go begins starts no task (`charliebot-tasks/src/execution.rs:338-344`) and leaves the durable state of a Python launch task that `asyncio.run` cancels before its first step (`src/runtime/task_execution.py:522`, `:731`; `asyncio/runners.py:208-216`): the run registered with its input claimed, no pid and no terminal fact, which the next boot dispatches once. |
| lifespan | 9. Recover pending triggers | agent-runtime | ported | |
| lifespan | 10. Start ready services | services | ported | Starts the ready services in registration order after the trigger step (`charliebot-registry/src/lifespan.rs:99`, `:102-114`; Python `server.py:284-285`). |
| lifespan | 11. Log ready and serve | foundation | ported | |
| lifespan | 12. Stop services and close shared clients | services | ported | `charliebot_shutdown` has `shutdown_ms`, `discord_ms`, `ext_usage_ms`, `host_auth_ms`, `merge_pool_ms`, `scheduler_ms`, `slack_ms`, `speech_ms`, `usage_tally_ms`, `http_client_ms` and `ws_close_ms` (`server.py:293-310`). Rust writes `usage_tally_ms = 0` because it has no `usage_tally` service. Services stop in reverse registration order, then the HTTP client and WebSockets close; Rust releases the fence and logs the line. Let-go steps run after the line on SIGINT and failed bind (`uvicorn/server.py:179-182`); SIGTERM raises its default action and skips them (`uvicorn/server.py:322-339`; `asyncio/runners.py:66-75`). Rust closes WebSockets with code 1000; Uvicorn closes them with code 1012 before lifespan shutdown. The web UI reconnects on any close and does not read the code (`web/static/js/websocket.js`, `socket.onclose`). The usage quota builds its own client, so it is not closed here. |
| lifespan | 13. Release home writer fence | foundation | ported | |
| service | `scheduler` | features/cron | ported | Waits for task-tree recovery, binds each unbound cron task to one manager node, and scans enabled tasks every 60 seconds. A bound-node wake clears a native anchor older than the latest Saturday 01:00 America/Los_Angeles boundary, archives old root chat events, and decides the report prefix from the cleared anchor; recycle errors log `scheduled_session_recycle_failed` and do not stop the wake. Worker-thread directories remain untouched, matching Python. Client-visible cron stamps use Python `datetime.isoformat()` microseconds and the task-zone offset. |
| service | `slack` | features/slack | ported | Starts Socket Mode and the lost-summon backfill when both Slack tokens and an allowed-user id are configured. |
| service | `discord` | features/discord | ported | Starts the Discord gateway and lost-summon backfill when the bot token and an allowed user are configured. |
| service | `host_auth` | features/host_auth | ported | one round at start, then one per 1800 s |
| service | `merge_pool` | features/trace | ported | In-process builds share one cache-key flight and a bounded build gate. |
| service | `usage_tally` | features/usage | not applicable | It only preloads Python modules; Rust has no import cost to hide. |
| service | `ext_usage` | features/usage | ported | one account per 60 s, round-robin; the account set is re-derived every round. Deviation: `ext_usage_poll_error` formats the error with its cause chain (`{error:#}`); Python logs `str(e)` for a fetch error, and the traceback for a round error. |
| service | `headless_renderer` | features/artifacts | ported | Lazy per-process Chrome renderer; the let-go step closes it after the shutdown log on SIGINT and failed bind. SIGTERM skips the step, so the browser stays open as Python's `atexit` hook does not run (`src/features/artifacts/headless_render.py:184`; `uvicorn/server.py:322-339`). Deviation: close sends SIGKILL to Chrome's process group; Python kills only the browser pid (`src/features/artifacts/headless_render.py:159`). |
| service | `speech` | features/voice | ported | Sherpa and `qwen3_hf` provisioning and warm-up run on a blocking worker. `speech.stop` aborts the monitor and stops helpers; the `speech` let-go step joins the worker after the shutdown log on SIGINT and failed bind. Python starts its speech thread at `src/features/voice/service.py:43-54`; `asyncio.run` joins its executor on normal exits (`asyncio/runners.py:66-75`; `concurrent/futures/thread.py:23-31`). SIGTERM skips let-go and ends the worker with the process. `qwen3_hf` runs its model in `rust/helpers/qwen3_hf_helper.py` (see `rust/docs/python_helper.md`): readiness does not wait for the transformers snapshot, which `create_qwen3_hf_bundle` fetches. A snapshot failure therefore falls back to sherpa, where Python parks the provisioning error. |
| registration | scheduled handler `backup` | features/backup | ported | `run_scheduled_backup` creates the archive and applies retention off the async runtime, and returns the archive path as the fire's summary. |
| registration | scheduled handler `usage_ledger` | features/usage | ported | Captures registered log sources and CharlieBot threads, master captures and Runs off the async runtime. |
| registration | scheduled handler `cool_storage` | features/usage | ported | Real run only (no dry run, session scope or vacuum): captures usage into the ledger, deletes cold sessions' raw transport files, then calls each usage source's sweep, off the async runtime. Returns `format_sweep_line` as the summary. The cold rule reads `updated_at` as `models.parse_utc_datetime` does (`utc_datetime.rs`), and a rotation suffix is `str.isdigit` (`charliebot_core::pystr`). Deviations: a metadata document with a NaN or infinite number fails the run, where Python loads it; `updated_at` text with a week date, a stray final character or a fractional offset keeps its session, where Python may read it as cold; `storage_cool_sweep_done` logs `session_id` as Debug text (`None`), not JSON null. |
| registration | backend type `opencode` | backends/opencode | ported | Registers the option model, factory, native-id traits, shared serve manager, usage source attribution and `snapshot` reading limits (`charliebot-agents/src/backends/opencode/mod.rs`). |
| registration | usage source `opencode` | backends/opencode | ported | Registers id prefixes `opencode-` and `oc-`. The log side reads the store database (`usage_logs.rs`); the sweep vacuums it and reports its freelist (`usage_sweep.rs`). |
| registration | diff root `memory` | features/memory | ported | `<home>/memory`. The diff view that reads it is not ported (features/diff_view). |
| registration | turn contribution `memory` | features/memory | ported | Adds the memory segments of every run's managed instructions. No Rust code launches a run yet, so nothing calls it until the run launch path lands. |
| registration | turn contribution `web_reply`: instruction segments and round-end audit | features/web_reply | ported | Adds `prompts/web_reply.md` to a manager run of a web session. The Rust server runs the round-end audit (`after_turn`, `audit.py`) after a clean turn that answers a user: the store starts each contribution's `after_turn` on `master_done` (`RegistryRuntimeEvents::after_turn` in `charliebot-registry/src/store_hooks.rs`), and the audit posts the nudge or the notice. The run launch path that assembles the segments is not ported yet. |
| registration | router `/api/internal` (`web_reply`) | features/web_reply | ported | The four web reply routes above. |
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
| `models/` speech assets | features/voice | ported | Sherpa assets are verified and cached. `qwen3_hf` caches the Silero VAD model; its transformers snapshot is cached by Python's `create_qwen3_hf_bundle` inside the helper, not by this server. |
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

Both all-stage Rust debug runs failed the same 176 of 1169 steps, and no step failed in only one of them, so no row is intermittent. The Python-vs-Python run passed all 1109 of its steps. Four main changes account for every row: `4c3bd3fda` (99 rows: speech models at startup and the online-only voice upload), `9b83e3b1f` (63 rows: mid-turn stdin and argv, and the raw-log cursor), `df7920bd7` (29 rows: `resource_class` in session metadata and bodies) and `93b427659` (2 rows: the removed confirm route). 17 rows name two commits.

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |
| manager-turn-cancellation-routes | server startup | agent-orchestration-cancel | 4c3bd3fda | candidate writes `home/models` at startup (R78) |
| manager-turn-cancellation-routes | wait at the retried manager run cancel gate | agent-orchestration-cancel | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R79) |
| manager-turn-cancellation-routes | server shutdown and fake-agent cleanup | agent-orchestration-cancel | 9b83e3b1f | manager-turn argv or stdin differs (R80) |
| agent-orchestration-cold-read | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R81) |
| agent-orchestration-cold-read | delegate a cold read of the synthetic page | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R82) |
| agent-orchestration-cold-read | let the manager finish the cold-read report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R83) |
| agent-orchestration-cold-read | delegate a cold read whose first reader fails and second answers | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R84) |
| agent-orchestration-cold-read | let the manager finish the retry report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R85) |
| agent-orchestration-cold-read | delegate a cold read whose two readers fail | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R86) |
| agent-orchestration-cold-read | let the manager finish the all-failed report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R87) |
| agent-orchestration-cold-read | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R88) |
| task-completion-routes | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R89) |
| task-completion-routes | cancel the stopped queued worker to deliver its report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R90) |
| task-completion-routes | release the active manager run | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R91) |
| task-completion-routes | cancel the task after its run finishes | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R92) |
| task-completion-routes | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R93) |
| agent-orchestration-delegate | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R94) |
| agent-orchestration-delegate | delegate one repo-less script run | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R95) |
| agent-orchestration-delegate | delegate a failing repo-less worker | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R96) |
| agent-orchestration-delegate | let the manager finish the failed-script report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R97) |
| agent-orchestration-delegate | delegate a failing implement worker on the local repository | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R98) |
| agent-orchestration-delegate | let the manager finish the failed-repository report | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R99) |
| agent-orchestration-delegate | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R100) |
| manager-turn-execution-and-routes | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R101) |
| manager-turn-execution-and-routes | wait for the retried manager run to finish | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R102) |
| manager-turn-execution-and-routes | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R103) |
| manager-turn-opencode | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R104) |
| implement-review-chain | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R105) |
| implement-review-chain | delegate a repo task whose unchanged branch already landed on origin main | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R106) |
| implement-review-chain | delegate a repo-less implement task | agent-orchestration | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R107) |
| implement-review-chain | delegate a task whose reviewer exhausts the configured pool | agent-orchestration | 9b83e3b1f, df7920bd7 | raw-log cursor differs; replay echo line missing from the raw log; `metadata.json` lacks `resource_class` (R108) |
| implement-review-chain | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R109) |
| agent-orchestration-session-message | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R110) |
| agent-orchestration-session-message | a relay to a live session is accepted | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R111) |
| agent-orchestration-session-message | let the target manager answer the relay | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R112) |
| agent-orchestration-session-message | server shutdown and fake-agent cleanup | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R113) |
| chat-request-id | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R114) |
| chat-upload | server startup | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R115) |
| cron-routes-and-ticks | server startup | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R116) |
| cron-routes-and-ticks | wait for the manual cron firing report | triggers-cron-improve | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R117) |
| cron-routes-and-ticks | wait for a scheduler tick to fire the same task again | triggers-cron-improve | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R118) |
| cron-routes-and-ticks | server shutdown and fake-agent cleanup | triggers-cron-improve | 9b83e3b1f | manager-turn argv or stdin differs (R119) |
| features-artifact-view | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R120) |
| features-artifacts-plan-fork | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R121) |
| features-artifacts-plan-fork | fork of the plan worker at a null index | features | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R122) |
| features-artifacts-plan-fork | server shutdown and fake-agent cleanup | features | 9b83e3b1f | manager-turn argv or stdin differs (R123) |
| features-artifacts-plan-present | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R124) |
| features-artifacts-plan-verbs | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R125) |
| features-artifacts-plans | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R126) |
| features-diag | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R127) |
| features-diff-view | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R128) |
| features-discord-check-guild | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R129) |
| features-discord-check-no-token | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R130) |
| features-discord-check-refused | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R131) |
| features-discord-routes | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R132) |
| features-explain | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R133) |
| features-ext-usage-failing | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R134) |
| features-ext-usage | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R135) |
| features-files | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R136) |
| features-harness-discord | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R137) |
| features-harness-discord | allowed user mention starts a Discord thread session | features | df7920bd7 | `metadata.json` lacks `resource_class` (R138) |
| features-harness-discord | release the silent summon result | features | df7920bd7 | `metadata.json` lacks `resource_class` (R139) |
| features-harness-slack | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R140) |
| features-harness-slack | allowed app mention starts its Slack thread session | features | df7920bd7 | `metadata.json` lacks `resource_class` (R141) |
| features-harness-slack | release summon agent after its result | features | df7920bd7 | `metadata.json` lacks `resource_class` (R142) |
| features-harness-terminal | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R143) |
| features-harness-trace-merge | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R144) |
| features-harness-tree | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R145) |
| features-harness-voice-bodies | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R146) |
| features-harness-voice-bodies | the removed confirm route answers as an unknown path | features | 93b427659 | removed confirm route still answers (R147) |
| features-harness-voice | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R148) |
| features-host-auth-empty | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R149) |
| features-host-auth | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R150) |
| features-ncu | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R151) |
| features-slack-ack | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R152) |
| features-slack-backfill | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R153) |
| features-slack-reply-failure | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R154) |
| features-slack-reply-refusals | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R155) |
| features-slack-reply-stale | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R156) |
| features-slack-reply-success | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R157) |
| features-slack-validation | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R158) |
| features-terminal | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R159) |
| features-trace | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R160) |
| features-usage-page-uncaptured | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R161) |
| features-usage-page | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R162) |
| features-voice-handshake | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R163) |
| features-voice-local | the removed confirm route answers as an unknown path | features | 93b427659 | removed confirm route still answers (R164) |
| features-voice-local | upload without a backend field goes to the online default, which has no credential here | features | 4c3bd3fda | voice upload backend choice differs; voice transcript written by the candidate only (R165) |
| features-voice-local | upload with the removed local backend id is refused | features | 4c3bd3fda | voice upload backend choice differs; voice transcript written by the candidate only (R166) |
| features-voice-local | upload with an unknown backend id is refused | features | 4c3bd3fda | voice upload backend choice differs; voice transcript written by the candidate only (R167) |
| features-voice-local | upload with the gemini backend id is refused without its credential | features | 4c3bd3fda | voice upload backend choice differs; voice transcript written by the candidate only (R168) |
| features-voice-provisioning | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R169) |
| features-voice-refusals | empty backend id | features | 4c3bd3fda | voice refusal message differs (R170) |
| features-voice-refusals | local backend does not stream live partials | features | 4c3bd3fda | voice refusal message differs (R171) |
| features-voice-refusals | unknown backend id | features | 4c3bd3fda | voice refusal message differs (R172) |
| features-voice-relay | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R173) |
| features-web-reply-audit | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R174) |
| features-web-reply-audit | wait for the audit nudge | features | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R175) |
| features-web-reply-audit | server shutdown and fake-agent cleanup | features | 9b83e3b1f | manager-turn argv or stdin differs (R176) |
| features-web-reply-language | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R177) |
| features-web-reply-language | wait for the audit nudge | features | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R178) |
| features-web-reply-language | server shutdown and fake-agent cleanup | features | 9b83e3b1f | manager-turn argv or stdin differs (R179) |
| features-web-reply | server startup | features | 4c3bd3fda | candidate writes `home/models` at startup (R180) |
| handover-admitted-input | hand over the live manager run · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R181) |
| handover-admitted-input | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R182) |
| handover-drain | stop both servers and let the agent exit while none runs · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R183) |
| handover-drain | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R184) |
| handover-drain | wait for the raw log cursor at the gate · candidate-to-python | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R185) |
| handover-drain | stop both servers and let the agent exit while none runs · candidate-to-python | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R186) |
| handover-drain | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R187) |
| handover-improve | hand over the loop with its iteration in flight · python-to-candidate | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R188) |
| handover-improve | release the iteration's second gate so its agent exits · python-to-candidate | triggers-cron-improve | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R189) |
| handover-improve | server shutdown and fake-agent cleanup · python-to-candidate | triggers-cron-improve | 9b83e3b1f | manager-turn argv or stdin differs (R190) |
| handover-improve | server startup · candidate-to-python | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R191) |
| handover-improve | wait for the iteration at its first gate · candidate-to-python | triggers-cron-improve | df7920bd7 | `metadata.json` lacks `resource_class` (R192) |
| handover-manager-turn | hand over the live manager run · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R193) |
| handover-manager-turn | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R194) |
| handover-manager-turn | wait for the raw log cursor at the gate · candidate-to-python | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R195) |
| handover-manager-turn | release the manager agent · candidate-to-python | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R196) |
| handover-manager-turn | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R197) |
| handover-opencode | hand over the terminated OpenCode turn to server B · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R198) |
| handover-opencode | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R199) |
| handover-review-run | adopt the live review run · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R200) |
| handover-review-run | release the adopted reviewer · python-to-candidate | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R201) |
| handover-review-run | server shutdown and fake-agent cleanup · python-to-candidate | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R202) |
| handover-review-run | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R203) |
| handover-review-run | delegate an implement task whose reviewer remains alive through handover · candidate-to-python | agent-orchestration | df7920bd7 | `metadata.json` lacks `resource_class` (R204) |
| handover-stop-request | hand over the durable interrupted outcome · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R205) |
| handover-stop-request | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R206) |
| handover-stop-request | wait for the stop target at its first gate · candidate-to-python | agent-orchestration | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R207) |
| handover-stop-request | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | 9b83e3b1f | manager-turn argv or stdin differs (R208) |
| handover-worker-run | hand over the live worker run · python-to-candidate | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R209) |
| handover-worker-run | server startup · candidate-to-python | agent-orchestration | 4c3bd3fda | candidate writes `home/models` at startup (R210) |
| handover-worker-run | wait for the worker agent at its live gate · candidate-to-python | agent-orchestration | df7920bd7 | `metadata.json` lacks `resource_class` (R211) |
| improve-loop-reports | server startup | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R212) |
| improve-loop-reports | wait for both iteration reports to reach the busy parent | triggers-cron-improve | df7920bd7 | `metadata.json` lacks `resource_class` (R213) |
| improve-routes | server startup | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R214) |
| other-backends-openai-proxy | server startup | other-backends | 4c3bd3fda | candidate writes `home/models` at startup (R215) |
| skeleton | server startup | server-skeleton | 4c3bd3fda | candidate writes `home/models` at startup (R216) |
| skeleton | page with a session, bearer key | server-skeleton | 4c3bd3fda | voice page backend list and default differ (R217) |
| skeleton | page with a session, no key | server-skeleton | 4c3bd3fda | voice page backend list and default differ (R218) |
| skeleton | page with a worker session, cookie | server-skeleton | 4c3bd3fda | voice page backend list and default differ (R219) |
| skeleton | page with a missing session | server-skeleton | 4c3bd3fda | voice page backend list and default differ (R220) |
| skeleton | page with an empty session | server-skeleton | 4c3bd3fda | voice page backend list and default differ (R221) |
| session-metadata-defaults-backend-display | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R222) |
| session-detail | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R223) |
| session-event-ranges | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R224) |
| session-listings | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R225) |
| session-runs | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R226) |
| session-stream | server startup | session-stream | 4c3bd3fda | candidate writes `home/models` at startup (R227) |
| session-stream | the trigger turns reach the live socket on the session channel | session-stream | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R228) |
| session-stream | the trigger turns reach the live socket on the sidebar channel | session-stream | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R229) |
| session-stream | wait for the last live trigger to fire before shutdown | session-stream | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R230) |
| session-stream | server shutdown and fake-agent cleanup | session-stream | 9b83e3b1f | manager-turn argv or stdin differs (R231) |
| session-usage-anthropic-compatible | server startup | session-read-api | 4c3bd3fda | candidate writes `home/models` at startup (R232) |
| session-write-agent | server startup | session-write | 4c3bd3fda | candidate writes `home/models` at startup (R233) |
| session-write-agent | create under the run's own manager task | session-write | df7920bd7 | `metadata.json` lacks `resource_class` (R234) |
| session-write-agent | release the agent caller turn | session-write | df7920bd7 | `metadata.json` lacks `resource_class` (R235) |
| session-write-cron-listing | server startup | session-write | 4c3bd3fda | candidate writes `home/models` at startup (R236) |
| session-write-fork | server startup | session-write | 4c3bd3fda | candidate writes `home/models` at startup (R237) |
| session-write-fork | fork of the source at a null index, the whole log | session-write | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R238) |
| session-write-fork | fork of the source at its first line | session-write | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R239) |
| session-write-fork | fork of the source at a middle line | session-write | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R240) |
| session-write-fork | fork of the source at its last line | session-write | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R241) |
| session-write-fork | elone of the source at a middle line takes over and archives the source | session-write | df7920bd7, 9b83e3b1f | `metadata.json` lacks `resource_class`; raw-log cursor differs; replay echo line missing from the raw log (R242) |
| session-write-fork | server shutdown and fake-agent cleanup | session-write | 9b83e3b1f | manager-turn argv or stdin differs (R243) |
| session-write-meta | server startup | session-write | 4c3bd3fda | candidate writes `home/models` at startup (R244) |
| session-write-task | server startup | session-write | 4c3bd3fda | candidate writes `home/models` at startup (R245) |
| session-write-task | legacy create opens a manager root | session-write | df7920bd7 | `metadata.json` lacks `resource_class` (R246) |
| session-write-task | v2 create under the manager root | session-write | df7920bd7 | `metadata.json` lacks `resource_class` (R247) |
| wait-trigger-delay | server startup | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R248) |
| wait-trigger-delay | wait until the trigger file is fired | triggers-cron-improve | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R249) |
| wait-trigger-delay | server shutdown and fake-agent cleanup | triggers-cron-improve | 9b83e3b1f | manager-turn argv or stdin differs (R250) |
| wait-trigger-local-pid | server startup | triggers-cron-improve | 4c3bd3fda | candidate writes `home/models` at startup (R251) |
| wait-trigger-local-pid | end the synthetic watched process | triggers-cron-improve | 9b83e3b1f | raw-log cursor differs; replay echo line missing from the raw log (R252) |
| wait-trigger-local-pid | server shutdown and fake-agent cleanup | triggers-cron-improve | 9b83e3b1f | manager-turn argv or stdin differs (R253) |


## Catch-up d1f7e3ba6 ignored Cargo tests

One Cargo test fails on a main change, and it carries `#[ignore = "catch-up d1f7e3ba6: <row>"]`. R254 and R255
are gone: the Python reference helper clears the account readings itself. R256 to R260 are gone: the Rust
`metadata.json` writer carries `resource_class`. The other three ignored tests were ignored before this catch-up.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |
| R261 | `sherpa_pin_is_the_release_the_python_wheel_locks` | `charliebot-voice` `--lib` (`transcriber::tests`) | 4c3bd3fda | The test reads the sherpa-onnx pin from `uv.lock`, which no longer locks `sherpa-onnx-core`: 23 `sherpa` lines at `3c4591bad`, none now. |

## Catch-up d1f7e3ba6 unported commits

Each of the 30 commits in `git rev-list --no-merges 3c4591bad..d1f7e3ba6 -- src server.py web` has one class.
`red-list` rows are named by their IDs in the Difference cells above.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| d5b194146 | add safe cleanup for old task paths | in Rust | Ports the scheduled `disk_sweep` handler and `paths.scratch_dir`; the Python CLI stays in place. The package mount remains empty like Python's. No parity scenario drives the sweep. Rust implementation: `rust/crates/features/disk_sweep/src/lib.rs:16` and `rust/crates/features/disk_sweep/src/sweep.rs:129`. |
| b3a121bf0 | drop two module constants nothing reads | no behavior | Removes two unread module constants from `src/features/memory/memory_proposal.py` and `src/infra/pytest_sandbox.py`. |
| 7fa53b91b | drop the deleted speech service from the preload docstring | no behavior | Changes only the docstring of `src/features/usage/api.py`; the service behavior is tracked by `4c3bd3fda`, owned by `rust/crates/features/voice`. |
| 8cbc40276 | drop the dead transcription registry setter | no behavior | Removes an unused setter in `src/features/voice/transcription/registry.py`; the local transcription gap is tracked by `4c3bd3fda`, owned by `rust/crates/features/voice`. |
| 224e4e5cf | report a missing session slice as unavailable at startup | unported | Reports an unavailable slice at startup and fails an unbounded spawn (`src/infra/process.py:114-119,142-179,408-416`). Rust target: `rust/crates/charliebot-core/src/cgroup.rs` and `rust/crates/charliebot-agents/src/transport/cgroup.rs`. |
| 3530285d8 | drop the unused session_cgroup_path helper | no behavior | Removes an unused helper from `src/infra/process.py`; the session-share behavior is tracked by `df7920bd7`, owned by `charliebot-agents` and `charliebot-core`. |
| 8fccbda46 | give the session join script the slice's OOM priority | unported | Writes the selected OOM priority from the opencode join script (`src/backends/opencode/cgroup_join.sh:2`). Rust target: `rust/crates/charliebot-agents/src/backends/opencode/server.rs`. No parity step reads the priority. |
| df7920bd7 | isolate server, agent and repair session shares | red-list | Adds `resource_class`, its PUT route and the slice shares (`src/runtime/resource_class.py`, `src/runtime/api/sessions.py:1357`). Rust owners: `charliebot-store` metadata, `charliebot-session-api` routes, `charliebot-agents` `transport/cgroup.rs` and `transport/spawn.rs`, and `charliebot-core` `cgroup.rs`. Rows: R82, R84, R86, R95–R96, R98, R106–R108, R117–R118, R122, R138–R139, R141–R142, R192, R204, R211, R213, R234–R235, R238–R242, R246–R247. |
| 1043df01b | single-home the archive operator refusal | no behavior | Moves the archive operator refusal into `ARCHIVE_OPERATOR_REFUSAL` (`src/runtime/task_sessions.py`). The 403 detail keeps its text. |
| 4c3bd3fda | transcribe uploads through the online backend | red-list | Deletes the local transcription engine and `voice_setup`, and sends uploads through online backends (`src/features/voice/`). Rust owner: `rust/crates/features/voice`, including `provisioning.rs`, `transcriber.rs` and `api.rs`. Rust still provisions `home/models` and accepts `local`. Rows: R78, R81, R89, R94, R101, R104–R105, R110, R114–R116, R120–R121, R124–R137, R140, R143–R146, R148–R163, R165–R174, R177, R180–R184, R188, R191, R193–R194, R198–R200, R203, R205–R206, R209–R210, R212, R214–R227, R232–R233, R236–R237, R244–R245, R248, R251. |
| 93b427659 | drop the browser's confirm probe and local-engine note | red-list | Drops the browser's confirm probe (`src/features/voice/static/voice-input.js`). Rust owner: `rust/crates/features/voice/src/api.rs` still registers the removed confirm route. Rows: R147, R164. |
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
| b03c38579 | scan the echoed raw log off the loop | no behavior | Scans the echoed raw log on a thread (`src/runtime/midturn_delivery.py`). The scan result is unchanged. The mid-turn delivery it serves is `9b83e3b1f`. |
| 9b83e3b1f | deliver messages during manager turns | red-list | Delivers messages during manager turns through stream-json stdin with the replay echo (`src/runtime/midturn_delivery.py`, `src/backends/claude_code/claude_code.py`). Rust has no mid-turn delivery. Rows: R79–R80, R82–R88, R90–R93, R95–R100, R102–R103, R106–R109, R111–R113, R117–R119, R122–R123, R175–R176, R178–R179, R185–R187, R189–R190, R195–R197, R201–R202, R207–R208, R228–R231, R238–R243, R249–R250, R252–R253. |
