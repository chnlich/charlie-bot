# Voice HTTP routes

The voice input in the chat page records audio in the browser. With a live transcription backend, it streams the audio to the preview relay `WS /ws/voice/{session_id}` (see [websocket.md](websocket.md)). These HTTP routes are the recognition probe and the upload fallback. They live in `src/features/voice/api.py`, mounted at `/api/voice`.

Shared rules:

- Auth: access key.
- The routes do not check that the session exists. The `session_id` path segment only names the directory for stored recordings.
- An error body is `{"error": "<message>"}`, not `{"detail": …}`. FastAPI request validation errors keep the usual 422 `{"detail": [...]}` form.
- Responses use `FastJsonResponse` (orjson).

## Audio format

Both routes accept one WAV file. `_wav_body_to_pcm` (`src/features/voice/api.py:269`) parses it with Python `wave`:

| Check | Status | `error` |
|---|---|---|
| Not a readable WAV (`wave.Error`, `EOFError`) | 400 | `malformed WAV body: <exception text>` |
| Not 1 channel, 2-byte samples, 16000 Hz | 400 | `voice upload must be 16 kHz mono PCM16 WAV, got <channels>ch/<bits>bit/<rate>Hz` |
| More PCM bytes than the limit × 32000 | 400 | `voice recording exceeds the <limit>s limit` |

The limit is 10 s for the probe and 300 s for the upload.

## Speech bundle

The local decode uses the resident speech bundle (`_speech_bundle`, `src/features/voice/api.py:707`). The server provisions the speech models in the background.

| Case | Status | `error` |
|---|---|---|
| Provisioning still runs | 503 | `speech models are still downloading` |
| Provisioning failed | 503 | `speech models are not ready: <error>` |
| Another failure while the bundle loads | 500 | `speech inference failed: <exception text>` |
| The decode raises | 500 | `speech inference failed: <exception text>` |

The decode runs in a worker thread. A decode failure logs `voice_decode_failed`.

## `POST /api/voice/{session_id}/confirm`

- Handler: `src/features/voice/api.py:79`.
- Body: the raw WAV bytes. The server ignores `Content-Type`.

Steps:

1. Audio format check with the 10 s limit (400).
2. Load the speech bundle (503 or 500).
3. Decode the audio (500).

200: `{"text": "<decoded text>"}`.

Side effects: none. The route stores nothing and logs no `voice_transcribed` line.

## `POST /api/voice/{session_id}`

- Handler: `src/features/voice/api.py:90`.
- Body: `multipart/form-data`.

| Field | Kind | Required | Value |
|---|---|---|---|
| `audio` | file | yes (422 when absent) | the WAV recording |
| `backend` | form string | no | the transcription backend id the browser selected |
| `devices` | form string | no | JSON object with the device facts |

Checks, in order:

1. `devices`, when present:
   - not JSON: 400 `malformed devices form field: <exception text>`;
   - not an object with exactly the keys `input_device`, `capture_settings`, `output_device`, `communications_output_device` and `device_error`: 400 `devices form field must carry exactly ['input_device', 'capture_settings', 'output_device', 'communications_output_device', 'device_error']`.
2. Audio format check with the 300 s limit (400).
3. Backend choice, see below.

### Backend choice

The transcription registry holds the ids `local`, `gemini` and `muse`. `gemini` and `muse` are live backends; `local` is not.

| `backend` value | Path |
|---|---|
| absent, `""`, `local`, an unknown id, or a live backend id | local path |
| another registered backend that is not live and is available | cloud path |
| another registered backend that is not live and is unavailable | 400 `<backend> is unavailable: <reason>` |

With the built-in registry, every value takes the local path. The cloud path needs a non-live backend that code registers at runtime.

### Local path

1. Load the speech bundle (503 or 500). A failure here stores nothing.
2. Write the recording to `sessions/<session_id>/voice/<stem>.wav` (16 kHz mono PCM16). Create the directory when absent.
3. Decode the audio (500). The WAV stays on disk after a decode failure.
4. Write the text to `sessions/<session_id>/voice/<stem>.txt` (UTF-8).
5. Log `voice_transcribed`.

### Cloud path

1. Write the WAV as in the local path, step 2.
2. Send the whole recording to the backend's `transcribe`, with `voice.vocabulary` and `voice.languages` from the config. The text is the last `final` event, or `""` with none.
3. A failure gives 500 `voice transcription failed: <exception text>` and logs `voice_cloud_transcribe_failed`. The WAV stays on disk.
4. Write the `.txt` file and log `voice_transcribed`.

### Result

200: `{"text": "<decoded text>"}`.

The `<stem>` is the UTC time to the millisecond plus 8 hex digits, for example `2026-01-01T120000.123Z_0a1b2c3d`.

The `voice_transcribed` log line has these fields:

- `session_id`
- `audio_path`
- `audio_bytes_size` (PCM bytes)
- `transcription_length`
- `transcription_preview` (first 80 characters)
- `backend` (the backend that produced the text: `local` or the cloud id)
- `selected_backend` (the form value, or null)
- the five device keys (each null when `devices` is absent)

## Security observation

The upload route builds a path from `session_id` and does not check the session. A request path segment `..` (sent percent-encoded) makes the route write under `<home>/voice/`. The route needs the access key.

UNVERIFIED: this was read from the code and not probed.
