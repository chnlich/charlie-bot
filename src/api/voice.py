"""Voice-input endpoints: the preview relay's live transcription plus the upload fallback.

With a live backend the browser streams every audio chunk to the preview relay
(/ws/voice/{session_id}) while recording; the relay forwards each chunk to the
backend and keeps it. After the browser seals the recording, the relay archives
the copy it already received: sessions/{id}/voice/, a 16 kHz mono PCM16 WAV
plus a .txt carrying the final text. The final is pushed only once that pair is
on disk, so a browser that receives it ends the recording with no upload. The
full-upload endpoint (POST /api/voice/{session_id}) is the fallback — no final
inside the browser's 2 s budget, a relay failure, or a non-live backend — and
it persists the recording BEFORE decoding it, so a decode failure or an
abandoned request never loses the audio. Its form's backend field dispatches: a
registered non-live backend other than the local one decodes the upload itself
(persist first, then that backend's transcribe over the whole recording), while
an absent field, an unknown id, a live backend id, or the local id keeps the
local decode behind the speech-bundle readiness gate (503) — an unavailable
non-live selection is an error, never a local fallback. The confirm probe (POST
/api/voice/{session_id}/confirm) decodes the opening clip through the local
model and persists nothing. The relay never falls back: its failures hand the
recording to the upload endpoint.
"""

from __future__ import annotations

import asyncio
import io
import json
import os
from collections.abc import AsyncIterator
from contextlib import suppress
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING
from uuid import uuid4

from fastapi import APIRouter, Form, Request, UploadFile, WebSocket

from src.api.responses import FastJsonResponse
from src.core.config import CharlieBotConfig, get_config
from src.core.log_once import LazyStructlogLogger

if TYPE_CHECKING:
  from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend

log = LazyStructlogLogger()

router = APIRouter()

# The recognition probe decodes the recording's opening clip only; the full upload
# takes a whole dictation and its cap is the transcriber's MAX_RECORDING_SAMPLES.
CONFIRM_MAX_SECONDS = 10

# The recording's device facts, sent by the browser both as the end frame's
# ``devices`` object and as the upload form's ``devices`` field, and logged by
# voice_transcribed under these exact names. A request without the object (a
# page still running the pre-devices script across a server restart) logs all
# five as None.
_VOICE_DEVICE_FIELDS = (
    "input_device",
    "capture_settings",
    "output_device",
    "communications_output_device",
    "device_error",
)


class _VoiceRequestError(Exception):
  """A voice-request failure carrying its HTTP status; endpoints render {"error": ...}."""

  def __init__(self, status_code: int, message: str) -> None:
    super().__init__(message)
    self.status_code = status_code
    self.message = message


@router.post("/{session_id}/confirm")
async def confirm_voice_recording(request: Request, session_id: str) -> FastJsonResponse:
  """Decode the opening clip as the one recognition probe; nothing is persisted."""
  try:
    pcm_bytes = _wav_body_to_pcm(await request.body(), _confirm_max_samples())
    text = await _decode_pcm(session_id, pcm_bytes)
  except _VoiceRequestError as exc:
    return _error_response(exc)
  return FastJsonResponse({"text": text})


