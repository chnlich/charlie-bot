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
| POST | `/api/voice/{session_id}/confirm` | features/voice | ported | raw WAV body: decodes the opening clip locally and persists nothing. The static sherpa-onnx 1.13.4 archive decodes some recordings differently from the Python wheel on aarch64 (see the differential). |
| POST | `/api/voice/{session_id}` | features/voice | ported | multipart upload: local decode, or the selected non-live cloud backend (the production registry has none). Same transcript divergence as the confirm route. |
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
| lifespan | 6. Start task-tree recovery | agent-runtime | ported | `charliebot-tasks` registers `recovery::recovery_step` and reconciles task-tree nodes. The four handover scenarios (`handover-manager-turn`, `handover-admitted-input`, `handover-drain`, `handover-stop-request`) match Python in both directions. The runner spawns the step (`charliebot-registry/src/lifespan.rs:98`). |
| lifespan | 7. Start early services | services | ported | Starts the early services in registration order after the recovery task starts (`charliebot-registry/src/lifespan.rs:91`, `:102-114`; Python `server.py:270-273`). |
<<<<<<< HEAD
| lifespan | 8. Await task-tree recovery | agent-runtime | ported | The runner awaits `recovery::recovery_step` (`charliebot-registry/src/lifespan.rs:108`). On SIGINT and failed bind, `recovery::let_go_step` cancels run launches and then session consumers after `charliebot_shutdown`, matching `asyncio.run` (`src/runtime/master_cc_run.py:648-664`; `asyncio/runners.py:66-75`). SIGTERM skips it (`uvicorn/server.py:322-339`). The four handover scenarios in row 6 cover the await in both directions. Deviation: a deliberate deviation from Python. Python does not stop during startup recovery: the lifespan awaits recovery inside Uvicorn's startup hook, and Uvicorn checks its exit flag only after startup finishes (`server.py:271-288`; uvicorn 0.42.0 `uvicorn/server.py:77-79` and `:94-96`; signal capture at `uvicorn/server.py:322-346`). Rust installs SIGINT and SIGTERM before any startup step and stops startup at safe points (between recovery nodes, trigger scans, the cgroup sweep and the speech provisioning download), releases the home writer fence and logs `charliebot_shutdown` with `boot=true` (`charliebot-server/src/main.rs`, `charliebot-registry/src/lifespan.rs`). After `server_ready`, the shutdown sequence matches Python. Deviation: when the let-go cancels a worker launch before the launch records its child's pid, Rust kills that child, and Python leaves it running. In Python the cancel passes the `except Exception` around `on_spawn` (`src/runtime/agent_process/base.py:930-938`), the child runs with `start_new_session` and no parent-death signal (`src/runtime/agent_process/base.py:994-1012`), and the next boot starts a second agent for the run (`src/runtime/task_execution.py:2780`, `:2816`). Rust holds the child stopped before `exec` until its pid is recorded, and a launch dropped before then sends SIGKILL to that one pid, the one from its own spawn, when the pid's start tick still matches (`charliebot-agents/src/runner.rs:592-628`); the run keeps no pid and no terminal fact, and the next boot launches it once. A launch requested after the let-go begins starts no task (`charliebot-tasks/src/execution.rs:338-344`) and leaves the durable state of a Python launch task that `asyncio.run` cancels before its first step (`src/runtime/task_execution.py:522`, `:731`; `asyncio/runners.py:208-216`): the run registered with its input claimed, no pid and no terminal fact, which the next boot dispatches once. |
>>>>>>> c7a90122d (docs(rust): record the cut launch deviation in PORT_STATUS)
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
| `state/boot_recovery.json` | tasks | ported | Python-compatible marker written atomically after a clean pass; `charliebot-tasks/src/recovery.rs` and `tests/recovery.rs::clean_boot_writes_marker_and_the_next_boot_skips_a_closed_worker`. |
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

## Catch-up 78e0d10e2 open steps

Each row is one failing Rust parity step of the all-stage run on this head. That run, with the debug binary, matches
1,160 of 1,160 steps, so no row is left. The `handover-stop-request` row is gone: its interrupted run's chat-event order
is accepted by that scenario's `either_order_of_adjacent_events` rule (see `rust/parity/README.md`).

