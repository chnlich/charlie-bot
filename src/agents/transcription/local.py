"""Local transcription backend: the resident bundle decoding the finished recording offline.

Wraps src/agents/transcriber.py — the engine choice (``voice.engine``), the
bundle cache, and the GPU fallback stay there (plan Trade-off 1: one local
backend following ``voice.engine``). The output is the upload endpoint's decode
for the same bytes: ``transcribe_pcm_offline`` on the resident bundle.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Sequence

from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend
from src.core.config import CharlieBotConfig


class LocalTranscriptionBackend(TranscriptionBackend):
  """The on-device engine. No partials: one final after the whole clip decodes."""

  id = "local"
  live_partials = False

  def __init__(self, cfg: CharlieBotConfig, hotwords: str = "") -> None:
    self._cfg = cfg
    # Empty in production: the resident bundle follows voice.engine. The replay
    # script's local-hotwords variant passes sherpa hotwords, which builds a
    # dedicated CPU bundle instead of using the resident one.
    self._hotwords = hotwords
    self._hotwords_bundle: object | None = None

  @property
  def label(self) -> str:
    return f"Local ({self._cfg.voice.engine})"

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[TranscriptEvent]:
    """Drain ``audio`` and decode the whole recording in one pass.

    ``vocabulary`` is ignored: the local engine's word biasing is sherpa's
    hotwords parameter, a constructor argument, not a per-recording one.
    """
    from src.agents import transcriber

    pcm = bytearray()
    async for chunk in audio:
      pcm.extend(chunk)
    if self._hotwords:
      text = await asyncio.to_thread(self._decode_with_hotwords, bytes(pcm))
    else:
      bundle = await asyncio.to_thread(transcriber.get_transcription_bundle, self._cfg)
      text = await asyncio.to_thread(transcriber.transcribe_pcm_offline, bundle, bytes(pcm))
    yield TranscriptEvent(kind="final", text=text)

  def _decode_with_hotwords(self, pcm: bytes) -> str:
    """Decode on a dedicated sherpa bundle carrying the hotwords, built once."""
    from src.agents import transcriber

    if self._hotwords_bundle is None:
      paths = transcriber._ensure_sherpa_paths_cached(self._cfg)
      bundle = transcriber.create_sherpa_bundle(paths, hotwords=self._hotwords)
      transcriber.warm_up_bundle(bundle)
      self._hotwords_bundle = bundle
    return transcriber.transcribe_pcm_offline(self._hotwords_bundle, pcm)
