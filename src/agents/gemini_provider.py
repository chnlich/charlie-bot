"""Gemini LLM provider (primary) for CharlieBot."""

import structlog
from google import genai

from src.agents.llm_provider import LLMProvider

log = structlog.get_logger()


class GeminiProvider(LLMProvider):
  """Gemini Flash implementation using google-genai SDK."""

  def __init__(self, api_key: str, model: str = "gemini-2.0-flash"):
    self._client = genai.Client(api_key=api_key)
    self._model_id = model

  async def generate_text(self, prompt: str) -> str:
    """Generate text using Gemini."""
    import asyncio

    def _call():
      response = self._client.models.generate_content(
          model=self._model_id,
          contents=prompt,
      )
      return response.text

    return await asyncio.to_thread(_call)
