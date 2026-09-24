"""Voice-input endpoints: record-then-upload transcription plus the preview relay.

The browser records locally and POSTs a 16 kHz mono PCM16 WAV: one probe request
for the opening clip (recognition confirmation) and one full-upload request on
release. The full upload persists the recording BEFORE decoding it, so a decode
failure or an abandoned request never loses the audio. When the preview relay
(/ws/voice/{session_id}) already transcribed the recording, the upload carries
the transcript and the decode is skipped. The relay itself is transport only:
it forwards a streaming backend's partials while speaking, persists nothing,
and never falls back — the upload endpoint owns recording, persistence, and
decode on every path.
"""

from __future__ import annotations

import asyncio
import io
import json
from collections.abc import AsyncIterator
from contextlib import suppress
from datetime import UTC, datetime
from pathlib import Path
from uuid import uuid4

from fastapi import APIRouter, Form, Request, UploadFile, WebSocket

from src.agents.transcription.base import TranscriptionBackend
from src.agents.transcription.registry import build_transcription_backend
from src.api.responses import FastJsonResponse
from src.core.config import CharlieBotConfig, get_config
from src.core.log_once import LazyStructlogLogger

log = LazyStructlogLogger()

router = APIRouter()

# The recognition probe decodes the recording's opening clip only; the full upload
# takes a whole dictation and its cap is the transcriber's MAX_RECORDING_SAMPLES.
CONFIRM_MAX_SECONDS = 10


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
    transcript: str | None = Form(None),
    backend: str | None = Form(None),
) -> FastJsonResponse:
  """Persist the full recording first, then decode it offline and return the text.

  Multipart form: ``audio`` is the WAV (same validation and size cap as the raw
  body ever enforced), ``transcript`` is optional — its presence means the
  preview relay already transcribed this recording, so the .txt is written
  verbatim, the decode is skipped, and no speech-model readiness is needed — and
  ``backend`` names the transcription backend for the voice_transcribed log line.
  """
  try:
    pcm_bytes = _wav_body_to_pcm(await audio.read(), _full_max_samples())
    if transcript is None:
      # The bundle comes first so models-not-ready (503) persists nothing — the client
      # keeps its buffer and retries, and no orphan wav piles up per retry.
      bundle = await _speech_bundle()
      audio_path = await asyncio.to_thread(_persist_voice_audio, get_config(), session_id, pcm_bytes)
      text = await _transcribe_with_bundle(session_id, bundle, pcm_bytes)
    else:
      audio_path = await asyncio.to_thread(_persist_voice_audio, get_config(), session_id, pcm_bytes)
      text = transcript
  except _VoiceRequestError as exc:
    # A decode failure (500) leaves the wav on disk: persist-before-decode means the
    # recording survives every later failure.
    return _error_response(exc)
  await asyncio.to_thread(_write_voice_transcript, audio_path, text)
  log.info(
      "voice_transcribed",
      session_id=session_id,
      audio_path=str(audio_path),
      audio_bytes_size=len(pcm_bytes),
      transcription_length=len(text),
      transcription_preview=text[:80],
      backend=backend,
  )
  return FastJsonResponse({"text": text})


def _error_response(exc: _VoiceRequestError) -> FastJsonResponse:
  return FastJsonResponse({"error": exc.message}, status_code=exc.status_code)


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
# Transport only. The relay buffers the browser's audio frames in a bounded
# queue, feeds that queue to the selected backend's transcribe as its audio
# iterator, and forwards the resulting events as they arrive. It persists
# nothing, decodes nothing, and has no fallback: the upload endpoint owns the
# recording, the persistence, and the decode on every path.

PREVIEW_QUEUE_SECONDS = 30


class _PreviewSocketLost(Exception):  # noqa: N818  a lost socket is a state, not a fault name
  """The preview socket died mid-relay; no further frame can go out."""


class _PreviewProtocolError(Exception):  # noqa: N818  names the protocol breach, not a fault
  """The browser sent a frame the preview protocol does not define."""