| Scenario | Step | Stage | Commits | Difference |
| --- | --- | --- | --- | --- |

## Catch-up 78e0d10e2 ignored Cargo tests

Each test below fails until Rust ports the behavior changed on `main`, or until its fixture matches `main`. A port task
removes the `catch-up 78e0d10e2` ignore when the test passes.

The launch-environment helper's synthetic compaction case uses `run_kind="manager_turn"`: it covers a manager turn,
whose Python production caller `master_cc_relay` passes `ctx.run_kind` to `compaction_env`.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |
## Catch-up 78e0d10e2 unported commits

Each of the 19 `main` commits that change `src` or `server.py` between `e44c9ccef` and `78e0d10e2` (`git rev-list
--no-merges e44c9ccef..78e0d10e2 -- src server.py`) has one class. `red list` commits appear in the two tables above.
`unported` commits have no Rust counterpart and no parity row or Cargo test. `in Rust` commits already match `main`.
`no behavior` commits change no output.

| Main commit | Change | Class | Note |
| --- | --- | --- | --- |
| e6f440710 | Backup keeps at most five archives; the retention tiers shorten to 3 days and Sundays to 15 days | in Rust | `src/features/backup/backup.py:42-43` and `:122-150`: tiers of 3 and 15 days, no monthly tier. Rust `rust/crates/features/backup/src/backup.rs:26-27` (`DAILY_THRESHOLD`, `WEEKLY_THRESHOLD`), applied by `apply_retention_in_with_now` (line 339). Covered by `retention_keeps_the_last_three_days_and_recent_sundays` in `rust/crates/features/backup/src/lib_tests.rs`, the counterpart of `tests/test_backup.py`. |
| 85054aeab | A recorded Slack or Discord summon counts as a user request | in Rust | `charliebot-core/src/event_types.rs:96` (`is_platform_summon`; the key list at :92) and the gate fold `charliebot-store/src/task_tree.rs:1490` (`holds_user_request`; refusal at :1502). Tests: `charliebot-store/tests/cross_implementation.rs` (`task_gate_matches_python_cases_and_the_parity_fixture_tree`, its summon cases), `charliebot-tasks/tests/delegate.rs` (`summon_authorizes_a_root_manager_but_a_relayed_agent_message_does_not`), and the platform-name pins `the_slack_platform_name_is_a_thread_platform_key` (`charliebot-slack`) and `the_discord_platform_name_is_a_thread_platform_key` (`charliebot-discord`). |
| e4f43f962 | Dated provenance dropped from two cost comments | no behavior | Comments only. |
| 0a844be82 | Web replies and status updates refuse dead links before recording | in Rust | `src/runtime/file_urls.py:63` (`FileLink.dead`), `:237-260` (`scan_file_links`, `dead_links_detail`), refused at `src/features/web_reply/reply.py:183-185` and `src/features/web_reply/status.py:88`. Rust `rust/crates/charliebot-compat/src/file_urls.rs` ports the scanner and `dead_links_detail`; `rust/crates/features/web_reply/src/reply.rs` (`refuse_dead_links`) and `status.rs` (`set_status`, `unread_waiting`) refuse before recording; `rust/crates/features/chat_threads/src/thread_entry.rs` (`assert_no_file_server_links`) refuses file-server and dead links. Covered by `the_file_link_scanner_matches_python_on_the_case_matrix` in `rust/crates/charliebot-compat/tests/file_links_cross_implementation.rs`, the `file_urls` unit tests, `a_dead_file_link_refuses_a_web_reply_with_a_correct_link_and_records_nothing` in `reply.rs`, `dead_links_in_every_status_field_refuse_before_any_cold_read_or_write` in `status.rs`, and `no_file_server_links_refuses_retired_and_local_paths_with_publish_instruction` in `rust/crates/features/chat_threads/src/thread_entry/tests.rs`; the counterparts are `tests/test_web_reply.py` and `tests/core/test_thread_entry.py`. Not ported: the NFKC check of the network location (`_checknetloc`). |
| 23924d7dd | Ruff fixes for the file-link change | no behavior | Lint only. |
| eaa087a23 | Anthropic-compatible Claude Code entries (`cc-anthropic-compatible`) | in Rust | `rust/crates/charliebot-agents/src/backends/anthropic_compatible/` registers options, factory, lifecycle, limits and attribution; `rust/crates/charliebot-registry/src/backend.rs`, `rust/crates/charliebot-usage/src/usage.rs` and `rust/crates/charliebot-session-api/src/detail.rs` pass type-specific limits. Covered by `cli_backends_match_the_python_package_on_synthetic_inputs`, `anthropic_compatible_registration_matches_python_traits_and_usage_attribution`, `anthropic_compatible_usage_uses_the_option_context_window`, `anthropic_compatible_option_requires_a_resolved_model_and_keeps_its_endpoint`, `anthropic_compatible_round_reports_a_reply_from_outside_the_pinned_family`, `test_non_claude_task_build_receives_no_claude_account`, and `session-usage-anthropic-compatible`. |
| eb606bcf1 | Claude Code window follows the entry window; `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is set too | in Rust | `rust/crates/charliebot-agents/src/backends/claude_code/claude_code.rs` sets both window variables and clears incompatible overrides and credentials. Covered by `anthropic_endpoint_sets_the_endpoint_and_pins_every_model_variable_without_a_flag`, `cli_backends_match_the_python_package_on_synthetic_inputs`, and `session-usage-anthropic-compatible`. |
| f6ae0c844 | Run facts folded once; task state read from the history tail | no behavior | Changes no output: one fold of Run facts per event list, and a history-tail read of the task state with a full-fold fallback, give the same outcomes and lifecycle states. The handover-stop-request order race predates it (`master_cc_queue.py:298-309`, `runs.py:1114-1124`) and reproduces Python against Python at `e44c9ccef`. |
| 85e41c2c3 | Boot recovery records a pass marker and skips closed nodes | in Rust | `charliebot-tasks/src/recovery.rs::reconcile_task_tree`; `charliebot-tasks/tests/recovery.rs::clean_boot_writes_marker_and_the_next_boot_skips_a_closed_worker`, `::failed_boot_keeps_the_previous_marker_and_out_of_space_still_retries`, `::breaking_each_skip_condition_alone_keeps_the_worker_in_full_recovery`, and the marker format tests. |
| 23de57ca8 | One recovery-done line; the worktree check uses `os.path.lexists` | in Rust | `charliebot-tasks/src/recovery.rs::boot_skip_reason` and `::recovery_step`; `charliebot-tasks/tests/recovery.rs::dangling_worktree_symlink_keeps_a_closed_worker_in_full_recovery` and `::recovery_done_logs_one_line_with_marker_and_skip_counters`. |
| 8b85da74d | Hardcoded paths replaced by `$CHARLIEBOT_HOME`; the `## Profile Home` section added to prompts | in Rust | `rust/crates/charliebot-tasks/src/prompts/sources.rs` adds `profile_home` to `COMMON_SECTIONS` and `WORKER_BASE_SECTIONS`; `snapshots_match_the_python_server_byte_for_byte` (`prompt_snapshot_golden`) covers it. |
| 2d24e472a | Modules named, not symbols, in the task recovery imports | no behavior | Import style only. |
| a47ad2003 | Type-less lines skipped in the history-tail task state read | in Rust | `charliebot-store/src/task_tree.rs:1362` (`fold_task_facts`). |
| 50bc61cd7 | YAPF formatting of `src/runtime/task_prompts.py` | no behavior | Formatting only. |
| 97d77338e | Unused `KEEP_REASONS` tuple dropped | no behavior | Nothing read the tuple. |
| e3b2c7425 | Shared `_no_summary_fallback` for the improve summary line | no behavior | Both call sites keep the same text. |
| 3be34accc | Startup cgroup sweep limited to this home's sessions | in Rust | `charliebot-agents/src/transport/cgroup.rs:200` (`sweep_stale_session_cgroups`). |
| 6cb6bd9e8 | `register_run` call sites migrated to `register_run_locked` (slice 1) | no behavior | Call-site migration only. |
| 8a7e08a3e | Test-only `register_run` wrapper dropped | no behavior | Python test helpers `python_store_ops.py` and `python_dispatch_golden.py` now call `register_run_locked`. |
