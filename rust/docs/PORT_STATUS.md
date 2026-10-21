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
| POST | `/api/internal/web/status/unread` | features/web_reply | todo |  |
| POST | `/api/internal/web/ask-check` | features/web_reply | todo |  |
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
| GET, HEAD | `/static/features/router` | foundation | todo |  |
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
| GET, HEAD | `/static/backends/kimi` | foundation | ported |  |
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
| lifespan | 5. Log and sweep session cgroups | foundation | ported | The `session_cgroups` pre-recovery step logs the cgroup startup line and sweeps empty stale cgroups (`charliebot-agents/src/agent_environment.rs:95`, `transport/cgroup.rs:184`, `:214`; Python `server.py:266-268`, `src/infra/process.py:307`, `:334`). Negative `session_memory_max_mb`, `session_swap_max_mb` and `session_tasks_max` start as in Python, and each spawn applies them as `transport/cgroup.rs:143` and `:52` do (Python `src/infra/process.py:272`, `:110`). Parent-death children spawn on a process-lifetime thread; pdeath-free children use Tokio's blocking pool (`charliebot-agents/src/transport/spawn.rs`). Parent-side turn-tree spawns set nice to 10 after exec; the Claude one-shot raises child nice by 10 before exec, matching Python `os.setpriority` and `os.nice` (`src/runtime/agent_process/base.py:858-903`, `src/infra/process.py:192-202`). |
| lifespan | 6. Start task-tree recovery | agent-runtime | ported | `charliebot-tasks` registers `recovery::recovery_step` and reconciles task-tree nodes. The four handover scenarios (`handover-manager-turn`, `handover-admitted-input`, `handover-drain`, `handover-stop-request`) match Python in both directions. The runner spawns the step (`charliebot-registry/src/lifespan.rs:98`). |
| lifespan | 7. Start early services | services | ported | Starts the early services in registration order after the recovery task starts (`charliebot-registry/src/lifespan.rs:91`, `:102-114`; Python `server.py:270-273`). |
| lifespan | 8. Await task-tree recovery | agent-runtime | ported | The runner awaits `recovery::recovery_step` (`charliebot-registry/src/lifespan.rs:108`). On SIGINT and failed bind, `recovery::let_go_step` cancels session consumers after `charliebot_shutdown`, matching `asyncio.run` (`src/runtime/master_cc_run.py:648-664`; `asyncio/runners.py:66-75`). SIGTERM skips it (`uvicorn/server.py:322-339`). The four handover scenarios in row 6 cover the await in both directions. |
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

## Catch-up e44c9ccef open steps

Each row is one failing Rust parity step from the all-stage run (1,014/1,122 steps matched).

