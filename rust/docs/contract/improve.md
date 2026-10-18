# Improve loops

The improve router (`src/features/improve/api.py`) starts and stops an iterative improvement loop. The CLI calls it (`charliebot improve`, `charliebot improve-stop`). It is mounted at `/api/internal`.

Shared rules:

- Auth: access key. No route calls `require_caller`.
- Bodies are JSON. Unknown keys give 422 `extra_forbidden`.
- The session must exist: 404 `{"detail":"Session not found"}`.

Loop files live in `sessions/<session_id>/loops/` (the state contract documents them):

- `loops/active.lock`: the session's active-loop lock;
- `loops/<loop id>/state.json`: the loop state;
- `loops/<loop id>/goal.md`: the live goal;
- `loops/<loop id>/plan.md`: the optional live plan.

Loop ids are integers from 1, one more than the largest existing loop directory.

## `POST /api/internal/improve`

- Handler: `src/features/improve/api.py:59`.

Body (`ImproveRequest`):

| Field | Type | Required | Default |
|---|---|---|---|
| `session_id` | string | yes | |
| `repo_path` | string | yes | |
| `base_branch` | string | yes | |
| `backend` | string or null | no | null |
| `iterations` | int | no | 3 |
| `goal` | string | yes | |
| `plan` | string or null | no | null |
| `work_branch` | string or null | no | null |
| `merge_back` | bool | no | false |

Checks, in order:

1. Session exists (404).
2. Takeoff gate on the session: the open manager chain must hold a real user message. Else 403 with the gate message.
3. Backend: with no `backend` and a non-empty `backends.pool`, the loop takes a pool member per iteration. Otherwise the server resolves the backend; a `ValueError` gives 400 with its message.
4. Reserve the loop. When a loop is already running: 409 `Loop <id> is already running for this session. Use charliebot improve-stop first.` (or `An improve loop is already running for this session. Use charliebot improve-stop first.` when the id is unknown).
5. Create the worker child task. A failure marks the reserved loop `failed`, clears the lock, and maps the error through the task mapping (see [shapes.md](shapes.md#task-error-mapping)).

200:

```json
{
  "status": "started",
  "session_id": "…",
  "iterations": 3,
  "loop_id": 1,
  "goal_path": "<sessions dir>/<id>/loops/1/goal.md",
  "child_session_id": "…",
  "plan_path": "<sessions dir>/<id>/loops/1/plan.md"
}
```

`plan_path` is present only when the request has `plan`.

Side effects:

1. Write `loops/active.lock`, `loops/<loop id>/goal.md` (the goal text), `plan.md` when `plan` is set, and `state.json` with status `running`.
2. `work_branch` defaults to `improve/<unix seconds>`.
3. Create one worker child task under the session (`task_created` event, `task_tree_changed` frame).
4. Start a background task (`improve_sequence.run_improve_sequence`). It runs one iteration run per round on the child (run kind `iteration`), then delivers one final report to the manager session. Each iteration starts an agent process.
5. Log `improve_sequence_started`.

The response returns before the first iteration ends.

UNVERIFIED: the iteration loop (worktree, merge-back, report format) lives in `src/features/improve/improve_sequence.py` and was not traced here.

## `POST /api/internal/improve/stop`

- Handler: `src/features/improve/api.py:40`.
- Body: `{"session_id": string}`.

| Status | Case | Body |
|---|---|---|
| 200 | A loop was running | `{"status": "stopped", "session_id": "<id>"}` |
| 409 | No running loop | `{"detail":"No active improve loop in this session"}` |
| 404 | Session absent | `{"detail":"Session not found"}` |

Side effects: sets the running loop's `state.json` status to `stopped` and logs `improve_loop_stopped`. The loop ends after its current iteration. The route stops no process.
