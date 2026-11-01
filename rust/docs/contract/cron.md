# Cron tasks

The cron router (`src/features/cron/api.py`) edits the scheduled-task files and runs a task on demand. It is mounted at `/api/cron`.

Each task is one YAML file: `<home>/config.d/cron.d/<name>.yaml`. The file body has no `name` key; the file stem is the name. The state contract documents the file format.

Shared rules:

- Auth: access key.
- A task name must match `^[A-Za-z0-9][A-Za-z0-9._-]*$`. Else 400 `{"detail":"invalid cron name: '<name>'"}`.
- A backend id must be a configured option. Else 400 with the config error text (`config.require_backend_option`).
- Missing task file: 404 `{"detail":"Task \"<name>\" not found"}` (the name in double quotes).

## Task JSON

`GET /api/cron/tasks` returns each valid task as the `ScheduledTaskConfig` dump, without `prompt` and without `steps[].prompt`. Keys in order:

| Key | Type | Default |
|---|---|---|
| `name` | string | |
| `cron` | string (cron expression) | |
| `prompt_file` | string or null | null |
| `repo` | string or null | null |
| `backend` | string or null | null |
| `timezone` | string | `America/Los_Angeles` |
| `enabled` | bool | true |
| `project` | string or null | null |
| `allow_failure` | bool | false |
| `session_id` | string or null | null |
| `mode` | `"master"`, `"worker"` or null | null |
| `handler` | string or null | null |
| `steps` | list of steps or null | null |

A step: `{"name", "prompt_file", "backend", "distinct_backend_from", …}` without `prompt`. UNVERIFIED: the full step key list is in `StepConfig` (`src/features/cron/config.py`); this file lists the keys seen in its first lines.

## `GET /api/cron/tasks`

- Handler: `src/features/cron/api.py:162`.
- Parameters: none.
- 200: a list. First the valid tasks sorted by name. Then one entry per broken file:

```json
{"name": "…", "error": "…", "broken": true, "path": "<absolute path>", "enabled": true}
```

`enabled` is the raw file value, or null when the body could not be parsed. The route never fails because of a broken file. It reads the loader's cached snapshot.

## `PUT /api/cron/tasks/{name}`

- Handler: `src/features/cron/api.py:256`.
- Body (`TaskUpdate`, all optional, unknown keys ignored):

| Field | Type | Effect when present |
|---|---|---|
| `cron` | string | Set when not null. |
| `prompt_file` | string | Set when not null. |
| `repo` | string | Set when not null; `""` stores null. |
| `backend` | string or null | Present and truthy: set. Present and null or `""`: remove the key. |
| `timezone` | string | Set when not null. |
| `enabled` | bool | Set when not null. |
| `project` | string | Set when not null; `""` stores null. |
| `allow_failure` | bool | Set when not null. |
| `session_id` | string or null | Present and truthy: set. Present and null or `""`: remove the key. |

Checks, in order:

1. Name rule (400).
2. Task file exists (404).
3. When `backend` is present: backend id rule (400).
4. The current file must load: else 409 with the loader error.
5. The updated body must validate through the loader: else 409 with the error. Examples: `task must have exactly one of 'prompt', 'prompt_file', 'steps', or 'handler'`; `'mode' requires 'session_id' (mode selects how a bound task fires)`.
6. When `backend` is present and the task is bound to a node (`session_id`) whose backend differs from the new effective backend:
   - when the node has work in flight: 409 `scheduled task '<name>' backend switch from '<old>' to '<new>' is blocked because node '<id>' has running work; retry when it is idle`. Nothing is written.
   - else the node's backend switches in place (sidebar `backend_switched` frame).
7. When `enabled` is true and the bound node is not open, the server restores the node's archived chain (`task_reopened` events, reason `cron enable`).
8. Write the YAML file.

200: the updated file body as a JSON object (no `name`; keys as stored).

## `POST /api/cron/tasks`

- Handler: `src/features/cron/api.py:269`.
- Body (`TaskCreate`, unknown keys ignored):

| Field | Type | Required | Default |
|---|---|---|---|
| `name` | string | yes | |
| `cron` | string | yes | |
| `prompt_file` | string or null | no | null |
| `repo` | string or null | no | null |
| `backend` | string or null | no | null |
| `timezone` | string | no | `America/Los_Angeles` |
| `enabled` | bool | no | true |
| `project` | string or null | no | null |
| `allow_failure` | bool | no | false |
| `session_id` | string or null | no | null |
| `mode` | `"master"`, `"worker"` or null | no | null |

Checks, in order: name rule (400); backend id rule (400); the file must not exist (409 `Task "<name>" already exists`); the body must validate through the loader (409 with the error).

The stored body is the request without `name`, without an empty `backend`, and without null values (except `cron` and `enabled`, which stay).

200: `{"name": "<name>", <stored body keys>}`.

Side effects: creates `config.d/cron.d/` when absent and writes `<name>.yaml`.

## Rounds in flight

A round is the worker session that the scheduler creates for one firing of a task that runs a worker, steps included. A `master` task and a handler task create no round. A task has a round in flight while one of its rounds has a run that is running or queued, or while the round's steps chain has a next step to start. A withheld run is not in flight. The judgment reads the persisted runs, so it holds after a server restart.

- A due tick for a task with a round in flight records `scheduled_run_skipped` with reason `previous round still running (<session id>)`, sets `last_run_status` to `skipped`, and sets `last_scheduled_run` to the tick time. The tick starts no run.
- A manual fire of a task with a round in flight starts no run and answers 409 (see `POST /api/cron/tasks/{name}/run`).

## `POST /api/cron/tasks/{name}/run`

- Handler: `src/features/cron/api.py:300`.
- Body: none.
- The name rule does not apply on this route.

| Status | Case | Body |
|---|---|---|
| 503 | The scheduler service is not running (`app.state.scheduler` unset) | `{"detail":"Scheduler not available"}` |
| 404 | No task with that name | `{"detail":"No scheduled task named '<name>'"}` |
| 409 | A round of the task is in flight (see Rounds in flight) | `{"detail":"scheduled task '<name>' has a round still running in session '<session id>'"}` |
| 202 | Started | `{"type":"task_triggered","task":"<name>","session_id":"<node id>","thread_id":<string or null>}` |

Side effects: the scheduler runs the task once through the same path as a cron fire (`scheduler.run_task_now`), unless a round of the task is in flight. An agent task starts an agent run on its node and delivers its completion report to the node. A handler task runs its handler and records a `handler_result` event. The route writes no chat event of its own.

## `DELETE /api/cron/tasks/{name}`

- Handler: `src/features/cron/api.py:328`.
- Checks: name rule (400); task file exists (404).
- 200: `{"ok": true}`.
- Side effects: deletes the YAML file. The bound node stays unchanged.