@router.post("/{session_id}")
async def upload_voice_recording(
    session_id: str,
    audio: UploadFile,
    backend: str | None = Form(None),
    devices: str | None = Form(None),
) -> FastJsonResponse:
  """The fallback path: persist the recording, decode it, return the text.

  Multipart form: ``audio`` is the WAV (same validation and size cap as the raw
  body ever enforced), ``backend`` names the backend the browser selected, and
  ``devices`` is the recording's device object as JSON. A registered non-live
  backend other than the local one decodes the recording itself; every other
  value keeps the local decode. The voice_transcribed log line records
  ``backend`` — the id of the backend that produced the persisted text — and
  ``selected_backend``, the form's ``backend`` exactly as sent, None when
  absent.
  """
  try:
    device_info = _devices_form_field(devices)
    pcm_bytes = _wav_body_to_pcm(await audio.read(), _full_max_samples())
    cloud_backend = _selected_cloud_backend(backend)
    if cloud_backend is not None:
      return await _upload_via_cloud_backend(session_id, cloud_backend, backend, pcm_bytes, device_info)
    # The bundle comes first so models-not-ready (503) persists nothing — the client
    # keeps its buffer and retries, and no orphan wav piles up per retry.
    bundle = await _speech_bundle()
    audio_path = await asyncio.to_thread(_persist_voice_audio, get_config(), session_id, pcm_bytes)
    text = await _transcribe_with_bundle(session_id, bundle, pcm_bytes)
    # The server decoded this recording, so the local backend produced the
    # persisted text; its id comes from the class, not a fresh literal. Lazy
    # import: the speech stack stays off `import server`'s startup path
    # (the M99 import floor, docs/perf_baseline.md).
    from src.agents.transcription.local import LocalTranscriptionBackend

    produced_by = LocalTranscriptionBackend.id
  except _VoiceRequestError as exc:
    # A decode failure (500) leaves the wav on disk: persist-before-decode means the
    # recording survives every later failure.
    return _error_response(exc)
  await asyncio.to_thread(_write_voice_transcript, audio_path, text)
  _log_voice_transcribed(session_id, audio_path, pcm_bytes, text, produced_by, backend, device_info)
  return FastJsonResponse({"text": text})


def _error_response(exc: _VoiceRequestError) -> FastJsonResponse:
  return FastJsonResponse({"error": exc.message}, status_code=exc.status_code)


def _selected_cloud_backend(backend_id: str | None) -> TranscriptionBackend | None:
  """The registered non-live backend the form's backend field selects; None for the local path.

  An absent field, an unknown id, a live backend (the relay owns those), and the
  local backend itself all keep today's local decode. A selected non-live backend
  that is unavailable raises instead — the selection never falls back to the
  local model, the same no-fallback contract the relay follows.
  """
  from src.agents.transcription import registry
  from src.agents.transcription.local import LocalTranscriptionBackend

  if not backend_id or backend_id == LocalTranscriptionBackend.id:
    return None
  if backend_id not in registry.backend_ids():
    return None
  candidate = registry.build_transcription_backend(backend_id, get_config())
  if candidate.live_partials:
    return None
  reason = candidate.unavailable_reason()
  if reason is not None:
    raise _VoiceRequestError(400, f"{backend_id} is unavailable: {reason}")
  return candidate


async def _upload_via_cloud_backend(
    session_id: str,
    cloud_backend: TranscriptionBackend,
    selected_backend: str | None,
    pcm_bytes: bytes,
    devices: dict | None,
) -> FastJsonResponse:
  """Decode the upload with the selected non-live cloud backend; the recording persists first.

  The WAV lands on disk before the backend runs, so a transcription failure
  leaves the recording for the browser's retry — the same persist-before-decode
  order the local path follows.
  """
  cfg = get_config()
  audio_path = await asyncio.to_thread(_persist_voice_audio, cfg, session_id, pcm_bytes)
  try:
    text = await _transcribe_with_cloud_backend(session_id, cloud_backend, pcm_bytes, cfg)
  except _VoiceRequestError as exc:
    # A transcription failure (500) leaves the wav on disk: the recording
    # survives every later failure.
    return _error_response(exc)
  await asyncio.to_thread(_write_voice_transcript, audio_path, text)
  _log_voice_transcribed(session_id, audio_path, pcm_bytes, text, cloud_backend.id, selected_backend, devices)
  return FastJsonResponse({"text": text})


async def _transcribe_with_cloud_backend(
    session_id: str, cloud_backend: TranscriptionBackend, pcm_bytes: bytes, cfg: CharlieBotConfig) -> str:
  """One whole-clip pass through the cloud backend's transcribe; any failure maps to 500."""

  async def whole_clip() -> AsyncIterator[bytes]:
    yield pcm_bytes

  text = ""
  try:
    async for event in cloud_backend.transcribe(whole_clip(), vocabulary=cfg.voice.vocabulary,
                                                languages=cfg.voice.languages):
      if event.kind == "final":
        text = event.text
  except Exception as exc:
    log.exception("voice_cloud_transcribe_failed", session_id=session_id, backend=cloud_backend.id)
    raise _VoiceRequestError(500, f"voice transcription failed: {exc}") from exc
  return text


