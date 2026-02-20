"""Audio transcription helper — uses Gemini provider for voice messages."""

from src.agents.gemini_provider import GeminiProvider
from src.core.config import CharliBotConfig


class AudioTranscriber:
  """Wraps Gemini for audio transcription (voice messages)."""

  def __init__(self, cfg: CharliBotConfig):
    self._provider = GeminiProvider(cfg.gemini_api_key, cfg.gemini_model)

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio bytes using Gemini."""
    return await self._provider.transcribe_audio(audio_bytes, mime_type)