| Scenario | Step | Stage | Main commit | Difference |
| --- | --- | --- | --- | --- |
| manager-turn-cancellation-routes | server shutdown and fake-agent cleanup | agent-orchestration-cancel | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| agent-orchestration-cold-read | delegate a cold read of the synthetic page | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | let the manager finish the cold-read report | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | delegate a cold read whose first reader fails and second answers | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | let the manager finish the retry report | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | delegate a cold read whose two readers fail | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | let the manager finish the all-failed report | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-cold-read | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| task-completion-routes | cancel the stopped queued worker to deliver its report | agent-orchestration | `d401d9a2a` | Python cancel responses report queued runs stopped, including `never_launched`; Rust omits that list or entry. |
| task-completion-routes | release the active manager run | agent-orchestration | `d401d9a2a` | The new task stop/completion events shift the returned or persisted event id by one. |
| task-completion-routes | cancel the task after its run finishes | agent-orchestration | `d401d9a2a` | Python cancel responses report queued runs stopped, including `never_launched`; Rust omits that list or entry. |
| task-completion-routes | acknowledge the pending Cobalt input | agent-orchestration | `d401d9a2a` | The new task stop/completion events shift the returned or persisted event id by one. |
| task-completion-routes | replay the input acknowledgement | agent-orchestration | `d401d9a2a` | The new task stop/completion events shift the returned or persisted event id by one. |
| task-completion-routes | operator restores an archived ancestor chain | agent-orchestration | `d401d9a2a` | The new task stop/completion events shift the returned or persisted event id by one. |
| task-completion-routes | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| agent-orchestration-delegate | delegate one repo-less script run | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| agent-orchestration-delegate | delegate a failing repo-less worker | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| agent-orchestration-delegate | let the manager finish the failed-script report | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-delegate | delegate a failing implement worker on the local repository | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| agent-orchestration-delegate | let the manager finish the failed-repository report | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| agent-orchestration-delegate | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| manager-turn-execution-and-routes | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| manager-turn-opencode | wait for the OpenCode manager run to finish | agent-orchestration | `d401d9a2a` | Python persists the completed OpenCode manager run directory; Rust leaves no matching run directory. |
| implement-review-chain | delegate a repo task whose unchanged branch already landed on origin main | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| implement-review-chain | delegate a repo-less implement task | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| implement-review-chain | delegate a task whose reviewer exhausts the configured pool | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| implement-review-chain | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| agent-orchestration-session-message | server shutdown and fake-agent cleanup | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| cron-routes-and-ticks | wait for the manual cron firing report | triggers-cron-improve | `f77008f66` | Python persists the scheduled task's canonical `task_spec.md` and references; Rust omits them. |
| cron-routes-and-ticks | wait for a scheduler tick to fire the same task again | triggers-cron-improve | `f77008f66` | Python persists the scheduled task's canonical `task_spec.md` and references; Rust omits them. |
| cron-routes-and-ticks | server shutdown and fake-agent cleanup | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-artifacts-plan-fork | fork of the plan worker at a null index | features | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| features-artifacts-plan-fork | server shutdown and fake-agent cleanup | features | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-explain | server shutdown and fake-agent cleanup | features | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-harness-discord | allowed user mention starts a Discord thread session | features | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| features-harness-discord | release the silent summon result | features | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| features-harness-discord | server shutdown and fake-agent cleanup | features | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-harness-slack | allowed app mention starts its Slack thread session | features | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| features-harness-slack | release summon agent after its result | features | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| features-harness-slack | server shutdown and fake-agent cleanup | features | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-web-reply-audit | wait for the audit nudge | features | `854286c2a` | Python's web reply nudge includes the reason and full-reply instruction; Rust still sends the short prompt. |
| features-web-reply-audit | server shutdown and fake-agent cleanup | features | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| features-web-reply | a status set of now alone | features | `854286c2a` | Python's status-set response adds `now_chars` and `now_over_budget`; Rust omits both. |
| features-web-reply | a status set with a null goal | features | `854286c2a` | Python's status-set response adds `now_chars` and `now_over_budget`; Rust omits both. |
| features-web-reply | the status read lists the open waiting descendant only | features | `854286c2a` | Python's status response includes open direct children with a Now item; Rust omits `children`. |
| handover-admitted-input | server shutdown and fake-agent cleanup · python-to-candidate | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-admitted-input | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-drain | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-improve | server shutdown and fake-agent cleanup · python-to-candidate | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-improve | wait for the iteration at its first gate · candidate-to-python | triggers-cron-improve | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| handover-improve | server shutdown and fake-agent cleanup · candidate-to-python | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-manager-turn | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-review-run | server shutdown and fake-agent cleanup · python-to-candidate | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-review-run | delegate an implement task whose reviewer remains alive through handover · candidate-to-python | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| handover-review-run | release the adopted reviewer · candidate-to-python | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| handover-review-run | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-stop-request | request the stop of the gated run; the agent ignores the signal · candidate-to-python | agent-orchestration | `d401d9a2a` | Python escalates an ignored stop, terminates the agent, and records the interrupted outcome and exit code; Rust does not. |
| handover-stop-request | hand over the durable interrupted outcome · candidate-to-python | agent-orchestration | `d401d9a2a` | Python escalates an ignored stop, terminates the agent, and records the interrupted outcome and exit code; Rust does not. |
| handover-stop-request | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| handover-worker-run | wait for the worker agent at its live gate · candidate-to-python | agent-orchestration | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| handover-worker-run | release the adopted worker agent · candidate-to-python | agent-orchestration | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| handover-worker-run | server shutdown and fake-agent cleanup · candidate-to-python | agent-orchestration | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| improve-loop-reports | wait for both iteration reports to reach the busy parent | triggers-cron-improve | `253e0dcbe` | Python task prompts include the new “Text a model reads” block; Rust stores the older prompt and hash. |
| improve-loop-reports | server shutdown and fake-agent cleanup | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| skeleton | page with a session, bearer key | server-skeleton | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| skeleton | page with a session, no key | server-skeleton | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| skeleton | page with a worker session, cookie | server-skeleton | `90e8ac300`, `f77008f66` | Python's added worker transcript rows advance the page cursor from 10 to 12. |
| session-detail | bootstrap, manager root, starred, grouped | session-read-api | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| session-detail | events, newest page, manager root, starred, grouped | session-read-api | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| session-detail | bootstrap, manager child with child reports | session-read-api | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-detail | events, newest page, manager child with child reports | session-read-api | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-detail | bootstrap, worker, retried, closed | session-read-api | `90e8ac300`, `f77008f66` | Python's worker transcript projects the added run lifecycle rows, increasing the event count. |
| session-detail | events, newest page, worker, retried, closed | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-detail | bootstrap, worker, interrupted, archived | session-read-api | `90e8ac300`, `f77008f66` | Python's worker transcript projects the added run lifecycle rows, increasing the event count. |
| session-detail | events, newest page, worker, interrupted, archived | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-detail | bootstrap, worker, queued run stopped | session-read-api | `90e8ac300`, `f77008f66` | Python's worker transcript projects the added run lifecycle rows, increasing the event count. |
| session-detail | events, newest page, worker, queued run stopped | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-detail | bootstrap, fork of atlas | session-read-api | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| session-detail | events, newest page, fork of atlas | session-read-api | `ae5555da7` | Python records the router's quota failover message; Rust retains the old prompt-overlay warning. |
| session-runs | run context, manager turn | session-read-api | `f08291064` | Python removed `legacy_prompt` from run-context responses; Rust still includes the field. |
| session-runs | run context, failed manager turn | session-read-api | `f08291064` | Python removed `legacy_prompt` from run-context responses; Rust still includes the field. |
| session-runs | run context, failed work run | session-read-api | `f08291064` | The extra Rust legacy-prompt payload changes the `Accept-Encoding` Vary header versus Python. |
| session-runs | run context, successful work run | session-read-api | `f08291064` | Python removed `legacy_prompt` from run-context responses; Rust still includes the field. |
| session-runs | run context, interrupted work run | session-read-api | `f08291064` | The extra Rust legacy-prompt payload changes the `Accept-Encoding` Vary header versus Python. |
| session-runs | run context, stopped queued run | session-read-api | `f08291064` | Python removed `legacy_prompt` from run-context responses; Rust still includes the field. |
| session-runs | transcript, closed worker | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-runs | transcript, archived worker | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-runs | transcript, stopped worker | session-read-api | `f08291064` | The extra Rust legacy-prompt payload changes the `Accept-Encoding` Vary header versus Python. |
| session-runs | transcript, closed worker, revision capture | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-runs | transcript, closed worker, after the rendered rows | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-runs | transcript, closed worker, stale revision | session-read-api | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-runs | effective prompt, worker | session-read-api | `253e0dcbe` | Python's effective-prompt body and hash reflect the merged prompt text; Rust serves the old prompt. |
| session-runs | effective prompt, work kind on a manager | session-read-api | `253e0dcbe` | Python's effective-prompt body and hash reflect the merged prompt text; Rust serves the old prompt. |
| session-stream | worker session catch-up replays the runs transcript | session-stream | `90e8ac300`, `f77008f66` | Python's run summary includes the new tool-call count and lifecycle rows; Rust omits them. |
| session-stream | server shutdown and fake-agent cleanup | session-stream | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| session-write-agent | create under the run's own manager task | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-agent | release the agent caller turn | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-agent | server shutdown and fake-agent cleanup | session-write | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| session-write-fork | fork of the source at a null index, the whole log | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-fork | fork of the source at its first line | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-fork | fork of the source at a middle line | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-fork | fork of the source at its last line | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-fork | elone of the source at a middle line takes over and archives the source | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-fork | server shutdown and fake-agent cleanup | session-write | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| session-write-task | legacy create opens a manager root | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| session-write-task | v2 create under the manager root | session-write | `ae5555da7` | Python persists and returns the router-selected backend order and reason; Rust omits `route`. |
| wait-trigger-delay | server shutdown and fake-agent cleanup | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |
| wait-trigger-local-pid | server shutdown and fake-agent cleanup | triggers-cron-improve | `3cd98aa21`, `2fa0a4ce2` | Claude launch argv and env lag Python's per-run-kind tools, settings, and advisor disablement. |