def _devices_form_field(raw: str | None) -> dict | None:
  """The upload form's ``devices`` field as its object; None when the field is absent.

  A present field must be JSON and an object carrying exactly the device keys:
  anything else is a 400 — the browser always sends the whole object, so a
  partial one is a broken client, not a default to paper over.
  """
  if raw is None:
    return None
  try:
    parsed = json.loads(raw)
  except ValueError as exc:
    raise _VoiceRequestError(400, f"malformed devices form field: {exc}") from exc
  if not isinstance(parsed, dict) or set(parsed) != set(_VOICE_DEVICE_FIELDS):
    raise _VoiceRequestError(400, f"devices form field must carry exactly {list(_VOICE_DEVICE_FIELDS)}")
  return parsed


def _log_voice_transcribed(
    session_id: str,
    audio_path: Path,
    pcm_bytes: bytes,
    text: str,
    produced_by: str,
    selected_backend: str | None,
    devices: dict | None,
) -> None:
  """The voice_transcribed line both archive paths write; its fields are the log's contract.

  ``devices`` is the recording's device object; None — every device field
  logged as None — is the request from a page loaded before the devices
  existed, still open across a server restart.
  """
  if devices is None:
    devices = dict.fromkeys(_VOICE_DEVICE_FIELDS)
  log.info(
      "voice_transcribed",
      session_id=session_id,
      audio_path=str(audio_path),
      audio_bytes_size=len(pcm_bytes),
      transcription_length=len(text),
      transcription_preview=text[:80],
      backend=produced_by,
      selected_backend=selected_backend,
      input_device=devices["input_device"],
      capture_settings=devices["capture_settings"],
      output_device=devices["output_device"],
      communications_output_device=devices["communications_output_device"],
      device_error=devices["device_error"],
  )


def _confirm_max_samples() -> int:
  from src.agents.transcriber import SAMPLE_RATE

  return SAMPLE_RATE * CONFIRM_MAX_SECONDS


def _full_max_samples() -> int:
  """The recording cap is the transcriber's own: one source owns the number."""
  from src.agents.transcriber import MAX_RECORDING_SAMPLES

  return MAX_RECORDING_SAMPLES


def _wav_body_to_pcm(body: bytes, max_samples: int) -> bytes:
  """Parse a request's WAV as 16 kHz mono PCM16 and return its PCM frames.

  Anything else — a truncated header, a foreign container, a wrong rate — is a 400:
  the browser worklet always produces the one accepted format.
  """
  import wave

  from src.agents.transcriber import SAMPLE_RATE

  try:
    reader = wave.open(io.BytesIO(body), "rb")
  except (wave.Error, EOFError) as exc:
    raise _VoiceRequestError(400, f"malformed WAV body: {exc}") from exc
  with reader:
    format_ = (reader.getnchannels(), reader.getsampwidth(), reader.getframerate())
    if format_ != (1, 2, SAMPLE_RATE):
      raise _VoiceRequestError(
          400, f"voice upload must be 16 kHz mono PCM16 WAV, got {format_[0]}ch/"
          f"{format_[1] * 8}bit/{format_[2]}Hz")
    pcm_bytes = reader.readframes(reader.getnframes())
  if len(pcm_bytes) > max_samples * 2:
    raise _VoiceRequestError(400, f"voice recording exceeds the {max_samples // SAMPLE_RATE}s limit")
  return pcm_bytes


# ---------------------------------------------------------------------------
# Preview relay: /ws/voice/{session_id}, one recording's live partials
# ---------------------------------------------------------------------------
# The relay buffers the browser's audio frames in a bounded queue, feeds that
# queue to the selected backend's transcribe as its audio iterator, and
# forwards the resulting events as they arrive. Every frame it accepts is also
# kept as the recording; once the browser seals the recording, the backend's
# final archives that copy before the final is pushed, and a browser that
# leaves first archives nothing. The relay decodes nothing and has no
# fallback: every failure hands the recording to the upload endpoint.

PREVIEW_QUEUE_SECONDS = 30


class _PreviewSocketLost(Exception):  # noqa: N818  a lost socket is a state, not a fault name
  """The preview socket died mid-relay; no further frame can go out."""


class _PreviewProtocolError(Exception):
  """The browser sent a frame the preview protocol does not define."""


class _PreviewEarlyFinalError(Exception):
  """The backend yielded its final before the browser's end frame sealed the recording."""


