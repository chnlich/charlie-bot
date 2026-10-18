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
| GET | `/api/sessions/chat-threads` | features/chat_threads | todo |  |
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
| GET | `/api/sessions/{session_id}/effective-prompt` | sessions | todo |  |
| GET | `/api/sessions/{session_id}/runs/{run_id}/context` | sessions | todo |  |
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
| GET | `/host-auth` | features/host_auth | todo |  |
| GET | `/api/host-auth/status` | features/host_auth | todo |  |
| POST | `/api/host-auth/probe` | features/host_auth | todo |  |
| GET | `/api/git/branches` | features/diff_view | todo |  |
| GET | `/api/git/repos` | features/diff_view | todo |  |
| GET | `/api/git/diff/files` | features/diff_view | todo |  |
| GET | `/api/git/diff/file` | features/diff_view | todo |  |
| GET | `/diff` | features/diff_view | todo |  |
| POST | `/api/sessions/{session_id}/explain` | features/explain | todo |  |
| GET | `/api/sessions/{session_id}/explain` | features/explain | todo |  |
| GET | `/api/sessions/{session_id}/explain/status` | features/explain | todo |  |
| POST | `/api/internal/improve/stop` | features/improve | todo |  |
| POST | `/api/internal/improve` | features/improve | todo |  |
| GET | `/ncu` | features/ncu | todo |  |
| WS | `/ws/terminal` | features/terminal | todo | the token check is ported; every upgrade answers 403 |
| GET | `/perfetto` | features/trace | todo |  |
| GET | `/perfetto/merged` | features/trace | todo |  |
| GET | `/api/ext-usage` | features/usage | todo |  |
| GET | `/token-usage` | features/usage | todo |  |
| POST | `/api/voice/{session_id}/confirm` | features/voice | todo |  |
| POST | `/api/voice/{session_id}` | features/voice | todo |  |
| WS | `/ws/voice/{session_id}` | features/voice | todo | the token check is ported; every upgrade answers 403 |
| POST | `/api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages` | backends/openai_compatible | todo |  |
| GET, HEAD | `/absolute_filepath/{path:path}` | features/files | todo |  |
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
