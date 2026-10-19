# Port status

One row per route of the Python app (`server.app.routes` at `PORT_BASELINE`), in its route order.

- **Stage** is the port stage that owns the route: `foundation` for the routes this workspace
  serves, otherwise the route family a later stage ports, named by the Python package that registers
  it (`sessions` is `src/runtime/api/sessions.py`, `features/cron` is `src/features/cron/`).
- **Status** is `ported` when `charliebot-server` answers the route as the Python server does, else
  `todo`. A `todo` route answers as an unknown path: 404 with the key, 401 without it.

| Method | Path | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| GET, HEAD | `/openapi.json` | api-docs | todo |  |
| GET, HEAD | `/docs` | api-docs | todo |  |
| GET, HEAD | `/docs/oauth2-redirect` | api-docs | todo |  |
| GET, HEAD | `/redoc` | api-docs | todo |  |
| GET | `/api/sessions/chat-threads` | features/chat_threads | ported |  |
| GET | `/api/auth/status` | foundation | ported |  |
| GET | `/sessions/{session_id}/events` | pages | todo |  |
| GET | `/home` | pages | todo |  |
| GET | `/` | foundation | ported | renders the empty-sessions context; a home with sessions answers 500 until the sessions stage |
| GET | `/api/sessions/` | sessions | todo |  |
| POST | `/api/sessions/` | sessions | todo |  |
| GET | `/api/sessions/archived` | sessions | todo |  |
| GET | `/api/sessions/starred` | sessions | todo |  |
| GET | `/api/sessions/groups` | sessions | todo |  |
| POST | `/api/sessions/groups/rename` | sessions | todo |  |
| POST | `/api/sessions/groups/delete` | sessions | todo |  |
| GET | `/api/sessions/status` | sessions | todo |  |
| GET | `/api/sessions/tree` | sessions | todo |  |
| GET | `/api/sessions/search` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/pending-triggers` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/bootstrap` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/read` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/usage` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/events` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/transcript` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/fork` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/elone` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/backend` | sessions | todo |  |
| GET | `/api/sessions/{session_id}` | sessions | todo |  |
| DELETE | `/api/sessions/{session_id}` | sessions | todo |  |
| DELETE | `/api/sessions/{session_id}/permanent` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/unarchive` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/star` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/unstar` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/rounds/{round_id}/rate` | sessions | todo |  |
| PATCH | `/api/sessions/{session_id}` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/group` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/events.jsonl` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/runs` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/effective-prompt` | sessions | todo | Reply segments from web_reply, Slack and Discord are not ported. |
| GET | `/api/sessions/{session_id}/runs/{run_id}/context` | sessions | todo | Missing-run detail differs in the minted ID until R2a lands. |
| POST | `/api/sessions/{session_id}/retry` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/task-inputs/pending` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/task-inputs/acknowledge` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/complete` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/cancel` | sessions | todo |  |
| POST | `/api/sessions/{session_id}/runs/{run_id}/cancel` | sessions | todo |  |
| POST | `/api/chat/{session_id}/upload` | chat | todo |  |
| POST | `/api/chat/{session_id}/message` | chat | todo |  |
| POST | `/api/chat/{session_id}/cancel` | chat | todo |  |
| GET | `/api/internal/version` | foundation | ported |  |
| POST | `/api/internal/delegate` | internal | todo |  |
| POST | `/api/internal/schedule-trigger` | internal | todo |  |
| POST | `/api/internal/triggers/{session_id}/{trigger_id}/cancel` | internal | todo |  |
| POST | `/api/internal/session-message` | internal | todo |  |
| WS | `/ws/sessions/{session_id}` | session-stream | todo | the token check is ported; every upgrade answers 403 |
| POST | `/api/internal/plan/present` | features/artifacts | todo |  |
| POST | `/api/internal/plan/amend` | features/artifacts | todo |  |
| POST | `/api/internal/plan/approve` | features/artifacts | todo |  |
| POST | `/api/internal/plan/close` | features/artifacts | todo |  |
| GET | `/api/sessions/{session_id}/plans` | features/artifacts | todo |  |
| GET | `/api/code-server/open` | features/code_server | todo |  |
| GET | `/api/cron/tasks` | features/cron | todo |  |
| PUT | `/api/cron/tasks/{name}` | features/cron | todo |  |
| POST | `/api/cron/tasks` | features/cron | todo |  |
| POST | `/api/cron/tasks/{name}/run` | features/cron | todo |  |
| DELETE | `/api/cron/tasks/{name}` | features/cron | todo |  |
| POST | `/api/diag/switch-events` | features/diag | todo |  |
| POST | `/api/internal/slack/reply` | features/slack | todo |  |
| POST | `/api/internal/slack/ack` | features/slack | todo |  |
| POST | `/api/internal/discord/reply` | features/discord | todo |  |
| POST | `/api/internal/discord/read` | features/discord | todo |  |
| POST | `/api/internal/discord/check` | features/discord | todo |  |
| POST | `/api/internal/web/reply` | features/web_reply | todo |  |
| GET | `/api/internal/web/dialog` | features/web_reply | todo |  |
| POST | `/api/internal/web/status` | features/web_reply | todo |  |
| GET | `/api/internal/web/status` | features/web_reply | todo |  |
| GET | `/host-auth` | features/host_auth | ported | renders through the shared `Templating` handle |
| GET | `/api/host-auth/status` | features/host_auth | ported |  |
| POST | `/api/host-auth/probe` | features/host_auth | ported |  |
| GET | `/api/git/branches` | features/diff_view | todo |  |
| GET | `/api/git/repos` | features/diff_view | todo |  |
| GET | `/api/git/diff/files` | features/diff_view | todo |  |
| GET | `/api/git/diff/file` | features/diff_view | todo |  |
| GET | `/diff` | features/diff_view | todo |  |
| POST | `/api/sessions/{session_id}/explain` | features/explain | ported |  |
| GET | `/api/sessions/{session_id}/explain` | features/explain | ported |  |
| GET | `/api/sessions/{session_id}/explain/status` | features/explain | ported |  |
| POST | `/api/internal/improve/stop` | features/improve | todo |  |
| POST | `/api/internal/improve` | features/improve | todo |  |
| GET | `/ncu` | features/ncu | todo |  |
| WS | `/ws/terminal` | features/terminal | ported |  |
| GET | `/perfetto` | features/trace | ported |  |
| GET | `/perfetto/merged` | features/trace | ported |  |
| GET | `/api/ext-usage` | features/usage | todo |  |
| GET | `/token-usage` | features/usage | todo |  |
| POST | `/api/voice/{session_id}/confirm` | features/voice | todo |  |
| POST | `/api/voice/{session_id}` | features/voice | todo |  |
| WS | `/ws/voice/{session_id}` | features/voice | ported | live preview relay and recording archive |
| POST | `/api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages` | backends/openai_compatible | ported | streaming and JSON answers; parity stage other-backends |
| GET, HEAD | `/absolute_filepath/{path:path}` | features/files | ported |  |
| GET, HEAD | `/static/features/artifacts/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/backup/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/backend_pool/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/chat_threads/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/code_server/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/cron/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/diag/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/slack/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/discord/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/web_reply/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/host_auth/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/diff_view/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/explain/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/improve/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/memory/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/ncu/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/terminal/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/trace/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/usage/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/voice/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/openai_compatible/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/antigravity/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/charlie_code/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/claude_code/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/codex/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/gemini/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/kimi/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/backends/opencode/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/features/files/{path:path}` | foundation | ported |  |
| GET, HEAD | `/static/{path:path}` | foundation | ported |  |

## Feature hooks

| Kind | Name | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| config section | `voice` | features/voice | ported | five fields, legacy keys and unknown-key refusal |
| config check | `check_default_backend` | features/voice | ported | validates against local, gemini and muse |
| template global | `voice_backends` | features/voice | ported | backend availability in dropdown order |
| template global | `voice_default_backend` | features/voice | ported | |
| turn contribution | `voice` | features/voice | ported | dictated input preamble |
| turn contribution | `chat_threads` | features/chat_threads | ported | names `thread_session.md` for a Slack or Discord thread session |

## Services and lifespan

Baseline: `rust/docs/contract/state/services.md`. Status describes this Rust workspace at this
stage; `ported` means the startup or service behavior is implemented, `todo` means it is not yet
implemented.

| Kind | Service or step | Stage | Status | Note |
| --- | --- | --- | --- | --- |
| pre-loop | `agent_environment.apply_agent_environment` | foundation | todo | |
| pre-loop | `config.get_config` | foundation | ported | |
| pre-loop | `config.require_backends` | foundation | ported | |
| pre-loop | `v1_sessions.require_no_v1_sessions` | foundation | ported | Runs before bind. |
| pre-loop | Registered startup checks | foundation | todo | Cron and usage backend-reference checks. |
| pre-loop | Start server on configured host and port | foundation | ported | |
| lifespan | 1. Read config | foundation | ported | |
| lifespan | 2. Acquire home writer fence | foundation | ported | |
| lifespan | 3. Capture build information | foundation | ported | |
| lifespan | 4. Initialize the home | foundation | ported | |
| lifespan | 5. Log and sweep session cgroups | foundation | todo | |
| lifespan | 6. Start task-tree recovery | agent-runtime | todo | |
| lifespan | 7. Start early services | services | todo | |
| lifespan | 8. Await task-tree recovery | agent-runtime | todo | |
| lifespan | 9. Recover pending triggers | agent-runtime | todo | |
| lifespan | 10. Start ready services | services | todo | |
| lifespan | 11. Log ready and serve | foundation | ported | |
| lifespan | 12. Stop services and close shared clients | services | todo | Signal shutdown and writer-fence release are implemented; service teardown is not. |
| lifespan | 13. Release home writer fence | foundation | ported | |
| service | `scheduler` | features/cron | todo | |
| service | `slack` | features/slack | todo | |
| service | `discord` | features/discord | todo | |
| service | `host_auth` | features/host_auth | ported | one round at start, then one per 1800 s |
| service | `merge_pool` | features/trace | ported | In-process builds share one cache-key flight and a bounded build gate. |
| service | `usage_tally` | features/usage | todo | |
| service | `ext_usage` | features/usage | todo | |
| service | `speech` | features/voice | todo | |
| registration | diff root `memory` | features/memory | ported | `<home>/memory`. The diff view that reads it is not ported (features/diff_view). |
| registration | turn contribution `memory` | features/memory | ported | Adds the memory segments of every run's managed instructions. No Rust code launches a run yet, so nothing calls it until the run launch path lands. |
| registration | turn contribution `web_reply`: instruction segments | features/web_reply | ported | Adds `prompts/web_reply.md` to a manager run of a web session. The run launch path that assembles the segments is not ported yet. |
| registration | turn contribution `slack`: instruction segments | features/slack | ported | Adds the thread reply contract to a manager run of a Slack thread session. The round-end audit and the rest of `thread_entry.py` are not ported yet. |
| registration | turn contribution `discord`: instruction segments | features/discord | ported | Adds the thread reply contract to a manager run of a Discord thread session. The round-end audit and the rest of `thread_entry.py` are not ported yet. |

## State files

Baseline: `rust/docs/contract/state/files.md`. `S1` covers session metadata and chat history. Later
run-record work is `S2`; task-tree runtime and feature-owned files remain with their named stages.

| File kind | Stage | Status | Note |
| --- | --- | --- | --- |
| `config.yaml` | foundation | ported | |
| `credentials.yaml` | foundation | ported | |
| `config.d/cron.d/<name>.yaml` | features/cron | todo | |
| `MASTER_AGENT_PROMPT.md` | agent-runtime | todo | |
| `state/home_writer.lock` | foundation | ported | |
| `state/writer_identity.json` | foundation | ported | |
| `state/cron_alert_fingerprint.json` | features/cron | todo | |
| `cache/cli_base_url.json` | foundation | todo | |
| `cache/perfetto_merge/` | features/trace | ported | Cache directory for merged Perfetto traces. |
| `cache/perfetto_merge/<sha256>.json.gz` | features/trace | ported | |
| `/tmp/merge-chunks-*` | features/trace | not applicable | the Rust merge runs in-process (parent-approved); the cache key is unchanged |
| `/tmp/merge-members-*` | features/trace | not applicable | the Rust merge runs in-process (parent-approved); the cache key is unchanged |
| `host_auth.json` | features/host_auth | ported | written atomically with `json.dumps(indent=2)` and a newline |
| `usage/ledger.sqlite3` | features/usage | todo | |
| `prompt_bodies/<sha256>.md` | agent-runtime | todo | |
| `memory/` | features/memory | ported | Git repository created by `ensure_store` on first use. |
| `memory/topics` | features/memory | ported | Seeded when missing; read by the selection. |
| `memory/.gitignore` | features/memory | ported | Seeded when missing. |
| `memory/entries/<topic>/<slug>.md` | features/memory | ported | Read only: format v2 grammar and selection. |
| `memory/staging/` | features/memory | ported | Created by `ensure_store`; never read by a selection. |
| `memory/staging/<stamp>-<sess8>-<slug>.md` | features/memory | not applicable | Written by `charliebot memory add`: the Python CLI stays (Trade-off 2). |
| `memory-proposal/` | features/memory | not applicable | Created by `charliebot memory proposal open`: the Python CLI stays (Trade-off 2). |
| `models/` speech assets | features/voice | todo | |
| `vendor/katex/katex.min.js` | foundation | todo | |
| `voice_eval/<ts>/results.json` | features/voice | todo | |
| `voice_eval/<ts>/summary.md` | features/voice | todo | |
| `opencode/` | backends/opencode | todo | |
| `sessions/.counter` | agent-runtime | todo | |
| `sessions/.task-<id>-<pid>-<uuid>.tmp/` | agent-runtime | todo | |
| in-memory usage caches | features/usage | todo | Not persisted files. |
| `sessions/<sid>/metadata.json` | S1 | ported | Ordered typed metadata, registered slots, unknown keys, atomic writes. |
| `sessions/<sid>/threads/<tid>/metadata.json` | S1 | ported | Typed thread model and registered slots. |
| `sessions/<sid>/data/chat_events.jsonl` | S1 | ported | Append, sync, tolerant NDJSON reads, cache and ranges. |
| `sessions/<sid>/data/archives/chat_events.<year>-W<week>.jsonl` | S1 | ported | Archive rotation and global `archive_offset`. |
| `sessions/<sid>/data/explain_results.json` | features/explain | ported | Written atomically as Python writes it; stale pending entries are reaped on read. |
| `sessions/<sid>/data/runs/<run>/task_spec.md` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/metadata.json` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/events.jsonl` | S2 | planned | |
| `sessions/<sid>/data/runs/<run>/agent.raw.ndjson` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/agent.stderr.log` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/agent.raw.cursor` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/agent.raw.ndjson.<n>` | agent-runtime | todo | Rotated raw output. |
| `sessions/<sid>/data/runs/<run>/agent.stderr.log.<n>` | agent-runtime | todo | Rotated stderr. |
| `sessions/<sid>/data/runs/<run>/stdout.log` | backends/antigravity | todo | |
| `sessions/<sid>/data/runs/<run>/stderr.log` | backends/antigravity | todo | |
| `sessions/<sid>/data/runs/<run>/task.md` | backends/charlie-code | todo | |
| `sessions/<sid>/data/runs/<run>/harness_instructions.md` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/prompt_snapshot.json` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/launch_prompt.md` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/cold_read.json` | agent-runtime | todo | |
| `sessions/<sid>/data/runs/<run>/hang_diagnostics.json` | agent-runtime | todo | |
| legacy `sessions/<sid>/data/master_runs/<started_at>/` | agent-runtime | todo | Read-only baseline layout. |
| legacy `sessions/<sid>/threads/<tid>/data/` | agent-runtime | todo | Read-only baseline layout. |
| `sessions/<sid>/triggers/<trigger_id>.json` | agent-runtime | todo | |
| `sessions/<sid>/launches/<launch_id>/metadata.json` | foundation | todo | |
| `sessions/<sid>/launches/<launch_id>/log` | foundation | todo | |
| `sessions/<sid>/launches/<launch_id>/exit` | foundation | todo | |
| `sessions/<sid>/plans.json` | features/artifacts | todo | |
| session artifact pages | features/artifacts | todo | |
| `sessions/<sid>/uploads/<filename>` | chat | todo | |
| `sessions/<sid>/voice/<stamp>_<id>.wav` | features/voice | ported | 16 kHz mono PCM16 WAV |
| `sessions/<sid>/voice/<stamp>_<id>.txt` | features/voice | ported | final transcript |
| `sessions/<sid>/loops/active.lock` | features/improve | todo | |
| `sessions/<sid>/loops/<n>/state.json` | features/improve | todo | |
| `sessions/<sid>/loops/<n>/goal.md` | features/improve | todo | |
| `sessions/<sid>/loops/<n>/plan.md` | features/improve | todo | |
| `sessions/<sid>/loops/<n>/iter_<n>.md` | features/improve | todo | |
| `sessions/<sid>/CLAUDE.md` instructions | agent-runtime | todo | |
| `sessions/<sid>/AGENTS.md` instructions | agent-runtime | todo | |
| session cgroup directories | agent-runtime | todo | Outside the home. |
| Claude `.credentials.json` | backends/claude-code | ported | Read for the panel and renewed by `claude_code/usage_quota.rs` (atomic write, mode 0600); account health reads in `claude_code/claude_accounts.rs`. Outside the home. |
| Claude and Codex transcript files | backends/claude-code, backends/codex | ported | Read by `claude_code/usage_logs.rs` and `codex/usage_logs.rs`; swept by `claude_code/usage_sweep.rs` and `codex/usage_sweep.rs`; live pool copies trimmed by `claude_accounts::retire_transcript_copies`. Outside the home. |
| worktree directories | agent-runtime | todo | Outside the home. |
| worktree trash | agent-runtime | todo | Outside the home. |
| sibling backup archives | features/backup | todo | Outside the home. |
| `<publish.dir>/<token>/<basename>` | CLI publish | todo | Outside the home. |
| one-shot backend temporary directories | backends | todo | Outside the home. |
| explain temporary directories | features/explain | ported | Outside the home. A read-only history copy per run, removed when the run ends. |
| headless-render temporary directories | features/artifacts | todo | Outside the home. |
| trace-merge temporary directories | features/trace | todo | Outside the home. |
| memory proposal temporary extraction | features/memory | not applicable | Outside the home. The Python CLI stays (Trade-off 2). |
| SSH ControlMaster sockets | foundation | todo | Outside the home. |
| artifact page-height probe files | features/artifacts | todo | Beside an artifact. |
| tmux socket `charliebot` | features/terminal | ported | Outside the home. |
| tmux session `charliebot-<id>` | features/terminal | ported | Detached terminal shell. |
| repo config examples and prompt inputs | foundation | reader-only | Not written by the server. |