class _PreviewQueue:
  """The relay's one buffer between the browser and the backend: 30 s of audio.

  The receive loop's only job is filling this; the buffer itself is the audio
  iterator handed to the backend, so a slow backend never blocks the browser and
  an overflow ends the preview with an error. Every frame the queue accepts is
  also kept as the recording the relay's archive writes; an overflow leaves that
  copy unused and the fallback upload owns the recording instead.
  """

  def __init__(self, max_bytes: int, recording_budget_bytes: int) -> None:
    self._frames: asyncio.Queue[bytes | None] = asyncio.Queue()
    self._max_bytes = max_bytes
    self._queued_bytes = 0
    self._ended = False
    self._devices: dict | None = None
    self._recording = bytearray()
    self._recording_budget_bytes = recording_budget_bytes

  @property
  def ended(self) -> bool:
    """Whether the browser's end frame has sealed the recording."""
    return self._ended

  @property
  def devices(self) -> dict | None:
    """The end frame's device object; None when the frame carried none."""
    return self._devices

  def put(self, frame: bytes) -> bool:
    """Buffer one audio frame; False when the frame would exceed the budget."""
    if self._ended or self._queued_bytes + len(frame) > self._max_bytes:
      return False
    self._frames.put_nowait(frame)
    self._queued_bytes += len(frame)
    # Kept samples stop at the recording cap, cut inside the frame the way the
    # browser's assembleVoiceWav cuts its local buffer at the same cap.
    room = self._recording_budget_bytes - len(self._recording)
    if room > 0:
      self._recording += frame[:room]
    return True

  def recording_pcm(self) -> bytes:
    """The audio accepted so far: 16 kHz mono PCM16, cut at the recording cap."""
    return bytes(self._recording)

  def end_audio(self, devices: dict | None) -> None:
    """Seal the recording: the audio iterator ends after the buffered frames."""
    self._frames.put_nowait(None)
    self._ended = True
    self._devices = devices

  async def audio(self) -> AsyncIterator[bytes]:
    """The audio the backend consumes; exhausted once the recording ends."""
    while True:
      frame = await self._frames.get()
      if frame is None:
        return
      self._queued_bytes -= len(frame)
      yield frame


def _preview_queue_budget_bytes() -> int:
  """The buffer's size: 30 s of audio, in bytes at the transcriber's rate."""
  from src.agents.transcriber import SAMPLE_RATE

  return PREVIEW_QUEUE_SECONDS * SAMPLE_RATE * 2


def _preview_recording_budget_bytes() -> int:
  """The kept recording's size: the transcriber's sample cap, 2 bytes per PCM16 sample."""
  return _full_max_samples() * 2


async def voice_preview_relay(websocket: WebSocket, session_id: str, backend_id: str) -> None:
  """One recording's live preview: stream the selected backend's partials, archive its audio.

  Auth happens in server.py next to /ws/sessions. The browser sends binary
  16 kHz PCM16 chunks from the start of the recording and a text end frame on
  stop — ``{"type":"end"}``, or ``{"type":"end","devices":{...}}`` carrying the
  recording's device facts; the relay answers with partial, final, and error
  frames. A refusal (unknown id, missing credential, no live partials) is one
  error frame and a close. Any backend failure is one error frame plus the
  voice_preview_failed log line, carrying the backend id and the reason and
  never the credential. After the end frame the relay archives the recording it
  already received and pushes the final only once the pair is on disk, so
  receiving the final tells the browser its recording is archived; a browser
  that leaves first, or a final that arrives before the end frame, archives
  nothing. An overflow ends the preview with an error while the socket keeps
  draining until the browser closes. Closing the socket from either side closes
  the backend iterator, which closes the backend's own connection.
  """
  from src.agents.transcription.registry import build_transcription_backend
  cfg = get_config()
  try:
    backend = build_transcription_backend(backend_id, cfg)
  except ValueError as exc:
    await _refuse_preview(websocket, str(exc))
    return
  reason = backend.unavailable_reason()
  if reason is not None:
    await _refuse_preview(websocket, f"voice backend {backend_id!r} is unavailable: {reason}")
    return
  if not backend.live_partials:
    await _refuse_preview(websocket, f"voice backend {backend_id!r} does not stream live partials")
    return

  await websocket.accept()
  queue = _PreviewQueue(_preview_queue_budget_bytes(), _preview_recording_budget_bytes())
  streamer = asyncio.create_task(
      _stream_preview_events(websocket, backend, queue, cfg, session_id=session_id, backend_id=backend_id))
  try:
    try:
      outcome, error_message = await _receive_preview_frames(websocket, queue)
    except _PreviewProtocolError as exc:
      outcome, error_message = "protocol", str(exc)
    if outcome == "ended":
      # The final rides out through the streamer; the socket closes after it.
      await _settle_sealed_recording(websocket, streamer)
      await _close_preview_socket(websocket)
      return
    if outcome == "disconnect":
      return
    # Overflow or a protocol violation: one error frame ends the preview, then
    # the socket keeps draining until the browser closes it — its recording is
    # unaffected and still uploads through the HTTP endpoint.
    streamer.cancel()
    await _await_cancelled_streamer(streamer)
    with suppress(_PreviewSocketLost):
      await _push_preview_frame(websocket, {"type": "error", "message": error_message})
    await _drain_preview_socket(websocket)
    await _close_preview_socket(websocket)
  finally:
    # Every exit closes the backend iterator: the normal end already did, a
    # browser hang-up or an overflow cancelled it, and this handler dying
    # mid-relay still must not leave the backend's session open.
    if not streamer.done():
      streamer.cancel()
      await _await_cancelled_streamer(streamer)


