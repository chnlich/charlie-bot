# Voice HTTP routes

The voice input in the chat page records audio in the browser. With a live transcription backend, it streams the audio to the preview relay `WS /ws/voice/{session_id}` (see [websocket.md](websocket.md)). This HTTP route is the upload fallback. It lives in `src/features/voice/api.py`, mounted at `/api/voice`. The recognition probe route `POST /api/voice/{session_id}/confirm` and the local speech engine were removed at `d1f7e3ba6` (`93b427659`, `4c3bd3fda`); every upload transcribes through an online backend.

Shared rules:

- Auth: access key.
- The route does not check that the session exists. The `session_id` path segment only names the directory for stored recordings.
- An error body is `{"error": "<message>"}`, not `{"detail": …}`. FastAPI request validation errors keep the usual 422 `{"detail": [...]}` form.
- Responses use `FastJsonResponse` (orjson).

## Audio format

The route accepts one WAV file. `_wav_body_to_pcm` (`src/features/voice/api.py:176`) parses it with Python `wave`:

| Check | Status | `error` |
|---|---|---|
| Not a readable WAV (`wave.Error`, `EOFError`) | 400 | `malformed WAV body: <exception text>` |
| Not 1 channel, 2-byte samples, 16000 Hz | 400 | `voice upload must be 16 kHz mono PCM16 WAV, got <channels>ch/<bits>bit/<rate>Hz` |
| More PCM bytes than the limit × 32000 | 400 | `voice recording exceeds the <limit>s limit` |

The limit is 300 s.

## `POST /api/voice/{session_id}`

- Handler: `src/features/voice/api.py:70`.
- Body: `multipart/form-data`.

| Field | Kind | Required | Value |
|---|---|---|---|
| `audio` | file | yes (422 when absent) | the WAV recording |
| `backend` | form string | no | the transcription backend id the browser selected |
| `devices` | form string | no | JSON object with the device facts |

Checks, in order:

1. `devices`, when present (`src/features/voice/api.py:119`):
   - not JSON: 400 `malformed devices form field: <exception text>`;
   - not an object with exactly the keys `input_device`, `capture_settings`, `output_device`, `communications_output_device` and `device_error`: 400 `devices form field must carry exactly ['input_device', 'capture_settings', 'output_device', 'communications_output_device', 'device_error']`.
2. Audio format check (400, see above).
3. Write the recording to `sessions/<session_id>/voice/<stem>.wav` (16 kHz mono PCM16). Create the directory when absent. This runs before the backend is chosen, so an unknown or unavailable backend keeps the WAV.
4. Backend choice, see below (400 with the reason for an unknown or unavailable id).
5. Transcribe the whole recording through the chosen backend (500 on failure).
6. Write the text to `sessions/<session_id>/voice/<stem>.txt` (UTF-8) and log `voice_transcribed`.

### Backend choice

The registry holds the ids `gemini` and `muse`. Both are live backends. `local` is no longer an id.

| `backend` value | Path |
|---|---|
| absent | `voice.default_backend` (`gemini` by default) |
| `gemini` or `muse` | that backend |
| any other id | 400 `unknown transcription backend '<id>'; known: gemini, muse` |
| a registered backend without its credential | 400 `<id> is unavailable: <reason>`, for example `gemini is unavailable: needs gemini.api_key` |

Both 400 cases are raised before any audio is sent (`src/features/voice/transcription/recording.py`, `select_backend`).

### Transcription failure

A backend failure during the recording gives 500 `voice transcription failed: <exception text>` (or `<id> produced no final event`) and logs `voice_transcribe_failed`. The WAV stays on disk, and no `.txt` is written.

### Result

200: `{"text": "<transcript>"}`.

The `<stem>` is the UTC time to the millisecond plus 8 hex digits, for example `2026-01-01T120000.123Z_0a1b2c3d`.

The `voice_transcribed` log line has these fields:

- `session_id`
- `audio_path`
- `audio_bytes_size` (PCM bytes)
- `transcription_length`
- `transcription_preview` (first 80 characters)
- `backend` (the backend that produced the text, `gemini` or `muse`)
- `selected_backend` (the form value, or null)
- the five device keys (each null when `devices` is absent)

## Security observation

The upload route builds a path from `session_id` and does not check the session. A request path segment `..` (sent percent-encoded) makes the route write under `<home>/voice/`. The route needs the access key.

UNVERIFIED: this was read from the code and not probed.
