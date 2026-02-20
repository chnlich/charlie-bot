"""Abstract LLM provider interface for CharlieBot."""

from abc import ABC, abstractmethod


class LLMProvider(ABC):
  """Abstract base class for LLM providers."""

  @abstractmethod
  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio bytes to text. Not all providers support this."""

  @abstractmethod
  async def generate_text(self, prompt: str) -> str:
    """Generate text from a prompt."""