async def _refuse_preview(websocket: WebSocket, message: str) -> None:
  """One error frame, then a close: the refusal is the whole session."""
  await websocket.accept()
  await websocket.send_json({"type": "error", "message": message})
  await websocket.close()


async def _receive_preview_frames(websocket: WebSocket, queue: _PreviewQueue) -> tuple[str, str | None]:
  """Fill the queue from the socket; returns (why it stopped, error frame or None).

  "ended" — the browser sealed the recording; "overflow" — the buffer is full and
  the preview must end; "disconnect" — the browser went away. A frame the
  protocol does not define raises _PreviewProtocolError.
  """
  while True:
    message = await websocket.receive()
    kind = message.get("type")
    if kind == "websocket.disconnect":
      return "disconnect", None
    if kind == "websocket.connect":
      continue
    frame = message.get("bytes")
    if frame is not None:
      if not queue.put(frame):
        return "overflow", f"voice preview buffer overflowed (over {PREVIEW_QUEUE_SECONDS}s of audio unconsumed)"
      continue
    queue.end_audio(_preview_end_frame_devices(message.get("text")))
    return "ended", None


def _preview_end_frame_devices(text: object) -> dict | None:
  """The devices the one control frame carries: ``{"type": "end"}`` and
  ``{"type": "end", "devices": {...}}`` both seal the recording.

  Returns None for the bare frame and the devices object for the devices
  frame. Anything else — malformed JSON, a foreign type, a ``devices`` value
  that is not an object with exactly the device keys — is a
  _PreviewProtocolError: the browser's own client never sends it.
  """
  try:
    control = json.loads(text)  # type: ignore[arg-type]
  except (TypeError, ValueError) as exc:
    raise _PreviewProtocolError(f"malformed preview control frame {text!r}") from exc
  if not (isinstance(control, dict) and control.get("type") == "end"):
    raise _PreviewProtocolError(f"unsupported preview control frame {text!r}")
  if "devices" not in control:
    return None
  devices = control["devices"]
  if not isinstance(devices, dict) or set(devices) != set(_VOICE_DEVICE_FIELDS):
    raise _PreviewProtocolError(f"end frame devices must carry exactly {list(_VOICE_DEVICE_FIELDS)}: {devices!r}")
  return devices


