"""Abstract LLM provider interface for CharlieBot."""

from abc import ABC, abstractmethod
from typing import AsyncGenerator

from src.core.models import ChatMessage


class LLMProvider(ABC):
  """Abstract base class for LLM providers. Enables model swapping without changing call sites."""

  @abstractmethod
  async def complete(self, messages: list[ChatMessage], system_prompt: str) -> str:
    """Return a complete assistant text response."""

  @abstractmethod
  async def complete_streaming(
    self,
    messages: list[ChatMessage],
    system_prompt: str,
  ) -> AsyncGenerator[str, None]:
    """Yield text chunks as an async generator."""

  @abstractmethod
  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio bytes to text. Not all providers support this."""

  @property
  @abstractmethod
  def model_name(self) -> str:
    """Human-readable model identifier."""
