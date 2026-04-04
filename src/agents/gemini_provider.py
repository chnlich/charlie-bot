"""Gemini LLM provider (primary) for CharlieBot."""

import asyncio

import structlog
from google import genai
from google.genai import types

from src.agents.llm_provider import LLMProvider

log = structlog.get_logger()


class GeminiProvider(LLMProvider):
  """Gemini Flash implementation using google-genai SDK."""

  def __init__(self, api_key: str, model: str = "gemini-2.0-flash"):
    self._client = genai.Client(api_key=api_key)
    self._model_id = model

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str, custom_words: list[str] | None = None) -> str:
    """Transcribe audio using Gemini's multimodal capabilities."""

    def _call():
      prompt = (
          "Transcribe this audio exactly, word for word. "
          "The speaker may use English, Chinese, or mix both languages — preserve the original language(s) used. Do not translate. "
          "For Chinese, always output simplified Chinese (简体字), never traditional Chinese.")
      if custom_words:
        prompt += " Pay special attention to these terms and spell them exactly: " + ", ".join(custom_words) + "."
      response = self._client.models.generate_content(
          model=self._model_id,
          contents=[prompt, types.Part.from_bytes(data=audio_bytes, mime_type=mime_type)],
      )
      return response.text

    return await asyncio.to_thread(_call)

  async def generate_text(self, prompt: str) -> str:
    """Generate text using Gemini."""

    def _call():
      response = self._client.models.generate_content(
          model=self._model_id,
          contents=prompt,
      )
      return response.text

    return await asyncio.to_thread(_call)