async def _stream_preview_events(
    websocket: WebSocket,
    backend: TranscriptionBackend,
    queue: _PreviewQueue,
    cfg: CharlieBotConfig,
    *,
    session_id: str,
    backend_id: str,
) -> None:
  """Drive the backend off the queue and forward its events as they arrive.

  Every backend failure — TranscriptionRejected included — becomes one error
  frame plus the voice_preview_failed log line, carrying the backend id and the
  failure reason and never the credential. The final event is the archive
  decision: the recording is archived first and the final is pushed only after
  that succeeded, so receiving it tells the browser the pair is on disk. A
  socket lost mid-stream only ends the forwarding. The backend iterator closes
  on every exit: that close is what ends the backend's own session when the
  browser hangs up or the preview dies.
  """
  audio = queue.audio()
  events = backend.transcribe(audio, vocabulary=cfg.voice.vocabulary, languages=cfg.voice.languages)
  try:
    async for event in events:
      if event.kind == "final":
        await _archive_then_push_final(websocket, queue, cfg, event, session_id=session_id, backend_id=backend_id)
        continue
      await _push_preview_frame(websocket, {"type": event.kind, "text": event.text})
  except _PreviewSocketLost as exc:
    log.warning("voice_preview_socket_lost", session_id=session_id, backend=backend_id, reason=str(exc))
  except Exception as exc:
    log.warning("voice_preview_failed", session_id=session_id, backend=backend_id, reason=str(exc))
    with suppress(_PreviewSocketLost):
      await _push_preview_frame(websocket, {"type": "error", "message": str(exc)})
  finally:
    await events.aclose()
    await audio.aclose()


async def _archive_then_push_final(
    websocket: WebSocket,
    queue: _PreviewQueue,
    cfg: CharlieBotConfig,
    event: TranscriptEvent,
    *,
    session_id: str,
    backend_id: str,
) -> None:
  """Archive the recording the relay already received, then push the final frame.

  Receiving the final now tells the browser the pair is on disk and no upload
  follows, so the archive runs first and a failed archive replaces the final
  with an error frame — the browser's fallback upload then archives the
  recording. A final that outruns the browser's end frame leaves the recording
  incomplete: that is a backend failure, not an archive.
  """
  if not queue.ended:
    raise _PreviewEarlyFinalError("backend produced its final before the recording's end frame")
  pcm_bytes = queue.recording_pcm()
  try:
    audio_path = await asyncio.to_thread(_archive_voice_pair, cfg, session_id, pcm_bytes, event.text)
  except Exception:
    # A server-side fault, not a backend failure: the traceback is the useful part.
    log.exception("voice_preview_archive_failed", session_id=session_id, backend=backend_id)
    with suppress(_PreviewSocketLost):
      await _push_preview_frame(websocket, {"type": "error", "message": "recording archive failed"})
    return
  _log_voice_transcribed(session_id, audio_path, pcm_bytes, event.text, backend_id, backend_id, queue.devices)
  await _push_preview_frame(websocket, {"type": "final", "text": event.text})


async def _push_preview_frame(websocket: WebSocket, payload: dict) -> None:
  """One JSON frame to the browser; any send failure becomes _PreviewSocketLost."""
  try:
    await websocket.send_json(payload)
  except Exception as exc:
    raise _PreviewSocketLost(str(exc)) from exc


async def _await_cancelled_streamer(streamer: asyncio.Task) -> None:
  """Wait out a cancelled streamer; the cancellation closes the backend iterator."""
  with suppress(asyncio.CancelledError):
    await streamer


async def _settle_sealed_recording(websocket: WebSocket, streamer: asyncio.Task) -> None:
  """Wait for the backend's final and the browser's leaving at once.

  The browser leaving first — or sending any frame past its end frame — cancels
  the streamer: a recording nobody waits for is never archived, and the
  cancellation closes the backend iterator.
  """
  watcher = asyncio.create_task(_watch_sealed_socket(websocket))
  done, _ = await asyncio.wait({streamer, watcher}, return_when=asyncio.FIRST_COMPLETED)
  if streamer in done:
    watcher.cancel()
    with suppress(asyncio.CancelledError):
      await watcher
    return
  streamer.cancel()
  await _await_cancelled_streamer(streamer)
  if watcher.result() == "violation":
    # A frame past the end frame breaks the protocol exactly like one before it.
    with suppress(_PreviewSocketLost):
      await _push_preview_frame(websocket, {"type": "error", "message": "unexpected frame after the recording's end"})
    await _drain_preview_socket(websocket)


async def _watch_sealed_socket(websocket: WebSocket) -> str:
  """Read the sealed recording's socket until the browser leaves or breaks protocol.

  Returns "disconnect" when the browser went away and "violation" when it sent
  any frame, since nothing past the end frame is part of the protocol.
  """
  while True:
    message = await websocket.receive()
    kind = message.get("type")
    if kind == "websocket.disconnect":
      return "disconnect"
    if kind == "websocket.connect":
      continue
    return "violation"


