# Chat

The chat router (`src/runtime/api/chat.py`) is mounted at `/api/chat`. It has three routes: upload a file, send a message, and stop the session's runs.

## `POST /api/chat/{session_id}/upload`

- Auth: access key.
- Handler: `src/runtime/api/chat.py:19`.
- Dependency: `require_session`.
- Body: `multipart/form-data` with one required file field `file`.

| Status | Case | Body |
|---|---|---|
| 200 | Saved | `{"filename": "<client file name>", "path": "<absolute path>", "size": <bytes>}` |
| 404 | Session absent | `{"detail":"Session not found"}` |
| 422 | No `file` field | validation list (`missing`, `["body","file"]`) |
| 500 | Write failed | `{"detail":"Failed to save uploaded file"}` |

- `filename` is the file name the client sent (it can be null when the client sent none).
- `path` is the resolved absolute path of the written file.

Side effects:

- The server creates `sessions/<session_id>/uploads/` when absent.
- It writes the file as `uploads/<base name of the client file name>`, or `uploads/upload` when the client sent no name. An existing file with that name is overwritten.
- It reads and writes in 1 MiB chunks.
- It logs `file_uploaded` (or `file_upload_failed`).

The browser then sends the returned objects in `uploaded_files` of the message route.

## `POST /api/chat/{session_id}/message`

- Auth: access key + caller.
- Handler: `src/runtime/api/chat.py:48`.
- Dependencies: `require_session`, then `require_caller`.

Body (`SendMessageRequest`, JSON, required):

| Field | Type | Required | Default |
|---|---|---|---|
| `content` | string | yes | |
| `uploaded_files` | list of `{"filename": string, "path": string, "size": int or null}` | no | `[]` |
| `input_mode` | string or null | no | null |

The server stores each uploaded-file object with `exclude_none`, so a null `size` is dropped.

### Input type

- An operator caller produces a `user` event with `actor: "user"`.
- An agent caller produces an `agent_message` event with `actor: "agent"` and `from_session` set to the token's session.

### Responses

| Status | Case | Body |
|---|---|---|
| 202 | Admitted | see below |
| 404 | Session absent | `{"detail":"Session not found"}` |
| 401 | Bad run token | see [auth.md](auth.md#caller-identity-require_caller) |
| 409 | The task is not open and the input is not a `user` message | `{"detail":"task <id> is archived"}` |
| 400 | `input_mode` on an agent message | `{"detail":"input_mode applies to a user message, not to agent_message"}` |
| 403 | An agent message to a worker node from a sender that is not its parent, or that fails the takeoff gate | `{"detail":"<message>"}` |
| 409 | Other task conflicts | `{"detail":{"message","blockers"}}` |

202 body:

```json
{"status": "accepted", "input_event_id": "<event id>", "launch": true}
```

- `launch` is true when this call started a run.
- `reason` is present when the dispatch did not launch. Its values:
  - `task is closed; input retained as history`
  - `run <run id> already consumes this node's inputs`
  - `execution adapters register in the next stage`
  - `queued run <run id> was not launched by this call`
  - `no pending inputs`
  - `another dispatch reserved this batch`

### Side effects

1. When the task is not open and the input is a `user` message, the server first reopens the task and each archived ancestor (one `task_reopened` event each, reason `user message`).
2. It appends the input event to `sessions/<id>/data/chat_events.jsonl`. The event has `id`, `type`, `timestamp`, `actor`, `source_session_id`, `content`, plus `uploaded_files`, `from_session` and `input_mode` when set.
3. For a `user` message, it moves `updated_at` of the node and its unarchived ancestors forward to the event time.
4. It broadcasts the event on the session channel: a `message` delta (the raw `user` event is in `RAW_EVENTS_REPLACED_BY_DELTAS`; a raw `agent_message` event is also sent).
5. It dispatches the pending inputs. When the node is idle, this starts an agent run (a `manager_turn` run for a manager, a `work` run for a worker). The agent's output then streams over the session WebSocket.
6. It logs `send_message` with `content_chars` and `uploaded_files_count`.

The response returns before the agent produces output.

UNVERIFIED: the run launch itself (process spawn, run record, `running_changed` frame) belongs to the execution adapter in `src/runtime/task_execution.py`; this file does not describe it.

## `POST /api/chat/{session_id}/cancel`

- Auth: access key.
- Handler: `src/runtime/api/chat.py:112`.
- Dependency: `require_session`.
- Body: none.

The handler requests a stop for every run of the session that has a `pid` and no terminal fact. Each stop uses the request id `chat-cancel:<run id>`, so a repeated press is idempotent.

| Status | Case | Body |
|---|---|---|
| 200 | At least one stop was requested | `{"ok": true}` |
| 404 | No stop was requested | `{"detail":"No active master agent"}` |
| 404 | Session absent | `{"detail":"Session not found"}` |
| 409 | A run's process identity no longer matches | `{"detail":{"message","blockers"}}` |

Side effects:

- For each stop: a durable stop-request fact, then an identity-checked signal to the run process.
- With no stop requested, the server appends an `assistant_error` event `{"type":"assistant_error","content":"No active master agent to cancel."}` and broadcasts it. The WebSocket clients get a `message` delta with `role: "system"` and `content: "Error: No active master agent to cancel."`, then the raw `assistant_error` event.