class _PreviewQueue:
  """The relay's one buffer between the browser and the backend: 30 s of audio.

  The receive loop's only job is filling this; the buffer itself is the audio
  iterator handed to the backend, so a slow backend never blocks the browser and
  an overflow ends the preview with an error while the recording keeps uploading
  through the HTTP endpoint.
  """

  def __init__(self, max_bytes: int) -> None:
    self._frames: asyncio.Queue[bytes | None] = asyncio.Queue()
    self._max_bytes = max_bytes
    self._queued_bytes = 0
    self._ended = False

  def put(self, frame: bytes) -> bool:
    """Buffer one audio frame; False when the frame would exceed the budget."""
    if self._ended or self._queued_bytes + len(frame) > self._max_bytes:
      return False
    self._frames.put_nowait(frame)
    self._queued_bytes += len(frame)
    return True

  def end_audio(self) -> None:
    """Seal the recording: the audio iterator ends after the buffered frames."""
    self._frames.put_nowait(None)
    self._ended = True

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


async def voice_preview_relay(websocket: WebSocket, session_id: str, backend_id: str) -> None:
  """One recording's live preview: stream the selected backend's partials.

  Auth happens in server.py next to /ws/sessions. The browser sends binary
  16 kHz PCM16 chunks from the start of the recording and a text
  ``{"type":"end"}`` on stop; the relay answers with partial, final, and error
  frames. A refusal (unknown id, missing credential, no live partials) is one
  error frame and a close. Any backend failure is one error frame plus the
  voice_preview_failed log line, carrying the backend id and the reason and
  never the credential. An overflow ends the preview with an error while the
  socket keeps draining until the browser closes. Closing the socket from
  either side closes the backend iterator, which closes the backend's own
  connection.
  """
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
  queue = _PreviewQueue(_preview_queue_budget_bytes())
  streamer = asyncio.create_task(
      _stream_preview_events(websocket, backend, queue, cfg, session_id=session_id, backend_id=backend_id))
  try:
    try:
      outcome, error_message = await _receive_preview_frames(websocket, queue)
    except _PreviewProtocolError as exc:
      outcome, error_message = "protocol", str(exc)
    if outcome == "ended":
      # The final rides out through the streamer; the socket closes after it.
      await streamer
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
    if _is_preview_end_frame(message.get("text")):
      queue.end_audio()
      return "ended", None
    raise _PreviewProtocolError(f"unsupported preview control frame {message.get('text')!r}")


def _is_preview_end_frame(text: object) -> bool:
  """True for the one control frame the protocol defines: ``{"type": "end"}``.

  Anything else — malformed JSON, a foreign type — is a _PreviewProtocolError:
  the browser's own client never sends it.
  """
  try:
    control = json.loads(text)  # type: ignore[arg-type]
  except (TypeError, ValueError) as exc:
    raise _PreviewProtocolError(f"malformed preview control frame {text!r}") from exc
  if isinstance(control, dict) and control.get("type") == "end":
    return True
  raise _PreviewProtocolError(f"unsupported preview control frame {text!r}")


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
  failure reason and never the credential. A socket lost mid-stream only ends
  the forwarding. The backend iterator closes on every exit: that close is what
  ends the backend's own session when the browser hangs up or the preview dies.
  """
  audio = queue.audio()
  events = backend.transcribe(audio, vocabulary=cfg.voice.vocabulary, languages=cfg.voice.languages)
  try:
    async for event in events:
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


def _persist_voice_audio(cfg: CharlieBotConfig, session_id: str, pcm_bytes: bytes) -> Path:
  """Write the uploaded recording to sessions/{id}/voice/ and return its path.

  Runs before the decode.
  """
  ts = datetime.now(UTC).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"
  stem = f"{ts}_{uuid4().hex[:8]}"
  audio_path = cfg.sessions_dir / session_id / "voice" / f"{stem}.wav"
  audio_path.parent.mkdir(parents=True, exist_ok=True)
  _write_wav(audio_path, pcm_bytes)
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