async def _drain_preview_socket(websocket: WebSocket) -> None:
  """Consume and discard frames until the browser closes, so its writes always land."""
  while True:
    message = await websocket.receive()
    if message.get("type") == "websocket.disconnect":
      return


async def _close_preview_socket(websocket: WebSocket) -> None:
  """Close the preview socket; a browser that already hung up is a logged no-op."""
  try:
    await websocket.close()
  except Exception as exc:
    log.debug("voice_preview_close_failed", error=str(exc))


async def _speech_bundle() -> object:
  """The resident speech bundle, built off the event loop on first use."""
  from src.agents import transcriber

  try:
    return await asyncio.to_thread(transcriber.get_transcription_bundle, get_config())
  except transcriber.SpeechModelsNotReadyError as exc:
    raise _VoiceRequestError(503, str(exc)) from exc
  except Exception as exc:
    log.exception("voice_bundle_acquire_failed")
    raise _VoiceRequestError(500, f"speech inference failed: {exc}") from exc


async def _decode_pcm(session_id: str, pcm_bytes: bytes) -> str:
  """Acquire the speech bundle and decode; the one entry the probe endpoint uses."""
  return await _transcribe_with_bundle(session_id, await _speech_bundle(), pcm_bytes)


async def _transcribe_with_bundle(session_id: str, bundle: object, pcm_bytes: bytes) -> str:
  """Offline-decode the PCM; decode-stage failures map to 500, never to a silent error."""
  from src.agents import transcriber

  try:
    return await asyncio.to_thread(transcriber.transcribe_pcm_offline, bundle, pcm_bytes)
  except Exception as exc:
    log.exception("voice_decode_failed", session_id=session_id)
    raise _VoiceRequestError(500, f"speech inference failed: {exc}") from exc


def _voice_stem() -> str:
  """The archive stem both paths publish: UTC timestamp to milliseconds, then 8 hex digits."""
  ts = datetime.now(UTC).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"
  return f"{ts}_{uuid4().hex[:8]}"


def _persist_voice_audio(cfg: CharlieBotConfig, session_id: str, pcm_bytes: bytes) -> Path:
  """Write the fallback upload's recording to sessions/{id}/voice/ and return its path.

  Runs before the decode.
  """
  audio_path = cfg.sessions_dir / session_id / "voice" / f"{_voice_stem()}.wav"
  audio_path.parent.mkdir(parents=True, exist_ok=True)
  _write_wav(audio_path, pcm_bytes)
  return audio_path


def _archive_voice_pair(cfg: CharlieBotConfig, session_id: str, pcm_bytes: bytes, text: str) -> Path:
  """Publish the relay's recording+text pair under sessions/{id}/voice/ and return the wav's path.

  The .txt is written first and the .wav is published by one rename from a
  temporary name, so the *.wav glob that discovers recordings never sees a
  partial pair. Any failure removes what was written and raises.
  """
  voice_dir = cfg.sessions_dir / session_id / "voice"
  voice_dir.mkdir(parents=True, exist_ok=True)
  audio_path = voice_dir / f"{_voice_stem()}.wav"
  partial_path = voice_dir / f"{audio_path.stem}.wav.partial"
  try:
    _write_voice_transcript(audio_path, text)
    _write_wav(partial_path, pcm_bytes)
    os.replace(partial_path, audio_path)
  except BaseException:
    partial_path.unlink(missing_ok=True)
    audio_path.with_suffix(".txt").unlink(missing_ok=True)
    raise
  return audio_path


def _write_voice_transcript(audio_path: Path, transcription: str) -> None:
  audio_path.with_suffix(".txt").write_text(transcription, encoding="utf-8")


def _write_wav(path: Path, pcm_bytes: bytes) -> None:
  # wave rides the write like the transcriber stack rides its provisioning: the
  # M99 server import floor carries no audio-container stack.
  import wave

  from src.agents.transcriber import SAMPLE_RATE

  with wave.open(str(path), "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(SAMPLE_RATE)
    wav.writeframes(pcm_bytes)
