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

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str, custom_words: list[str] | None = None) -> str:
    """Transcribe audio using Gemini's multimodal capabilities."""
    model = genai.GenerativeModel(self._model_id)

    def _call():
      part = {"inline_data": {"data": audio_bytes, "mime_type": mime_type}}
      prompt = (
        "Transcribe this audio exactly, word for word. "
        "The speaker may use English, Chinese, or mix both languages — preserve the original language(s) used. Do not translate. "
        "For Chinese, always output simplified Chinese (简体字), never traditional Chinese."
      )
      if custom_words:
        prompt += " Pay special attention to these terms and spell them exactly: " + ", ".join(custom_words) + "."
      response = model.generate_content([prompt, part])
      return response.text

    return await asyncio.to_thread(_call)

  async def generate_text(self, prompt: str) -> str:
    """Generate text using Gemini."""
    model = genai.GenerativeModel(self._model_id)

    def _call():
      response = model.generate_content(prompt)
      return response.text

    return await asyncio.to_thread(_call)
