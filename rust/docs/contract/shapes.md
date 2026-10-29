# Shared shapes

This file holds the JSON shapes and rules that more than one family file uses.

## JSON encoding

Three encoders produce the response bodies:

| Encoder | Used by | Separators | Non-ASCII |
|---|---|---|---|
| `orjson` (`fast_json_bytes`, `src/infra/responses.py:37`) | `FastJsonResponse`, `PreencodedJSONResponse`, WebSocket broadcasts | compact | raw UTF-8 |
| Starlette `JSONResponse` and FastAPI's default response | plain `dict` returns and `JSONResponse(...)` | compact (`,` and `:`) | raw UTF-8 |
| Python `json.dumps` | the auth middleware 401 body | `, ` and `: ` | `\u` escapes |

Clients parse the JSON. The Rust port must keep the parsed content. It must also keep the 401 body bytes, see [auth.md](auth.md#rejection).

Key order is the insertion order shown in each shape. Clients do not depend on key order, but the parity harness compares bodies, so the port must keep the documented order.

A FastAPI route with a `response_model` filters and validates its return through that model. The output keys are the model fields in declaration order.

### Time values

Two time forms appear on the wire:

| Form | Example | Where |
|---|---|---|
| pydantic UTC datetime | `2026-01-01T12:00:00.123456Z` | Every datetime field of a pydantic model dump: `created_at`, `updated_at`, `thinking_since` in listing rows, `fire_at`, and others. |
| Python `isoformat()` | `2026-01-01T12:00:00.123456+00:00` | Event `timestamp` values written by the server, `thinking_since` and `next_trigger_at` in `GET /api/sessions/status`, `thinking_since` in the `running_changed` frame and the transcript route. |

The microsecond part is omitted when it is zero, in both forms.

## Error bodies

| Shape | Producer |
|---|---|
| `{"detail": "<message>"}` | `HTTPException` with a string detail. Most errors. |
| `{"detail": {"message": "<text>", "blockers": ["<text>", …]}}` | Task conflicts (409), see below. |
| `{"detail": {"error": "stale_thread", …}}` | The reply routes (412), see [replies.md](replies.md). |
| `{"detail": [<validation error>, …]}` | Request validation (422), see [middleware.md](middleware.md#framework-error-responses). |
| `{"error": "<message>"}` | The voice HTTP routes, see [voice.md](voice.md). |
| `Internal Server Error` (text/plain) | Unhandled exceptions. |

### Task error mapping

The task routes translate domain errors with `_task_http_error` (`src/runtime/api/sessions.py:793`). The check order matters, because `TaskArchivedError` is a subclass of `TaskConflictError`.

| Error | Status | `detail` |
|---|---|---|
| `TaskArchivedError` | 409 | `"task <id> is archived"` (a string) |
| `TaskInvalidError` | 400 | the message |
| `TaskNotFoundError`, `RunNotFoundError` | 404 | the message |
| `TaskForbiddenError`, `DelegationBlockedError` | 403 | the message |
| `TaskConflictError`, `RunIdentityConflictError` | 409 | `{"message": "<blockers joined with '; '>", "blockers": [...]}`. For an error with no `blockers`, the list is `[message]`. |
| any other exception | 500 | `Internal Server Error` |

Fixed messages the port must keep:

| Constant | Text |
|---|---|
| `SESSION_NOT_FOUND_DETAIL` | `Session not found` |
| `TASK_CREATE_REQUEST_ID_REQUIRED` | `request_id is required for task creation` |
| `AGENT_CREATE_SCOPE_REFUSAL` | `an agent may only create a task directly under its own open manager task` |
| `_UNKNOWN_BACKEND_DETAIL` | `backend '<id>' is not a recognized backend id; valid ids: <sorted Python list repr>`, for example `valid ids: ['claude-opus']` |
| run miss | `run <run_id> not found in task <session_id>` |

## Request processing order

FastAPI handles one request in this order:

1. Read the body. A body that is not valid JSON on a JSON route gives 422 with `type` `json_invalid`.
2. Call the dependencies in declaration order. A dependency that raises `HTTPException` ends the request with that error.
3. Validate the path, query, header and cookie parameters, then the body.
4. When step 3 found errors, return 422 with all of them.
5. Call the handler.

For example, `POST /api/sessions/{session_id}/fork` with an unknown session and a bad run token gives 404, because `require_session` comes before `require_caller`.

`deps.require_session` loads the session and gives 404 `Session not found` when it is absent. The family files list it as a dependency where a route uses it.

## Session JSON

`SessionMetadata` (`src/infra/models.py:354`) is the session record. The state contract documents the stored file. The API dumps it with `model_dump(mode="json")`.

Keys in output order, with types:

| Key | Type |
|---|---|
| `id` | string |
| `name` | string |
| `status` | `"active"` or `"archived"` |
| `has_unread` | bool |
| `has_running_tasks` | bool |
| `work_state` | `"idle"`, `"running"`, `"waiting"` or null |
| `has_pending_trigger` | bool |
| `pending_trigger_count` | int |
| `next_trigger_at` | datetime or null |
| `has_pending_plan_approval` | bool |
| `starred` | bool |
| `thinking_since` | datetime or null |
| `run_backend` | string or null |
| `created_at` | datetime |
| `updated_at` | datetime |
| `cc_session_id` | string or null |
| `cc_session_started_at` | datetime or null |
| `claude_account` | slot value or null |
| `backend` | string |
| `scheduled_task` | slot value or null |
| `last_scheduled_run` | slot value or null |
| `last_run_status` | slot value or null |
| `last_scheduled_cron` | slot value or null |
| `schedule_cron` | string or null |
| `schedule_enabled` | bool or null |
| `schedule_next_run` | string or null |
| `schedule_timezone` | string or null |
| `schedule_project` | string or null |
| `schedule_allow_failure` | bool or null |
| `parent_session_id` | string or null |
| `successor_session_id` | string or null |
| `slack_origin` | slot value or null |
| `slack_watermark_ts` | slot value or null |
| `discord_origin` | slot value or null |
| `discord_watermark_id` | slot value or null |
| `schema_version` | `2` |
| `task_parent_id` | string or null |
| `profile` | `"manager"` or `"worker"` |
| `task` | TaskSpec object or null |
| `created_by_event` | `{"session_id": string, "event_id": string or null}` or null |
| `origin_ref` | same shape as `created_by_event`, or null |
| `subtree_prompt_ref` | string or null |
| `node_prompt_ref` | string or null |
| `native_prompt_hash` | string or null |
| `native_backend` | string or null |
| `native_model` | string or null |
| `round_ratings` | object: round id to `"thumbs_up"` or `"thumbs_down"` |
| `group` | string or null |
| `archive_offset` | int |
| `backend_from_pool` | bool |
| `web_status` | slot value or null |

"Slot value" keys belong to feature packages. The registered slot keys and their owners:

| Owner | Keys |
|---|---|
| `backend_pool` | `backend_from_pool` |
| `cron` | `scheduled_task`, `last_scheduled_run`, `last_run_status`, `last_scheduled_cron` |
| `slack` | `slack_origin`, `slack_watermark_ts` |
| `discord` | `discord_origin`, `discord_watermark_id` |
| `web_reply` | `web_status` |
| `claude_code` | `claude_account` |

A model dump places the slot keys at the positions shown above. `backend_from_pool` and `web_status` come last, after any extra keys a response model adds.

`TaskSpec` keys: `goal` (string, default `""`), `acceptance` (list of strings), `context_refs` (list of strings), `repo_path` (string or null), `base_branch` (string or null), `task_type` (`implement`, `quick-edit`, `script-run`, `verify`, `cold-read` or null), `keep_worktree` (bool), `cold_read` (`{"page_path","genre","trigger"}` or null).

The raw model dump has `work_state: null` until the sidebar state is probed.

### Response forms

| Form | Built by | Difference from the model dump |
|---|---|---|
| model | `response_model=SessionMetadata` routes (create, fork, elone, backend switch, star, unstar, rate, group) | none |
| row | lists and `session` in bootstrap and usage | `task.goal` is removed (the rest of `task` stays). |
| listing row | `GET /api/sessions/`, `/chat-threads`, `/archived`, `/starred`, and the index page's `initial_sessions_json` | See below. |
| detail | `GET /api/sessions/{session_id}`, `PATCH`, task cancel, complete (200) | See below. |

### Listing rows

A listing row is the row form with the schedule join applied (`apply_listing_fields`, `src/runtime/api/sessions.py:231`):

1. Remove the six keys `schedule_cron`, `schedule_timezone`, `schedule_enabled`, `schedule_next_run`, `schedule_project`, `schedule_allow_failure`.
2. Append the listing fields of the cron controller at the end:
   - an unbound row gets `"schedule_task": null`;
   - a row bound to a scheduled task gets `schedule_task` (task name), `schedule_cron`, `schedule_timezone`, `schedule_enabled`, `schedule_next_run` (ISO string of the next run) and `schedule_allow_failure`, in that order.

The index page's `initial_sessions_json` rows keep `task.goal`. Every other listing row removes it.

`GET /api/sessions/` and `/chat-threads` also overlay the derived sidebar state onto each row, in the model positions: `thinking_since`, `has_running_tasks`, `has_pending_trigger`, `pending_trigger_count`, `next_trigger_at`, `has_pending_plan_approval`, `backend` (for a worker row: its newest run's backend, else its own), and `work_state` when the state has one.

### Detail form

`SessionDetailResponse` (`src/runtime/api/sessions.py:750`) is the model plus derived task fields. Keys: the model keys up to `archive_offset`, then:

| Key | Type |
|---|---|
| `task_state` | `"open"`, `"completed"`, `"cancelled"` or `"archived"` |
| `archived` | bool |
| `ancestors` | list of `{"id", "name"}`, root first |
| `prompt_rules` | object, see below |

Then `backend_from_pool` and `web_status`. `work_state` is `"idle"` when no verdict exists. `task.goal` is present.

`prompt_rules` from `task_mgr.session_detail`:

```json
{
  "subtree": {"ref": null, "source": null, "chars": 0, "text": null},
  "node": {"ref": null, "source": null, "chars": 0, "text": null},
  "affected_descendants": 0
}
```

The name-only `PATCH` path builds the detail from the model alone. Its `prompt_rules` is `{}`, `task_state` is `"open"`, `archived` is false and `ancestors` is `[]`.

## Run rows

`RunRow` (`src/infra/models.py`) is a run record plus two derived keys. Keys in order:

`id`, `session_id`, `kind`, `input_event_ids`, `task_spec_hash`, `task_spec_ref`, `prompt_snapshot_ref`, `retry_of_run_id`, `review_of_run_id`, `backend`, `model`, `native_session_id`, `pid`, `pid_start`, `started_at`, `ended_at`, `exit_code`, `repo_path`, `base_branch`, `branch_name`, `worktree_path`, `sequence_ref`, `raw_log_ref`, `events_ref`, `result_ref`, `state`, `stop_requested`.

- `kind`: `manager_turn`, `work`, `review`, `iteration` or `scheduled_step`.
- `sequence_ref`: `{"kind", "owner_ref", "position"}` or null.
- `state`: the display state folded from the run facts (default `queued`).
- `stop_requested`: true when a stop request fact exists for the run.

The state contract documents the run record file and its facts.

## Pending trigger

`PendingTrigger` (`src/infra/models.py:318`), dumped with `mode="json"`:

| Key | Type |
|---|---|
| `id` | string (UUID) |
| `session_id` | string |
| `fire_at` | datetime |
| `message` | string (at most 200 characters) |
| `created_at` | datetime |
| `status` | `"pending"`, `"fired"` or `"cancelled"` |
| `fired_at` | datetime or null |
| `watch_targets` | list of watch targets |
| `fire_reason` | string or null |

A watch target is one of:

- `{"kind":"local_pid","pid":int,"exit_file":string|null,"log":string|null}`
- `{"kind":"remote_pid","host":string,"pid":int,"exit_file":string|null,"log":string|null}`
- `{"kind":"slurm_job","host":string|null,"job_id":int}`

The `status` values come from `TriggerStatus` (`src/infra/models.py:84`).

## Chat message objects

The `MessageAggregator` (`src/runtime/message_aggregator.py:493`) turns raw chat events into message objects. The same objects appear in:

- the WebSocket `message` and `stream` frames;
- `messages` and `pending_draft` of the bootstrap route;
- `messages` of the events page and the transcript route;
- the index page bootstrap.

### Deltas

- `{"type":"message","message":<message>}`: a committed message.
- `{"type":"stream","message":<assistant draft>}`: the current assistant draft. It replaces the previous draft.

### Common keys

Every message has `role`, `content` (except `separator`), `event_index`, `id` and `timestamp`:

- `event_index`: the global index of the source event (archive offset plus the line index).
- `id`: the event `id`, or `legacy:<event_index>` when the event has none.
- `timestamp`: the event `timestamp`, or the value the role sets.

For roles built by a render handler, the aggregator sets `timestamp` (when the handler did not), then `event_index`, then `id`, after the handler keys.

### Assistant draft

The aggregator buffers `assistant`, `tool_use`, `tool_result` and `thinking` events into one draft:

```json
{"role":"assistant","content":"…","event_index":12,"id":"…","timestamp":"…","tools":[…],"thinking":"…"}
```

- `tools` is present only when the draft has tool rows.
- `thinking` is present only when the draft has thinking text.
- Each new text block after existing text commits the old draft as a `message` and starts a new one.
- Any event with a render handler, and any real `user` event, commits the draft first.

A tool row:

```json
{"name":"Bash","input":{"command":"…"},"output":"…","is_error":false}
```

The aggregator trims tool rows at ingestion (`tool_preview`, `src/runtime/message_aggregator.py:70`):

- A string `output` longer than 500 characters is cut to 500, and `output_truncated: true` is added.
- In `input`, the fields the browser reads keep up to 500 characters. Other string fields keep up to 60. A cut adds `input_truncated: true`.
- The read fields: `Bash` and `bash` read `command`; `Read`, `Edit` and `Write` read `file_path`; `Glob` reads `pattern`; `Grep` reads `pattern` and `path`; any other tool reads its first input key.

An `ExitPlanMode` tool use with a `plan` input commits the draft and emits `{"role":"plan","content":<plan>,…}`. With no plan text, the buffered assistant text becomes the plan message.

### User message

```json
{"role":"user","content":"…","uploaded_files":[{"filename":"…","path":"…","size":123}],"input_mode":null,"event_index":3,"id":"…","timestamp":"…","request_id":"…"}
```

`request_id` is present when the stored user event came from a request-keyed input. The browser uses it to match a
server echo to the optimistic message with the same request id.

A `user` event with a `message` key and no `content` key is a tool-result carrier. It attaches outputs to the newest tool rows and commits nothing.

A legacy event with no `uploaded_files` may carry an `[Attached files]` footer in its content. The aggregator strips that footer and builds `uploaded_files` from its `- <path>` lines.

### Roles from render handlers

| Event type | Message keys (besides the common keys) |
|---|---|
| `master_done` | `role: "separator"`, `thinking_seconds` (number or null). No `content`. |
| `assistant_error` | `role: "system"`, `content: "Error: <content>"` |
| `error` | `role: "system"`, `content: "Error: <content or message or 'Unknown error'>"` |
| `task_delegated` | `role: "task_delegated"`, `content: "Task delegated"`, `thread_id`, `child_session_id`, `delegate_invocation`, `backend`, `model`, `timestamp` (event `timestamp` or `created_at`) |
| `worker_summary` | `role: "worker_summary"`, `content`, `thread_id`, `origin_session_id` (only when set) |
| `handler_result` | `role: "system"`, `content: "✓ <task>: <message>"` (`✗` when `status` is not `ok`) |
| `context_compacted` | `role: "system"`, `content` (see the source), `kind: "context_compacted"` |
| `context_compact_failed` | `role: "system"`, `kind: "context_compact_failed"`, `content` |
| `resume_context_dropped` | `role: "system"`, `content` |
| `system` | only subtype `command_progress`: `role: "system"`, `content`. Other subtypes render nothing. |
| `clone_start` | `role: "clone_start"`, `content` (parent name, default `Unknown session`), `parent_session_id` |
| `scheduled_trigger` | `role: "scheduled_trigger"`, `content` |
| `agent_message` | `role: "agent_message"`, `content`, `from_session`, `from_session_name` |
| `backend_switched` | `role: "system"`, `content: "Backend switched: <from> → <to>"` |
| `backend_overlay_inactive`, `backend_overlay_undeclared` | `role: "system"`, `content` |
| `model_fallback_notice` | `role: "system"`, `content: "Served by <models> (configured <model>)"` |
| `child_report` | `role: "child_report"`, `content` (summary), `child_session_id`, `outcome`, `result_refs` |
| `run_header` | `role: "system"`, `kind: "run_header"`, `content`, `run_id`, `state`, `error`, `launched`, `launch_failed`, `task_spec_ref`, `launch_prompt_ref` |
| `run_delivery` | `role: "run_delivery"`, `content`, `task_state`, `completed`, `result_refs`, `raw_log_ref`, `events_ref`, `result_ref`, `repo_path`, `base_branch`, `branch_name`, `run_id` |
| `task_closed` | `role: "system"`, `content: "Task <outcome>: <summary>"` (right-trimmed) |
| `task_reopened` | `role: "system"`, `content: "Task reopened: <reason>"` (right-trimmed) |
| `task_input_acknowledged` | `role: "system"`, `content: "Task input(s) acknowledged as handled: <ids>[ — <note>]"` |
| `claude_account_login_required` | `role: "system"`, `kind: "claude_account_login_required"`, `content: "One account in the Claude pool needs a new login; see the usage panel."` |
| `scheduled_run_skipped` | `role: "system"`, `content: "Scheduled run of '<task>' skipped at <skipped_at>: <reason>"` |
| `slack_reply` | `role: "system"`, `content: "Posted to Slack: <content>"` |
| `discord_reply` | `role: "system"`, `content: "Posted to Discord: <content>"` |
| `web_reply` | `role: "web_reply"`, `content` |
| `web_reply_notice` | `role: "web_reply_notice"`, `content` |
| `web_status` | `role: "web_status"`, `content`, `web_status` |

Other event types produce no message. They still appear as raw WebSocket frames when they are not in `RAW_EVENTS_REPLACED_BY_DELTAS` (see [websocket.md](websocket.md#catch-up)).

The exact `content` text of `context_compacted`, `context_compact_failed`, `resume_context_dropped`, `backend_overlay_inactive` and `run_header` follows the functions in `src/runtime/message_aggregator.py:149-337`. The port must copy those strings.

### History projection

The history routes do not feed the events in file order. `_stable_history_projection` (`src/runtime/message_events.py:119`) moves queued user messages behind the run that was active when they arrived:

1. A run interval starts at an event whose type is `session_attached` (or missing) and that has a `session_id`. It ends at the next `master_done`.
2. Inside a completed interval, a real `user` event (not a tool-result carrier, and whose content does not start with `/`) is deferred.
3. The deferred events follow the closing `master_done`, in their original order. They keep their original `event_index`.

The live WebSocket path does not reorder. The bootstrap and events-page routes reorder.

### Message pages

The bootstrap route and the events page read a page from the message projection (`src/runtime/message_projection.py`) for a live session (`archive_offset` 0).

- `committed` is the list of committed messages in projection order. A message ordinal is a position in this list.
- `pending_draft` is the open assistant draft, or null.
- A turn start is ordinal 0, or the ordinal right after a `separator` message.

Page rules (`src/runtime/message_projection.py:159-195`):

- Snap: an ordinal moves back to the turn start at or below it. With no separator below it, it moves to 0.
- `tail(limit)`: `start = snap(max(0, total - limit))`. The page is `committed[start:]`. `has_more` is `start > 0`.
- `slice_before(before, limit)`: clamp `before` to `[0, total]`. Then `start = snap(max(0, before - limit))`. The page is `committed[start:before]`. `next_before` is `start`. `has_more` is `start > 0`.

Thus a page holds at least `limit` messages unless the history is shorter, and it can hold more.

A session with `archive_offset > 0` uses the legacy path. It reads raw events by index and feeds them through a fresh aggregator. Its cursors are event indexes, not message ordinals.
