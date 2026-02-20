"""Gemini LLM provider (primary) for CharlieBot."""

import asyncio

import google.generativeai as genai
import structlog

from src.agents.llm_provider import LLMProvider

log = structlog.get_logger()


class GeminiProvider(LLMProvider):
  """Gemini Flash implementation using google-generativeai SDK."""

  def __init__(self, api_key: str, model: str = "gemini-2.0-flash"):
    genai.configure(api_key=api_key)
    self._model_id = model
    self._model = genai.GenerativeModel(model)

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio using Gemini's multimodal capabilities."""
    model = genai.GenerativeModel(self._model_id)

    def _call():
      part = {"inline_data": {"data": audio_bytes, "mime_type": mime_type}}
      response = model.generate_content(["Transcribe this audio exactly, word for word.", part])
      return response.text

    return await asyncio.to_thread(_call)
