"""Voice-input HTTP endpoints: record-then-upload transcription.

The browser records locally and POSTs a 16 kHz mono PCM16 WAV: one probe request
for the opening clip (recognition confirmation) and one full-upload request on
release. The full upload persists the recording BEFORE decoding it, so a decode
failure or an abandoned request never loses the audio.
"""

from __future__ import annotations

import asyncio
import io
from datetime import UTC, datetime
from pathlib import Path
from uuid import uuid4

from fastapi import APIRouter, Request

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
async def upload_voice_recording(request: Request, session_id: str) -> FastJsonResponse:
  """Persist the full recording first, then decode it offline and return the text."""
  try:
    pcm_bytes = _wav_body_to_pcm(await request.body(), _full_max_samples())
    # The bundle comes first so models-not-ready (503) persists nothing — the client
    # keeps its buffer and retries, and no orphan wav piles up per retry.
    bundle = await _speech_bundle()
    audio_path = await asyncio.to_thread(_persist_voice_audio, get_config(), session_id, pcm_bytes)
    text = await _transcribe_with_bundle(session_id, bundle, pcm_bytes)
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
  """Parse the request body as 16 kHz mono PCM16 WAV and return its PCM frames.

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

  Same layout the streaming persistence used; runs before the decode.
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
