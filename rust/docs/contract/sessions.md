# Sessions

The sessions router (`src/runtime/api/sessions.py`) is mounted at `/api/sessions`. The chat-threads route (`src/features/chat_threads/api.py:15`) shares the prefix and registers before it.

Shared shapes (session JSON forms, run rows, chat messages, error mapping) are in [shapes.md](shapes.md). Auth values are in [auth.md](auth.md#auth-values-in-routestsv).

Route order matters. The fixed paths (`/archived`, `/starred`, `/groups`, `/status`, `/tree`, `/search`, `/chat-threads`) register before `/{session_id}`.

Every route in this file returns JSON unless it says otherwise. A missing session in `deps.require_session` gives 404 `{"detail":"Session not found"}`.

## Lists

### `GET /api/sessions/`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:389`.
- Parameters: none.
- A request to `/api/sessions` (no slash) gets 307 to `http://<host>/api/sessions/`.

200: a JSON list of listing rows (see [shapes.md](shapes.md#listing-rows)) with the derived sidebar state overlaid.

Row set:

- active sessions (`status` `active`) that are not scheduled;
- minus the sessions in a sequence subtree (scheduled-task firings);
- minus the sessions in any sidebar view subtree (chat-platform thread sessions and their descendants).

Order: newest first, as `list_sessions_readonly` returns them. UNVERIFIED: the sort key (`updated_at` descending) comes from the listing block; the probe had one session.

The route pre-compresses: with gzip accepted, the body is gzip with `Content-Encoding: gzip` and `Vary: Accept-Encoding` at any size. See [middleware.md](middleware.md#pre-compressed-routes).

Side effects: none on disk. The server caches the rendered body in memory.

### `GET /api/sessions/chat-threads`

- Auth: access key.
- Handler: `src/features/chat_threads/api.py:15`.
- Parameters: none.

200: the same row form as `GET /api/sessions/`. The row set is the active, non-scheduled sessions that belong to the `threads` view subtree: sessions with a `slack_origin` or `discord_origin`, plus every descendant through `task_parent_id`. Pre-compressed like `GET /api/sessions/`.

### `GET /api/sessions/archived`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:473`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `group` | query | string | no | null |
| `limit` | query | int | no | 100 |
| `before` | query | string (ISO datetime with offset) | no | null |
| `before_id` | query | string | no | null |

- `group`: null means every archived session. `""` means the sessions with no group. A name means that group.
- `limit` is clamped to 1..500. No validation error.
- `before` and `before_id` form one cursor. Rows strictly after (`before`, `before_id`) in (`updated_at`, `id`) descending order form the page.

Responses:

| Status | Case | Body |
|---|---|---|
| 200 | | see below |
| 422 | Only one of `before` and `before_id` | `{"detail":"before and before_id form one cursor: pass both or neither"}` |
| 422 | `before` has no time zone | `{"detail":"before must be a timezone-aware ISO timestamp"}` |
| 422 | `before` is not an ISO datetime | `{"detail":"<Python fromisoformat message>"}` |
| 422 | `limit` is not an integer | validation list |

200 body:

```json
{
  "sessions": [ {listing row…, "context_only": true?} ],
  "has_more": false,
  "next_before": null,
  "next_before_id": null,
  "groups": [ {"group": "Synthetic group", "total": 2}, {"group": null, "total": 1} ]
}
```

- `sessions`: the archived page rows, plus their unarchived ancestors as context rows with `"context_only": true`. Sorted by (`updated_at`, `id`) descending. Rows in a sequence subtree are left out. `task.goal` is removed.
- `has_more`: more archived rows exist after the page.
- `next_before`: the last page row's `updated_at` as Python `isoformat()` (`+00:00`) when `has_more`, else null.
- `next_before_id`: that row's id when `has_more`, else null.
- `groups`: counts over the whole archived set (not the filter). Named groups in alphabetical order, then the `null` bucket last when it is not empty.

### `GET /api/sessions/starred`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:515`.
- Parameters: none.
- 200: a list of listing rows for the starred sessions, archived ones included, newest first. `task.goal` is removed. No sidebar overlay.

### `GET /api/sessions/groups`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:537`.
- 200: a sorted list of the distinct group names over all sessions, for example `["Synthetic group"]`.

### `GET /api/sessions/status`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:588`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `ids` | query | string, comma-separated session ids | yes | |
| `force` | query | bool | no | false |

The server splits `ids` on `,`, strips each part, drops empty parts and removes duplicates (first occurrence wins).

| Status | Case | Body |
|---|---|---|
| 200 | | object keyed by session id |
| 422 | `ids` missing | validation list (`missing`, `["query","ids"]`) |
| 422 | `ids` has no non-empty part | `{"detail":"ids must name at least one session"}` |

200 body, one entry per known requested session (unknown ids are left out; an all-unknown request gives `{}`):

```json
{
  "00000000-0000-4000-8000-000000000001": {
    "has_unread": false,
    "has_running_tasks": false,
    "thinking_since": null,
    "has_pending_trigger": false,
    "pending_trigger_count": 0,
    "next_trigger_at": null,
    "has_pending_plan_approval": false,
    "work_state": "idle"
  }
}
```

- `thinking_since` and `next_trigger_at` use Python `isoformat()` (`+00:00`).
- `work_state` is present only when the state has a verdict.
- Pre-compressed.

Side effects:

- The server memoizes the body per (ids, sidebar generation). `force=true` skips the memo and probes every requested session.
- Every 10th poll schedules a background self-heal sweep of the sidebar state.

### `GET /api/sessions/tree`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:843`.

| Name | In | Type | Required | Default | Validation |
|---|---|---|---|---|---|
| `parent_id` | query | string | no | null | `""` means the roots |
| `include_archived` | query | bool | no | false | |
| `limit` | query | int | no | 100 | 1..500, else 422 |
| `cursor` | query | string | no | null | |

200:

```json
{"items":[{"id":"…","name":"…","profile":"manager","task_parent_id":null,"task_state":"open","work_state":"idle","archived":false,"child_count":0,"open_descendant_count":0,"running_descendant_count":0,"has_unread":false}],"next_cursor":null,"tree_revision":"<64 hex>"}
```

- `items`: the children of `parent_id`, sorted by (`created_at`, `id`) ascending.
- Without `include_archived`, archived rows are left out, except an archived row that is running or has running work below it.
- `next_cursor`: unpadded base64url of `{"r": revision, "a": created_at ISO, "i": id}` for the last item when more items exist, else null.

Errors (task mapping):

| Status | Case | `detail` |
|---|---|---|
| 404 | `parent_id` unknown | `task <id> not found` |
| 400 | Malformed cursor | `malformed tree page cursor: '<cursor>'` |
| 409 | The tree changed since the cursor | `{"message":"task tree changed during pagination; refresh and re-paginate","blockers":["task tree changed during pagination; refresh and re-paginate"]}` |

### `GET /api/sessions/search`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:866`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `q` | query | string | no | `""` |

- When `q` is empty or only white space, the result is every active session (scheduled ones included).
- Otherwise the result is the full-text search over session names and chat content (`session_search.search_sessions_readonly`, which caps the result count).
- 200: a list of row-form sessions (`task.goal` removed). The derived values `thinking_since`, `has_running_tasks`, `has_pending_trigger`, `pending_trigger_count`, `next_trigger_at` and `work_state` are overlaid in their model positions. There is no schedule join.
- Pre-compressed.

UNVERIFIED: the search ranking and the result cap live in `src/runtime/session_search.py` and were not traced.

## One session: reads

### `GET /api/sessions/{session_id}`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1367`.
- Dependency: `require_session`.
- 200: the detail form (see [shapes.md](shapes.md#detail-form)).
- Errors: 404 `Session not found`; task mapping errors from `session_detail`.

### `GET /api/sessions/{session_id}/bootstrap`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1019`.
- Dependency: `require_session`.
- 200 (pre-compressed):

```json
{
  "session": {row form},
  "messages": [message, …],
  "pending_draft": null,
  "event_count": 1,
  "oldest_message_ordinal": 0,
  "has_more": false,
  "active_backend": "claude-opus",
  "active_backend_type": "cc-claude",
  "switchable_backends": ["claude-opus"],
  "worker_summary": {summary fields}
}
```

- `messages`: the tail page of 40 messages (see [shapes.md](shapes.md#message-pages)). For a worker session, the page comes from the Runs transcript.
- Tool rows in `messages` are trimmed again by `tool_preview`.
- `event_count`: the total chat event count, archive offset included.
- `oldest_message_ordinal`: the page start ordinal.
- `active_backend`: `run_backend` or `backend` of the session, else the first configured option id, else `claude`.
- `active_backend_type`: the type of that option, or `""` when it is not configured.
- `switchable_backends`: every configured option id in config order. A session bound to a dedicated-backend sequence lists only the ids in the same continuation domain. An unconfigured active backend gives `[]`.
- `worker_summary` appears for a worker session and is the summary object also returned by the transcript route.

Side effects: none. The route does not mark the session read.

### `GET /api/sessions/{session_id}/usage`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1052`.
- Dependency: `require_session`.
- 200:

```json
{
  "session": {row form},
  "usage": {"context_tokens": null, "context_full": null, "context_compact_at": null, "total_cost_usd": null, "model": ""},
  "active_backend": "…",
  "active_backend_type": "…",
  "switchable_backends": ["…"]
}
```

- `usage` is the usage object or null. The state contract and `src/runtime/session_usage.py` define how the values are derived.
  `context_full` uses integer `limit.input`, or integer `limit.context` with integer `limit.output`.
  A missing `limit.output` defaults to zero when `limit.input` is an integer.
  `context_compact_at` subtracts the registered compact reserve, bounded by `limit.output`, from that full value.
- A worker session adds `active_run_id` (string or null) after `usage`.

### `GET /api/sessions/{session_id}/events`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1081`.
- Dependency: `require_session` (it runs before the query check).

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `before` | query | int | yes | |
| `limit` | query | int | no | 40, clamped to 1..200 |

200: `{"messages": [...], "has_more": bool, "next_before": int}`.

- Live session: `before` is a message ordinal. The page follows `slice_before` (see [shapes.md](shapes.md#message-pages)). With gzip accepted, the body is pre-compressed.
- Worker session: the same rules over the Runs transcript projection. Not pre-compressed.
- Archived-offset session (`archive_offset > 0`): `before` is an event index. `start = max(0, before - limit)`. The server reads events `[start, before)`, feeds them to a fresh aggregator and flushes the draft. `next_before` is `start`. Not pre-compressed.

Errors: 404 `Session not found`; 422 when `before` is missing or not an int.

### `GET /api/sessions/{session_id}/transcript`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1143`.
- Dependency: `require_session`.

| Name | In | Type | Required | Default | Validation |
|---|---|---|---|---|---|
| `after` | query | int | no | 0 | ≥ 0, else 422 |
| `revision` | query | string | no | `""` | |

| Status | Case | Body |
|---|---|---|
| 400 | The session profile is not `worker` | `{"detail":"session <id> has no worker transcript"}` |
| 200 | | see below |

```json
{"messages":[…],"total":12,"pending_draft":null,"revision":"…","reset":false,"active_run_id":"…","summary":{…},"thinking_since":null}
```

- `reset` is true when `revision` is empty or differs from the current revision.
- With `reset`, `messages` is the whole committed list. Otherwise it is `committed[after:]`.
- `summary` contains `task_state`, `closed_at`, `landed`, `delivery`, `runs`, `active`, `closing`, `cause`, `stop` and `links`. `runs` has one row per Run; `active` describes the newest running Run; `closing` has the newest work and review text; the remaining fields read the current close, newest failure, stop request and evidence refs.
- `thinking_since` uses `isoformat()` (`+00:00`).

### `GET /api/sessions/{session_id}/pending-triggers`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:997`.
- Dependency: `require_session`.
- 200: a list of pending triggers (see [shapes.md](shapes.md#pending-trigger)), `fire_at` ascending. Fired and cancelled triggers are left out.

### `GET /api/sessions/{session_id}/events.jsonl`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1525`.
- No `require_session`. The route checks the file only.

| Status | Case | Body |
|---|---|---|
| 404 | `sessions/<id>/data/chat_events.jsonl` does not exist | `{"detail":"Events file not found"}` |
| 200, gzip accepted | | the file bytes gzipped at level 1, `content-type: application/x-ndjson`, `Content-Encoding: gzip`, `Vary: Accept-Encoding` |
| 200, no gzip | | a `FileResponse` of the file, `content-type: application/x-ndjson`, with `etag`, `last-modified`, `accept-ranges` |

The server memoizes the gzip form per file (mtime, size).

### `GET /api/sessions/{session_id}/runs`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1547`.
- Dependency: `require_session`.

| Name | In | Type | Required | Default | Validation |
|---|---|---|---|---|---|
| `limit` | query | int | no | 100 | 1..500, else 422 |
| `cursor` | query | string | no | null | |
| `order` | query | string | no | `asc` | `asc` or `desc` |

| Status | Case | Body |
|---|---|---|
| 200 | | `{"items":[run row…],"next_cursor":null}` |
| 400 | Bad `order` | `{"detail":"unknown runs order: '<order>' (use 'asc' or 'desc')"}` |
| 400 | Bad cursor, or a cursor from the other order | `{"detail":"<message>"}` |

- `asc` orders runs by (`started_at`, `id`), queued runs first.
- `desc` reverses it. A queued run never opens a `desc` page while any run has started.
- Run rows: see [shapes.md](shapes.md#run-rows).

### `GET /api/sessions/{session_id}/effective-prompt`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1593`.
- Dependency: `require_session`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `kind` | query | string | no | `manager_turn` for a manager, `work` for a worker |

| Status | Case | `detail` |
|---|---|---|
| 400 | `kind` not a run kind | `unknown run kind: '<kind>'` |
| 400 | The session backend is not configured | `session backend '<id>' is not configured; update the task backend before previewing` |
| 400 | No backend is configured | `spawner_backends.EMPTY_BACKENDS_OPTIONS_REFUSAL` |
| 404 | The task metadata is missing | `Session not found` |
| 500 | Prompt assembly failed | the error message |

200:

```json
{
  "session_id": "…",
  "kind": "manager_turn",
  "mode": "preview",
  "overlay": {"declared": true, "error": null},
  "blocks": [{"sources": [{"scope": "…", "source_ref": "…", "source_session_id": "…"}], "body_ref": "…", "delivery": "…", "text": "…"}],
  "prompt_hash": "…",
  "char_count": 1234
}
```

`overlay.error` is the exception class name of an overlay read failure, or null. The route is read-only. It starts no process.

### `GET /api/sessions/{session_id}/runs/{run_id}/context`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1653`.
- Dependency: `require_session`.

| Status | Case | `detail` |
|---|---|---|
| 404 | No run record | `run <run_id> not found in task <session_id>` |
| 500 | The stored snapshot file is missing | `stored prompt snapshot missing at <ref>` |
| 500 | The snapshot file is unreadable | `stored prompt snapshot unreadable at <ref>: <error>` |
| 500 | The snapshot is malformed | the error message |

200:

```json
{
  "session_id": "…",
  "run_id": "…",
  "kind": "work",
  "mode": "historical",
  "snapshot": {"blocks": […], "prompt_hash": "…", "char_count": 0},
  "task_spec": {"ref": "…", "sha256": "…"},
  "logs": {"raw_log_ref": "…", "events_ref": "…", "result_ref": "…"}
}
```

- `snapshot` is set when `prompt_snapshot_ref` names the snapshot file.
- `task_spec` is null when the run has no `task_spec_ref`.

### `GET /api/sessions/{session_id}/task-inputs/pending`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1730`.
- Dependency: `require_session`.
- 200: `{"items":[{"id","type","timestamp","actor","source_session_id","from_session_name","text"}]}`. `timestamp` is a pydantic datetime (`Z`). `text` is the event `content`, else `summary`, else `""`.
- Errors: task mapping (400, 404).

## One session: writes

### `POST /api/sessions/`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:409`.
- Body: `CreateSessionRequest` (JSON object, required).

| Field | Type | Required |
|---|---|---|
| `name` | string or null | no |
| `backend` | string or null | no |
| `session_id` | string or null | no |
| `request_id` | string or null | no |
| `task_parent_id` | string or null | no |
| `profile` | `"manager"`, `"worker"` or null | no |
| `task` | TaskSpec or null | no |
| `group` | string or null | no |
| registered slot keys | slot types | no |

A key that is not a field and not a registered slot key gives 422 with `type` `value_error` and `msg` `Value error, unknown keys ['<key>', …]`.

Two paths:

1. Task create. Any of `request_id`, `task_parent_id`, `profile` or `task` is set.
   - `request_id` missing: 400 `request_id is required for task creation`.
   - `backend` set and unknown: 400 with the unknown-backend text (see [shapes.md](shapes.md#task-error-mapping)).
   - The node id is `session_id` when given, else `uuid5(TASK_ID_NAMESPACE, "task:<parent or 'root'>:<request_id>")`.
   - A repeated request returns the original node.
   - An agent caller may create only under its own open manager task. Otherwise 403 `an agent may only create a task directly under its own open manager task`. An agent worker create also needs the takeoff gate (403 on refusal), except for `verify` and `cold-read` task types.
   - An unknown parent gives 404 `task <id> not found`. A worker parent gives 400 `parent task <id> is not a manager`. A parent that is not open, or a closed ancestor, gives 409.
2. Legacy create. None of those four fields is set.
   - An agent caller gets 403 `an agent may only create a task directly under its own open manager task`.
   - `backend` is resolved: an exact option id; or a family prefix (for example a codex id) that maps to the first option of that type; or, when absent, the first configured option. An unknown id gives 400.
   - The server creates a manager root with `request_id` `operator-create-<uuid4>`.

200: the model form of the new session.

Side effects:

- The server writes `sessions/<id>/` (metadata and a `task_created` event) by an atomic directory rename.
- A node created with no name gets the first line of `task.goal`, or `Session <N>` from the counter file `sessions/.counter`.
- A sidebar `task_tree_changed` frame with `fact_type: "task_created"`.
- No agent starts.

### `POST /api/sessions/groups/rename`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:543`.
- Body: `{"old_name": string, "new_name": string}`, both required.
- 200: `{"updated": <number of sessions changed>}`.
- Side effects: rewrites the `group` field of each session in the old group.

### `POST /api/sessions/groups/delete`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:553`.
- Body: `{"group": string}`, required.
- 200: `{"updated": <count>}`.
- Side effects: sets `group` to null on each session in the group.

### `POST /api/sessions/{session_id}/read`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1035`.
- Dependency: `require_session`.
- Body: none.
- 200: `{"session_id": "<id>", "has_unread": false}`.
- Side effects: clears `has_unread` in the metadata. When the flag changed, a sidebar `unread_changed` frame with `has_unread: false`.

### `POST /api/sessions/{session_id}/backend`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1301`.
- Dependency: `require_session`.
- Body: `{"backend": string}`, required.

| Status | Case | Body |
|---|---|---|
| 400 | `backend` not a configured id | unknown-backend text |
| 400 | The session is bound to a dedicated-backend sequence and the target is in another continuation domain | the binding's lock text |
| 200 | `backend` equals the current effective backend | the unchanged session (model form); no side effect |
| 200 | Switched | the updated session (model form) |

Side effects of a switch:

1. When the session holds a native session id with no `native_backend`, the server first records `native_backend` as the old backend.
2. It writes the new `backend` to the metadata, and sends a sidebar `backend_switched` frame.
3. It appends a `backend_switched` chat event `{"type","from","to","previous_native_backend","previous_native_session_id"}` and broadcasts it.

### `POST /api/sessions/{session_id}/archive`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1380`.
- Dependency: `require_session`, then `require_caller`.
- Body: `{"request_id": string, "summary": string = "", "result_refs": [string], "run_ids": [string]}`. Unknown keys give 422.

| Status | Case | Body |
|---|---|---|
| 200 | An operator archives the open subtree | `{"archived": [<ids archived by this call>]}`. An already archived target gives `[]`. |
| 202 | An agent manager archives its own task through its active run | UNVERIFIED: the pending body. The close waits for the run to finish. |
| 409 | Blockers | `{"detail":{"message","blockers"}}`. An unfinished run in the subtree gives 409 before any write. |
| 400, 403, 404 | Task mapping | An agent may archive only its own manager task, through its own active run. |

Side effects: an operator archive writes one `task_closed` event with outcome `archived` for each open node of the subtree. The target's fact names its parent as the report target, so the parent receives a child report. Each descendant's fact names the target in `archived_with`. A repeated `request_id` replays the original outcome. UNVERIFIED: the trigger cancel and the exact per-node fact set come from `TaskCompletionManager.archive_subtree` and were not traced.

### `DELETE /api/sessions/{session_id}/permanent`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1406`.

| Status | Case | Body |
|---|---|---|
| 204 | Deleted | empty |
| 404 | Nothing deleted | `{"detail":"Session not found"}` |
| 400, 403, 404, 409 | Task mapping | `delete_permanently` refuses a task that has content, children or references. |

Side effects: removes the session directory under the control lock.

### `POST /api/sessions/{session_id}/unarchive`

- Auth: access key + caller, operator only.
- Handler: `src/runtime/api/sessions.py:1423`.
- Dependency: `require_session`, then `require_caller`.

| Status | Case | Body |
|---|---|---|
| 403 | Agent caller | `{"detail":"unarchiving a task requires operator credentials"}` |
| 200 | | `{"restored": [<ids>]}`, topmost ancestor first |
| 400, 404, 409 | Task mapping | |

Side effects: one `task_reopened` event (reason `sidebar unarchive`) for the target and each archived ancestor. No agent starts.

### `POST /api/sessions/{session_id}/star` and `POST /api/sessions/{session_id}/unstar`

- Auth: access key.
- Handlers: `src/runtime/api/sessions.py:1402` (star), `src/runtime/api/sessions.py:1409` (unstar).
- No `require_session`; the handler gives 404 `Session not found` when the session is absent.
- Body: none.
- 200: the model form with `starred` true or false.
- Side effects: writes `starred` and `updated_at`.

### `POST /api/sessions/{session_id}/rounds/{round_id}/rate`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1462`.
- Dependency: `require_session`.
- Body: `{"rating": "thumbs_up" | "thumbs_down" | null}`. The key is required. Another value gives 422 `literal_error`.
- 200: the model form.
- Side effects: sets or removes `round_ratings[<round_id>]`, sets `updated_at`, saves the metadata, logs `round_rated`.

### `PATCH /api/sessions/{session_id}`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1480`.
- Body: `PatchSessionTaskRequest`. Unknown keys give 422 `extra_forbidden`.

| Field | Type |
|---|---|
| `name` | string or null |
| `task_parent_id` | string or null |
| `profile` | `"manager"`, `"worker"` or null |
| `task` | TaskSpec or null |
| `subtree_prompt` | string or null |
| `node_prompt` | string or null |

A field that is absent means no change. An explicit null clears where that is legal.

Name-only body (only `name` set, or an empty object):

| Status | Case | Body |
|---|---|---|
| 400 | `name` empty, null or absent | `{"detail":"rename requires a non-empty name"}` |
| 404 | Session absent | `Session not found` |
| 200 | | the detail form built from the model alone (see [shapes.md](shapes.md#detail-form)) |

Side effects: writes `name`, sends a tree notification with `fact_type: "task_updated"`.

Any other body goes to `task_mgr.patch_task` with the caller. 200 is the full detail form. Errors follow the task mapping. A prompt change stores the new body and records a `prompt_changed` fact.

### `POST /api/sessions/{session_id}/group`

- Auth: access key.
- Handler: `src/runtime/api/sessions.py:1508`.
- Body: `{"group": string | null}` (the key may be absent; then null).
- 200: the model form. 404 `Session not found` when absent.
- Side effects: writes `group` verbatim, sends a sidebar `session_group_changed` frame.

### `POST /api/sessions/{session_id}/task-inputs/acknowledge`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1760`.
- Dependency: `require_session`.
- Body: `{"request_id": string, "input_ids": [string], "note": string = ""}`. Unknown keys give 422.
- 200: `{"session_id","acknowledged_event_id","input_ids","already_acknowledged"}`.
- Errors: task mapping. Only an operator may acknowledge (403). An unknown or claimed id gives 409.
- Side effects: appends a `task_input_acknowledged` event. A repeated `request_id` replays the result.

### `POST /api/sessions/{session_id}/cancel`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1791`.
- Dependency: `require_session`.
- Body: `{"request_id": string, "reason": string}`, both required.
- 200: the detail form plus `stopped_runs`, one row per stopped Run: `{"run_id": string, "outcome": "interrupted" | "never_launched" | null}`.
- Errors: an agent may cancel only a direct child of its own task (403). A manager task keeps its active-run and open-child blockers (409).
- Side effects: a worker target stops its non-terminal Runs and ends the delegation. A manager target keeps its existing blocker behavior.

## Agent orchestration routes

### `POST /api/sessions/{session_id}/fork`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1237`.
- Dependency: `require_session`, then `require_caller`.
- Body: optional `{"event_index": int | null, "backend": string | null}`.

| Status | Case | Body |
|---|---|---|
| 200 | | the model form of the new session |
| 400 | Unknown backend | unknown-backend text |
| 400 | Fork refused (`ValueError`) | the message |
| 404 | Source files missing | `Session not found` |
| 409 | Task conflict | task mapping |

Side effects:

1. A new session that copies the source chat history up to `event_index` (all when null). Backend: the request backend, else the source backend.
2. The bootstrap prompt (`FORK_BOOTSTRAP_OPENER`, the history note, the orientation directive and the tree note) is admitted as the first input: a `user` event for an operator caller, an `agent_message` event for an agent caller.
3. The server dispatches the pending input. This starts an agent run.

### `POST /api/sessions/{session_id}/elone`

- Auth: access key + caller.
- Handler: `src/runtime/api/sessions.py:1272`.
- Body: `{"event_index": int, "backend": string | null}`. `event_index` is required.
- Responses and side effects: as fork. The new session starts fresh and its bootstrap prompt (`ELONE_BOOTSTRAP_OPENER`) asks the agent to read the parent's log up to the takeover point.

### `POST /api/sessions/{session_id}/retry`

- Auth: access key + caller, operator only.
- Handler: `src/runtime/api/sessions.py:1702`.
- Dependency: `require_session`.
- Body: `{"request_id": string, "run_id": string}`. Unknown keys give 422.

| Status | Case | Body |
|---|---|---|
| 403 | Agent caller | `{"detail":"retrying a run requires operator credentials"}` |
| 200 | | `{"session_id","run_id"}` (the new retry run) |
| 400, 404, 409 | Task mapping | |

Side effects: creates a queued retry run with a stable id per (session, `request_id`), then dispatches it. Dispatch starts an agent run.

### `POST /api/sessions/{session_id}/runs/{run_id}/cancel`

- Auth: access key + caller, own run.
- Handler: `src/runtime/api/sessions.py:1828`.
- No `require_session`.
- Body: `{"request_id": string}`. Unknown keys give 422.

| Status | Case | Body |
|---|---|---|
| 403 | Agent caller whose token names another session or run | `{"detail":"an agent may only stop its own bound run"}` |
| 404 | No run record | the run miss text |
| 409 | The recorded process identity no longer matches | `{"detail":{"message","blockers"}}` |
| 200 | | `{"run_id","stop_requested":bool,"outcome":"success"|"failed"|"interrupted"|null}` |

Side effects: writes a durable stop-request fact first, then signals the run process after an identity check and waits for its exit.
