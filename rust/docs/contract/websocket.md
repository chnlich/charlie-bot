# WebSocket endpoints

The server has three WebSocket routes:

- `/ws/sessions/{session_id}`: the chat event stream of one session, plus sidebar notifications.
- `/ws/terminal`: a browser terminal attached to a host tmux session.
- `/ws/voice/{session_id}`: the live transcription relay for one voice recording.

All three use the same auth rule. All frames are JSON text frames, except the binary audio frames of the voice relay.

## Shared rules

### Auth

The handler calls `check_ws_auth` (`src/runtime/api/auth.py:157`) before it accepts the socket.

- When no access key is configured, every connection passes.
- Otherwise the query parameter `token` must equal the access key (constant-time compare).
- A missing or wrong `token` makes the handler call `websocket.close(code=4401)` before `accept`.
- The auth check ignores the `Authorization` header and the `charliebot_access_key` cookie.
- A run token is not accepted as `token`.

The wire result of a rejection depends on the ASGI server:

- Under uvicorn (the production server), a close before accept becomes an HTTP 403 response to the upgrade request. The body is empty. A browser reports close code 1006.
- Under the Starlette `TestClient`, the client sees a close with code 4401.

The Rust port must reject the upgrade with HTTP 403. UNVERIFIED: the 403 result comes from uvicorn 0.42 behavior for a close before accept; no probe ran a real uvicorn socket.

The HTTP middlewares do not run for WebSocket scopes. The `http_request` log line is not written for WebSocket connections.

### Frame encoding

- Server frames go out through `orjson` (`src/infra/responses.py:37`) or Starlette `send_json`.
- Parsed content is the same for both encoders. Byte differences do not matter to the browser client.
- A NaN or Infinity float renders as `null`.

### Handler end

When a handler returns without an explicit close, uvicorn closes the socket. UNVERIFIED: the close code in that case (expected 1000) was not measured.

When a handler raises after `accept`, uvicorn closes the socket with code 1011. UNVERIFIED: not measured on a real socket.

## `/ws/sessions/{session_id}`

Handler: `server.py:361`.

### Connect

- Path parameter `session_id`: any string. The handler does not check that the session exists.
- Query parameter `token`: the access key, when one is configured.
- Query parameter `cursor`: the number of chat events the client already has. Required; see the cursor section below.
- Query parameter `asset_version`: the page's static asset version (`web/templates/index.html` renders it
  into the `?v=` script URLs and `web/static/js/websocket.js` sends it here). Optional; see the asset
  version section below.

### Sequence

1. Auth, then `accept`.
2. Log `session_ws_connected`.
3. Read the `cursor` query parameter. A missing value, a value with a non-digit or non-ASCII character, or a value longer than 19 characters closes the socket with code 4400 (the `session_ws_cursor_invalid` warning) before any subscription. No frame is awaited.
4. Read the optional `asset_version` query parameter. When it is present, the handler compares it with the server's current static asset version (`Templating::static_asset_version`, read off the async runtime). A differing value closes the socket with code 4409 (the `session_ws_asset_version_stale` warning, carrying the page's and the server's values) before the subscription, and no frame precedes the close. A failure to compute the current version logs `session_ws_asset_version_failed` and closes the socket with code 1011. An absent value skips the check. The page answers a 4409 by reloading itself once per tab per 60 seconds (`web/static/js/websocket.js`).
5. Subscribe the socket to the channels `session:<session_id>` and `sidebar`.
6. Send the catch-up frames.
7. Send `{"type":"catchup_complete"}`.
8. Enter the keepalive loop until the client disconnects.
9. Unsubscribe from both channels and log `session_ws_disconnected`.

The subscription happens before the catch-up read. Thus no event falls between the catch-up and the live stream. An event can arrive twice: once in catch-up and once live.

### Cursor query parameter

`GET /ws/sessions/{session_id}?cursor=42`. The value is decimal digits only, at most 19 characters, and `int()` converts it, so a leading zero is accepted.

- `cursor` is the number of chat events the client already has. It is read from the handshake URL, not from a client frame.
- A missing `cursor`, a non-decimal or non-ASCII value and a value longer than 19 characters close the socket with code 4400 before the subscription. The server sends no frame first.
- UNVERIFIED: a value above the signed 64-bit maximum (19 digits) was not probed. Python's `int()` holds it, and the catch-up then treats the cursor as at or past the total.
- The Rust handler still reads the first client text frame, not the query (`rust/crates/runtime/session-api/src/websocket.rs`, `receive_cursor`); its port is tracked in `PORT_STATUS.md`.

### Asset version query parameter

