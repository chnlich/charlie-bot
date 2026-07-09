"""Abstract LLM provider interface for CharlieBot."""

from abc import ABC, abstractmethod


class LLMProvider(ABC):
  """Abstract base class for LLM providers."""

  @abstractmethod
  async def generate_text(self, prompt: str) -> str:
    """Generate text from a prompt."""
