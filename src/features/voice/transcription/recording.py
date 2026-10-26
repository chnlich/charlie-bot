"""Transcribe one finished recording through an online backend.

The upload endpoint (src/features/voice/api.py) and the voice-notes skill
(skills/voice-notes/scripts/decode_audio.py) both call this module, so the
selection rules and the chunking live in one place. The recording's PCM is cut
into the capture worklet's chunks, the cadence a live recording's audio carries,
and the backend's final event is the transcript.
"""

from __future__ import annotations

import dataclasses
from collections.abc import AsyncIterator
from typing import TYPE_CHECKING

from src.features.voice.transcription import base, registry

if TYPE_CHECKING:
  from src.infra import config


class TranscriptionFailedError(RuntimeError):
  """The selected backend raised, or ended its session without a final event."""


@dataclasses.dataclass(frozen=True)
class RecordingTranscript:
  """The transcript of one recording and the id of the backend that produced it."""

  backend_id: str
  text: str


def select_backend(backend_id: str | None, cfg: config.CharlieBotConfig) -> base.TranscriptionBackend:
  """The backend a recording goes to: the named one, or ``voice.default_backend`` when no id is named.

  Raises base.BackendSelectionError for an id the registry does not know and for a
  backend whose unavailable_reason is set.
  """
  chosen = cfg.voice.default_backend if backend_id is None else backend_id
  backend = registry.build_transcription_backend(chosen, cfg)
  reason = backend.unavailable_reason()
  if reason is not None:
    raise base.BackendSelectionError(f"{chosen} is unavailable: {reason}")
  return backend


async def transcribe_recording(
    pcm_bytes: bytes, backend_id: str | None, cfg: config.CharlieBotConfig) -> RecordingTranscript:
  """Transcribe the whole recording (16 kHz mono PCM16) through the selected backend.

  Raises base.BackendSelectionError before any audio is sent, and TranscriptionFailedError
  when the backend fails during the recording.
  """
  backend = select_backend(backend_id, cfg)
  final: str | None = None
  try:
    async for event in backend.transcribe(_recording_chunks(pcm_bytes), vocabulary=cfg.voice.vocabulary,
                                          languages=cfg.voice.languages):
      if event.kind == "final":
        final = event.text
  except Exception as exc:
    raise TranscriptionFailedError(f"voice transcription failed: {exc}") from exc
  if final is None:
    raise TranscriptionFailedError(f"voice transcription failed: {backend.id} produced no final event")
  return RecordingTranscript(backend_id=backend.id, text=final)


async def _recording_chunks(pcm_bytes: bytes) -> AsyncIterator[bytes]:
  """The recording cut into the worklet's chunks: the audio a live backend receives."""
  frame_bytes = base.VOICE_CHUNK_SAMPLES * 2  # PCM16: two bytes per sample
  for offset in range(0, len(pcm_bytes), frame_bytes):
    yield pcm_bytes[offset:offset + frame_bytes]
