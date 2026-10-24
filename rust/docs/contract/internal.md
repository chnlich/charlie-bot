# Internal API

The internal router (`src/runtime/api/internal.py`) is mounted at `/api/internal`. The `charliebot` CLI calls these routes from agent processes. The plan, reply and improve routes also live under `/api/internal`; they are in [plans.md](plans.md), [replies.md](replies.md) and [improve.md](improve.md).

## `GET /api/internal/version`

- Auth: access key.
- Handler: `src/runtime/api/internal.py:29`.
- Parameters: none.
- 200: `{"sha": "<short git sha>", "started_at": "<ISO time>"}`.

The server captures both values once at startup (`buildinfo.init_build_info`, called from the lifespan):

- `sha` is `git rev-parse --short HEAD` in the repository root, or `unknown` when git fails.
- `started_at` is the UTC start time in Python `isoformat()` form (`+00:00`).
- Before startup runs, the values are `unknown` and `""`.

Side effects: none. The CLI uses the route to detect a server that runs older code than the checkout.

## `POST /api/internal/delegate`

- Auth: access key + caller.
- Handler: `src/runtime/api/internal.py:266`.
- Dependency: `require_caller`.

Body (`DelegateRequest`, JSON, required):

| Field | Type | Required | Default |
|---|---|---|---|
| `session_id` | string | yes | |
| `description` | string | yes | |
| `base_branch` | string or null | no | null |
| `backend` | string or null | no | null |
| `repo_path` | string or null | no | null |
| `task_type` | `implement`, `quick-edit`, `script-run`, `verify` or `cold-read` | no | `implement` |
| `keep_worktree` | bool | no | false |
| `delegate_invocation` | object or null | no | null |
| `request_id` | string or null | no | null |
| `cold_read` | `{"page_path","genre","trigger"}` or null | no | null |

`delegate_invocation` keys: `task_type` (required), `repo_path`, `base_branch`, `task_spec_file`, `reviewer_context_file`, `keep_worktree` (required), `backend`. Unknown keys give 422.

### Checks, in order

1. Field rules (400):
   - `cold-read` without `cold_read`: `cold-read delegations require cold_read`.
   - another type with `cold_read`: `<type> delegations reject cold_read`.
   - `verify` or `cold-read` with `repo_path`: `<type> delegations are repo-less; omit repo_path`.
   - `verify` or `cold-read` with `base_branch`: `<type> delegations are repo-less; omit base_branch`.
   - another type with only one of `repo_path` and `base_branch`: `<type> delegations take repo_path and base_branch together; give both for a repo task, neither for a repo-less one`.