## Catch-up e44c9ccef ignored Cargo tests

Each test below fails because Rust has not ported the Python behavior changed on `main`. Port tasks
remove its `catch-up e44c9ccef` ignore when they port that behavior.

The launch-environment helper's synthetic compaction case uses `run_kind="manager_turn"`: it covers a
manager turn, whose Python production caller `master_cc_relay` passes `ctx.run_kind` to
`compaction_env`.

| Row | Test | Binary | Main commit | Difference |
| --- | --- | --- | --- | --- |
| `cargo-claude-factory-matrix` | `registered_claude_factory_matches_the_python_argv_and_env_matrix` | `charliebot-agents --test claude_backend_golden` | `3cd98aa21`, `2fa0a4ce2`, `e001d863e` | Python now varies Claude launch tools and settings by run kind, sets advisor disablement, and adds a 15-minute Bash limit; Rust's argv and env matrix is stale. |
| `cargo-account-selection-matrix` | `account_selection_matrix_matches_python` | `charliebot-agents --test claude_pool_cross_impl` | `c9ade3029` | Python considers account contention below the 90% utilization line before idleness; Rust keeps the prior selection order. |
| `cargo-cli-backend-matrix` | `cli_backends_match_the_python_package_on_synthetic_inputs` | `charliebot-agents --test cli_backends_cross_implementation` | `3cd98aa21`, `2fa0a4ce2`, `e001d863e`, `63004f809` | Python's Claude launch matrix changed and Codex now includes `--disable multi_agent`; Rust's CLI argv and env differ. |
| `cargo-codex-matrix` | `codex_matches_the_python_package_on_synthetic_inputs` | `charliebot-agents --test codex_cross_implementation` | `63004f809` | Python's Codex argv includes `--disable multi_agent`; Rust omits the flag. |
| `cargo-launch-environment` | `complete_agent_child_environments_match_python_for_each_launch_kind` | `charliebot-agents --test launch_environment_cross_implementation` | `3cd98aa21`, `e001d863e` | Python supplies the 15-minute Bash limit and advisor disablement for manager turns; Rust's child environment lacks them. |
| `cargo-store-operations` | `python_and_rust_store_operations_write_identical_files_and_read_each_others_output` | `charliebot-store --test cross_implementation` | `ae5555da7` | Python serializes the new route metadata before `web_status`; Rust places the field after it. |
| `cargo-store-task-tree` | `python_and_rust_task_trees_share_nodes_gates_and_mutation_bytes` | `charliebot-store --test cross_implementation` | `ae5555da7` | Python omits unset route metadata; Rust serializes it as `null`. |
| `cargo-store-unnamed-node` | `python_and_rust_name_an_unnamed_node_from_a_corrupt_counter_the_same_way` | `charliebot-store --test cross_implementation` | `ae5555da7` | Python omits unset route metadata; Rust serializes it as `null`. |
| `cargo-store-fork-elone` | `python_and_rust_fork_and_elone_write_the_same_session_files_at_every_index` | `charliebot-store --test cross_implementation` | `ae5555da7` | Python omits unset route metadata; Rust serializes it as `null`. |
| `cargo-store-delegate-task` | `python_and_rust_delegate_a_task_spec_into_the_same_node_run_and_spec_bytes` | `charliebot-store --test cross_implementation` | `ae5555da7` | Python omits unset route metadata; Rust serializes it as `null`. |
| `cargo-prompt-snapshot` | `snapshots_match_the_python_server_byte_for_byte` | `charliebot-tasks --test prompt_snapshot_golden` | `253e0dcbe` | Python task prompts now include the "Text a model reads" block; Rust's snapshot contains the old prompt and hash. |
