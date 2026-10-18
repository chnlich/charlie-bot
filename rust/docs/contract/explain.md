# Explain

The explain button on a chat divider asks a chosen backend to explain the round that the divider closes. The routes live in `src/features/explain/api.py`, mounted at `/api/sessions`. The logic lives in `src/features/explain/explain.py`.

Shared rules:

- Auth: access key.
- Dependency: `require_session` (404 `{"detail":"Session not found"}`).
- A divider is named by its event index, `upto`.
- Responses use `FastJsonResponse` (orjson).

## Explain file

The server keeps one file per session: `sessions/<session_id>/data/explain_results.json`, next to `data/chat_events.jsonl`. The file is a JSON object keyed by the event index as a string. Each value is one entry:

| Key | Type | Value |
|---|---|---|
| `state` | string | `pending`, `ready` or `error` |
| `backend` | string | the backend option id |
| `answer` | string | the explanation; `""` while pending or after an error |
| `error` | string or null | the error text; null while pending and when ready |
| `requested_at` | string | `isoformat()` UTC (`+00:00`) |
| `generated_at` | string or null | `isoformat()` UTC; null while pending |

- The server writes the file atomically with `indent=2`.
- A file that is not valid JSON raises, which gives 500 `Internal Server Error` on every explain route.
- A re-run overwrites the whole entry.
- The explain routes never write chat events.

### Stale pending entries

A pending entry older than 600 s (the one-shot timeout) is stale. The running generations live in memory only, so a server restart leaves pending entries without a generation.

Each route that reads a stale pending entry replaces it before it answers:

- `state`: `error`;
- `error`: `interrupted: server restarted`;
- `generated_at`: now.

The route writes the replacement to the file and logs `explain_pending_reaped`. The GET routes therefore can write the file.

## `POST /api/sessions/{session_id}/explain`

- Handler: `src/features/explain/api.py:20`.

Body (`ExplainRequest`, unknown keys ignored):

| Field | Type | Required |
|---|---|---|
| `event_index` | int | yes |
| `backend` | string (backend option id) | yes |

| Status | Case | Body |
|---|---|---|
| 400 | `backend` is not a configured backend option | `{"detail":"unknown backend: <backend>"}` |
| 200 | The divider has a pending entry that is not stale | the stored entry |
| 202 | No entry, a terminal entry, or a stale pending entry | the new pending entry |

The server does not check that `event_index` names a divider or an existing event.

Side effects on 202:

1. Write the pending entry `{"state":"pending","backend":<id>,"answer":"","error":null,"requested_at":<now>,"generated_at":null}`. A process-wide lock covers the read and this write.
2. Start a background task named `explain:<session_id>:<upto>`. The response returns before the task ends.

The background task:

1. Reads the chat events `[0, upto + 1)`, projects them to messages and takes the last assistant message with non-blank text. With no such message, it ends the entry with the error `This round has no explainable answer text.`.
2. Copies `chat_events.jsonl` to a new temporary directory (prefix `charliebot-explain-`) and sets the copy to mode 0444.
3. Builds the backend from the option and starts one agent run through the base `AgentBackend.one_shot_text`, with a 600 s timeout. The agent process runs in an empty temporary directory (prefix `charliebot-oneshot-`). The prompt holds the round text and the path of the read-only copy.
4. Deletes the temporary copy directory.
5. Writes the terminal entry:
   - non-empty answer: `state` `ready` with the answer;
   - empty answer: `state` `error` with `backend returned an empty explanation`;
   - exception: `state` `error` with the exception text, or the exception class name when the text is empty (for example `TimeoutError`). The server also logs `explain_failed`.
6. Broadcasts `{"type":"explain_status","upto":<upto>,"state":<state>,"backend":<id>}` on the session channel. The frame is not persisted.
7. Logs `explain_finished`.

The server sends no `explain_status` frame for the pending state.

## `GET /api/sessions/{session_id}/explain`

- Handler: `src/features/explain/api.py:41`.

| Name | In | Type | Required |
|---|---|---|---|
| `upto` | query | int | yes (422 when absent or not an int) |

| Status | Case | Body |
|---|---|---|
| 200 | The entry exists | the entry, with `answer` and `error` |
| 404 | No entry | `{"detail":"no explain entry for event_index <upto>"}` |

Side effect: replaces a stale pending entry (see above).

## `GET /api/sessions/{session_id}/explain/status`

- Handler: `src/features/explain/api.py:55`.
- Parameters: none.
- 200: one summary per entry, in file key order:

```json
{"12": {"state": "ready", "backend": "claude-opus", "generated_at": "2026-01-01T12:00:00+00:00"}}
```

- With no file or no entry, the body is `{}`.
- Side effect: replaces each stale pending entry (see above).

The chat page calls this route once per session load or switch.