2. The parent session must exist: 404 `Session not found`.
3. For a type other than `verify` and `cold-read`, the takeoff gate: the open manager chain must hold a real user message. A refusal gives 403 with the gate message.
4. Backend resolution. `verify` and `cold-read` with no `backend` pick a verify backend. Others resolve the requested backend, or let the backend pool choose. A `ValueError` gives 400 with its message.
5. Child creation through `create_task` with the caller. Agent scope rules give 403. A closed parent or ancestor gives 409. See [sessions.md](sessions.md#post-apisessions).

### Responses

| Status | Body |
|---|---|
| 200 | `{"session_id": "<child id>", "parent_session_id": "<parent id>", "run_id": "<run id>", "thread_id": "<run id>", "description": "<description>"}` |
| 200 (child already closed) | the same keys with `run_id` and `thread_id` null |
| 503 | `{"detail":"task execution adapter is not installed"}` |

### Ids

- `request_id` defaults to a value derived from (session, task type, description). A repeated call with the same values returns the same child and run.
- The child id is `stable_task_id(parent, request_id)` (see [sessions.md](sessions.md#post-apisessions)).
- The run id is `uuid5(TASK_ID_NAMESPACE, "run:<child id>:<request_id>:work")`.

### Side effects

1. Create the worker child `sessions/<child id>/` with a `task_created` event. Its `task` is `{goal: description, repo_path, base_branch, task_type, keep_worktree, cold_read}`. Its `backend` is the resolved backend. A `cold-read` child is named `Cold read: <page file name>`.
2. When the backend pool chose the backend, set `backend_from_pool: true` on the child.
3. When the run record does not exist, register a `work` run with the resolved backend and model.
4. Launch the run through the execution adapter. This starts an agent process.
5. Append a `task_delegated` event to the parent session and broadcast it:

```json
{"type":"task_delegated","thread_id":"<run id>","description":"…","backend":"…","model":"…","child_session_id":"<child id>","delegate_invocation":{…}}
```

When the request has no `delegate_invocation`, the event carries one built from the request fields with null `task_spec_file` and `reviewer_context_file`.

6. Log `task_delegated_task_tree`.

## `POST /api/internal/schedule-trigger`

- Auth: access key.
- Handler: `src/runtime/api/internal.py:309`.

Body (`ScheduleTriggerRequest`, unknown keys give 422 `extra_forbidden`):

| Field | Type | Required | Validation |
|---|---|---|---|
| `session_id` | string | yes | |
| `delay_seconds` | int | yes | |
| `message` | string | yes | at most 200 characters, else 422 `string_too_long` |
| `watch_targets` | list of watch targets or null | no | discriminated by `kind`, see [shapes.md](shapes.md#pending-trigger) |

| Status | Case | `detail` |
|---|---|---|
| 404 | Session absent | `Session not found` |
| 400 | `watch_targets` is `[]` | `watch_targets must be non-empty when provided` |
| 400 | A `slurm_job` target with `job_id <= 0` | `watch_targets slurm job_id must be a positive integer` |
| 400 | A pid target with `pid <= 0` | `watch_targets pids must be positive integers` |
| 422 | The task is not open | `session <id> is archived (<reason>); trigger rejected` |
| 422 | A remote target probe failed | `verify-on-create failed for remote watch target(s): …` or `verify-on-create failed for remote SLURM host(s): …` |
| 422 | Five pending triggers already exist | `session <id> has <n> pending triggers (limit 5); watch several targets with one trigger: --watch A --watch B` |
| 400 | A local pid target on a host without `pidfd_open` | `pidfd_open unavailable: need Linux 5.3+ with kernel pidfd_open support` |
| 400 | A local SLURM target on a host without `sacct` | `sacct unavailable: cannot watch a SLURM job on a host without slurm` |

200:

```json
{"trigger_id": "<uuid>", "fire_at": "<isoformat, +00:00>", "watch_probe": {"<label>": "<observed state>"}}
```

`watch_probe` is present only when a remote SLURM target was probed.

Side effects:

- A remote pid target runs one `ssh` probe per target before the write. A remote SLURM target runs one probe per host.
- The server writes `sessions/<id>/triggers/<trigger id>.json` (the state contract documents it).
- It starts an in-process timer task. At `fire_at` (or when every watch target ends), the trigger fires: it appends a `scheduled_trigger` event and dispatches the session, which starts an agent run.
- It logs `trigger_scheduled` and `trigger_created`.

UNVERIFIED: the fire path (watch polling, the timeout rule, the event content) lives in `src/runtime/triggers.py` and was not traced here.

## `POST /api/internal/triggers/{session_id}/{trigger_id}/cancel`

- Auth: access key.
- Handler: `src/runtime/api/internal.py:357`.
- Body: none.

| Status | Case | Body |
|---|---|---|
| 200 | Cancelled, or the trigger was not pending | `{"ok": true}` |
| 404 | No trigger file | `{"detail":"Trigger not found"}` |

Side effects: for a pending trigger, the server sets `status` to `cancelled` in the trigger file, cancels the timer task and logs `trigger_cancelled`. A trigger that already fired or was cancelled stays unchanged.

## `POST /api/internal/session-message`

- Auth: access key + caller. The body `session_id` names the sender; an agent run token supplies the caller identity
  used to make retries idempotent.
- Handler: `src/runtime/api/internal.py:371`.

Body (`SessionMessageRequest`, unknown keys give 422):

| Field | Type | Required |
|---|---|---|
| `session_id` | string (the sender) | yes |
| `target_session_id` | string | yes |
| `content` | string | yes |

Checks, in order:

1. Weighted length: the character count plus one for each character in U+3000–U+303F, U+4E00–U+9FFF or U+FF00–U+FFEF. Above 2000 gives 422. The detail is these four lines joined by newline characters, with no trailing newline:
   - `Message is <n> weighted characters; the limit is 2000.`
   - `CJK characters, CJK punctuation, and fullwidth characters count 2.`
   - `Write the long content to a file. Send a short message with the conclusion,`
   - `the request, and the file path.`
2. Sender session absent: 404 `Session not found`.
3. Target session absent: 404 `Target session not found`.
4. Admission to the target (task mapping): a target that is not open gives 409 `task <id> is archived`. A worker target accepts an agent message only from its parent, and the parent must pass the takeoff gate (403).

200: `{"status": "accepted"}`.

For an agent caller, the server derives `request_id` from the active run id, verb `session send`, target id and
content. Repeating the same send from that run returns the accepted result without adding another target event;
reusing the derived id with a different event payload conflicts. An operator call has no run-derived request id.

Side effects:

1. Append an `agent_message` event to the target: `{"type":"agent_message","actor":"agent","content","from_session":<sender id>,"from_session_name":<sender name>,…}`. Agent sends also store the derived `request_id` and a stable event id. Broadcast it on the target's session channel.
2. Dispatch the target's pending inputs. An idle target starts an agent run.
3. Log `session_message_dispatched`. A refusal for length logs `session_message_refused`.