`GET /ws/sessions/{session_id}?cursor=42&asset_version=<static asset version>`. The check runs after the
cursor check and before the subscription, so a rejection sends no frame.

- The value is the page's static asset version, URL-encoded by `encodeURIComponent`
  (`web/static/js/websocket.js`) and decoded the way the `cursor` reader decodes.
- The server's current value is `Templating::static_asset_version()` — the same function the page's
  `?v=` script URLs come from (`rust/crates/runtime/registry/src/templating.rs`). Reading it walks the
  served static trees, so the Rust handler runs it on the blocking pool.
- When the two values differ, the handler logs `session_ws_asset_version_stale` (warning, with the
  page's value as `page_version` and the server's as `server_version`) and closes the socket with code
  4409. The page answers by reloading itself, so the tab runs the deploy's current JavaScript.
- When computing the server's value fails, the handler logs `session_ws_asset_version_failed` (error,
  with the error) and closes the socket with code 1011, the code a catch-up failure already uses.
- When the parameter is absent, the handler skips the check and serves the connection as before.
- The page reloads at most once per tab per 60 seconds: it stores the last automatic reload's time in
  `sessionStorage`, and a second 4409 inside that window shows a page-level banner ("This page is out
  of date. Reload to continue.", with a Reload button) instead of reloading. Any close with another
  code keeps the reconnect backoff.

### Catch-up

The catch-up replays the events at and after the cursor.

For a session whose `profile` is `worker`, the event list is the projected Runs transcript (`worker_transcript.load_worker_transcript`). The event index offset is 0.

For every other session, including an unknown session id:

- The total is `get_chat_event_count_sync`. An unknown session counts 0 events.
- When `cursor >= total`, the server sends no catch-up frame.
- Otherwise the server reads all chat events from `sessions/<id>/data/chat_events.jsonl` (the state contract documents the file).
- The event index offset is the session's `archive_offset`.

The replay walk (`_CatchupWalk`, `server.py:445`) feeds every event, from index 0, into a fresh `MessageAggregator`. For each event at global index `offset + i`:

1. The aggregator returns zero or more deltas. A delta is a `message` frame or a `stream` frame.
2. When the global index is below the cursor, the walk drops the deltas and the raw event.
3. A `stream` delta replaces the buffered latest stream delta. The walk does not send it yet.
4. Any other delta clears the buffered stream delta and goes into the frame list.
5. When the event type is in `RAW_EVENTS_REPLACED_BY_DELTAS`, the walk does not send the raw event.
6. Otherwise the walk appends a copy of the raw event. It sets `event_index` to the global index when the event has no `event_index`.

After the last event, a buffered stream delta goes at the end of the frame list.

`RAW_EVENTS_REPLACED_BY_DELTAS` (`src/runtime/session_events.py:23`):

- `assistant`
- `user`
- `scheduled_trigger`
- `child_report`
- `task_closed`
- `task_reopened`
- `web_reply`
- `web_reply_notice`
- `web_status`

When a send fails, the catch-up stops. The handler still tries to send `catchup_complete`. A failure in the catch-up read logs `session_ws_catchup_failed`, and the handler skips `catchup_complete`.

Example catch-up for a session with a `task_created` event and one system error message:

```json
{"id":"…","type":"task_created","timestamp":"2026-01-01T00:00:00.000000+00:00","actor":"user","source_session_id":"00000000-0000-4000-8000-000000000001","request_id":"operator-create-…","task_parent_id":null,"task_spec_hash":null,"event_index":0}
{"type":"message","message":{"role":"system","content":"Error: No active master agent to cancel.","timestamp":"…","event_index":1,"id":"…"}}
{"type":"assistant_error","content":"No active master agent to cancel.","id":"…","timestamp":"…","event_index":1}
{"type":"catchup_complete"}
```

The `message` and `stream` delta shapes are in [shapes.md](shapes.md#chat-message-objects).

### Keepalive

- The server waits for client text frames with a 30 s timeout (`WS_KEEPALIVE_TIMEOUT`, `src/infra/timeouts.py:134`).
- After 30 s with no client text frame, the server sends `{"type":"ping"}`.
- The server ignores the content of client text frames.
- A client binary frame raises inside `receive_text`. The handler logs `session_ws_closed` and ends.

### Live frames on the session channel

`SessionEvents.persist_and_broadcast` (`src/runtime/session_events.py`) appends an event to the chat log and then broadcasts it:

1. It sets `event["event_index"] = archive_offset + cached_event_count - 1`.
2. It feeds the event to the session's live aggregator and broadcasts each delta.
3. It broadcasts the raw event when its type is not in `RAW_EVENTS_REPLACED_BY_DELTAS`.

Delta frames:

| Frame | Shape |
|---|---|
| committed message | `{"type":"message","message":{…}}` |
| in-progress draft | `{"type":"stream","message":{…}}` |

Stream coalescing (`src/runtime/streaming.py:78`):

- A `stream` frame to a channel with no subscriber is dropped.
- The first `stream` frame of a burst goes out at once and opens a 200 ms window for that channel.
- Later `stream` frames in the window replace one pending frame.
- When the window ends, the pending frame goes out.
- A frame of type `message`, `assistant_error` or `error` drops the pending stream frame and closes the window. Then it goes out.

Session-channel frames that are not chat events. The server broadcasts them and does not persist them, so catch-up never replays them:

| `type` | Fields | Producer |
|---|---|---|
| `session_renamed` | `name` | `src/runtime/autonamer.py:177` |
| `plan_updated` | `session_id`, `plan_id` | `src/features/artifacts/plans.py:263` |
| `explain_status` | `upto`, `state`, `backend` | `src/features/explain/explain.py:181` |
| `web_status_subtree` | `source_session_id` | `src/features/web_reply/status.py:88`, sent to each ancestor's channel |

### Live frames on the sidebar channel

Every session socket receives the sidebar frames of every session.

| `type` | Fields | Producer |
|---|---|---|
| `running_changed` | `session_id`, `has_running_tasks` (bool), `thinking_since` (ISO string with `+00:00`, or null), `auto_trigger` (bool) | `src/runtime/master_cc_queue.py:94` |
| `unread_changed` | `session_id`, `has_unread` (bool) | `src/runtime/session_lifecycle.py:84` |
| `session_group_changed` | `session_id`, `group` (string or null) | `src/runtime/session_lifecycle.py:165` |
| `backend_switched` | `session_id`, `backend` | `src/runtime/session_lifecycle.py:55` |
| `task_tree_changed` | `session_id`, `fact_type` (event type or null) | `src/runtime/session_events.py:202` |
| `session_renamed` | `session_id`, `name` | `src/runtime/autonamer.py:182` |
| `ext_usage` | `providers` (the `/api/ext-usage` providers map) | `src/features/usage/ext_usage.py:107` |

The sidebar `backend_switched` frame differs from the persisted `backend_switched` chat event. The chat event has `from`, `to`, `previous_native_backend` and `previous_native_session_id` (see `POST /api/sessions/{session_id}/backend` in [sessions.md](sessions.md)).

### Message types the browser handles

`web/static/js/websocket-handlers.js` dispatches on `type`. The Rust port must keep these names:

- `assistant_error`
- `backend_switched`
- `catchup_complete`
- `error`
- `explain_status`
- `ext_usage`
- `master_done`
- `message`
- `ping`
- `plan_updated`
- `result`
- `running_changed`
- `session_group_changed`
- `session_renamed`
- `stream`
- `task_delegated`
- `task_tree_changed`
- `unread_changed`
- `web_status_subtree`
- `worker_summary`

The browser reconnects after a close. The delay starts at 1 s and grows to at most 30 s. A close with
code 4409 is the exception: the page reloads itself instead (see the asset version section above).

### Shutdown

At server shutdown, after the shared HTTP client closes, `StreamingManager.close_all` (`src/runtime/streaming.py:131`) drops pending stream frames. Then it closes every open session socket with the default close code (1000) and waits for each close frame. The step is timed as `ws_close_ms`.

## `/ws/terminal`

Handler: `src/features/terminal/api.py:13`. Relay: `src/runtime/agent_process/pty_common.py:250`.

### Connect

- No path parameter.
- Query parameter `token`: the access key, when one is configured.

### Sequence

1. Auth, then `accept`. Log `terminal_ws_connected`.
2. Make sure the tmux session exists (`src/features/terminal/terminal.py`):
   - tmux socket: `tmux -L charliebot`;
   - session name: `charliebot-terminal` for the default home;
   - for any other home: `charliebot-terminal-<first 8 hex chars of sha256(home path)>`;
   - a new session starts `bash -l` in the user's home directory;
   - when `CHARLIEBOT_HOME` is set, the session gets `-e CHARLIEBOT_HOME=<home>`.
3. Spawn a PTY that runs `tmux -L charliebot attach -t <name>` with `TERM=xterm-256color`. The PTY starts at 80 columns by 24 rows.
4. Relay frames in both directions until the client disconnects.
5. Cancel the output pump, close the PTY, log `terminal_ws_disconnected`.

When step 2 or step 3 fails, the server sends `{"type":"pty_exit","error":"<message>"}`. Then the handler returns.

### Server to client

| Frame | When |
|---|---|
| `{"type":"pty_output","data":"<base64>"}` | Each PTY read of up to 4096 bytes. `data` is standard base64 with padding. |
| `{"type":"pty_exit"}` | The PTY reached end of file. The receive loop continues until the client disconnects. |
| `{"type":"pty_exit","error":"<message>"}` | The tmux session or the PTY failed to start. |
| `{"type":"ping"}` | 30 s (`PTY_WS_RECV_TIMEOUT`) passed with no client text frame. |

### Client to server

| Frame | Effect |
|---|---|
| `{"type":"pty_input","data":"<base64>"}` | Decode `data` (non-strict) and write the bytes to the PTY. An empty `data` writes nothing. A decode failure drops the frame. |
| `{"type":"pty_resize","cols":N,"rows":N}` | Set the PTY size. A missing or zero value means 80 columns or 24 rows. Values below 1 become 1. A non-integer value drops the frame. |

- The relay drops a frame that is not valid JSON.
- The relay ignores a JSON object with any other `type`.
- A JSON frame that is not an object raises `AttributeError` and ends the relay.

## `/ws/voice/{session_id}`

Handler: `src/features/voice/api.py:401`. Relay: `voice_preview_relay`, `src/features/voice/api.py:409`.

### Connect

- Path parameter `session_id`: the session that owns the recording archive. The handler does not check that the session exists.
- Query parameter `backend`: the transcription backend id. Default `""`.
- Query parameter `token`: the access key, when one is configured.

### Refusal

After auth, the server builds the backend. The server refuses the connection in these cases:

| Case | Error message |
|---|---|
| Unknown backend id | `unknown transcription backend '<id>'; known: gemini, muse` |
| Backend not available | `voice backend '<id>' is unavailable: <reason>` |
| Backend has no live partials | `voice backend '<id>' does not stream live partials` |

A refusal is: `accept`, one frame `{"type":"error","message":"<message>"}`, then a close with the default code.

The known ids come from the voice registry in registration order.

### Client to server

- Binary frames: 16 kHz mono PCM16 little-endian audio, from the start of the recording.
- One text end frame, which seals the recording:
  - `{"type":"end"}`, or
  - `{"type":"end","devices":{…}}`.

The `devices` object must have exactly these keys:

- `input_device`
- `capture_settings`
- `output_device`
- `communications_output_device`
- `device_error`

### Server to client

| Frame | Meaning |
|---|---|
| `{"type":"partial","text":"…"}` | A live partial transcript. |
| `{"type":"final","text":"…"}` | The final transcript. The server sends it only after it wrote the archive pair. |
| `{"type":"error","message":"…"}` | A failure. The relay sends at most one error frame per failure path. |

### Limits and errors

- The audio queue holds at most 30 s of unconsumed audio (960 000 bytes).
- A binary frame past that limit ends the preview with the error `voice preview buffer overflowed (over 30s of audio unconsumed)`.
- The kept recording stops at 5 minutes (9 600 000 bytes). Frames past the cap still feed the backend.
- An end frame that is not valid JSON gives the error `malformed preview control frame '<text>'`.
- A JSON end frame whose `type` is not `end` gives `unsupported preview control frame '<text>'`.
- A bad `devices` value gives `end frame devices must carry exactly [<keys>]: <value>`.
- After an overflow or a protocol error, the server sends one error frame. Then it reads and discards frames until the client closes. Then it closes.
- A backend exception gives one error frame whose message is the exception text.
- A backend final before the end frame is a backend failure: the error message is `backend produced its final before the recording's end frame`, and the server archives nothing.
- A client frame after the end frame cancels the backend, sends `unexpected frame after the recording's end`, and drains until the client closes.
- A client disconnect after the end frame and before the final cancels the backend. The server logs `voice_preview_abandoned` and archives nothing.

### Side effects

When the backend yields its final after the end frame:

1. The server writes `sessions/<session_id>/voice/<stem>.txt` with the final text.
2. It writes `sessions/<session_id>/voice/<stem>.wav.partial` (16 kHz mono PCM16 WAV).
3. It renames the `.wav.partial` file to `<stem>.wav`.
4. It logs `voice_transcribed` with `end_to_final_s`.
5. It sends the `final` frame, then closes the socket.

`<stem>` is the UTC time as `YYYY-MM-DDTHHMMSS.mmmZ`, then `_`, then 8 random hex characters.

When the archive fails, the server deletes the partial files and sends `{"type":"error","message":"recording archive failed"}` in place of the final.
