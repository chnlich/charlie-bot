"""Gemini LLM provider (primary) for CharlieBot."""

import asyncio
from typing import AsyncGenerator

import google.generativeai as genai
import structlog

from src.agents.llm_provider import LLMProvider
from src.core.models import ChatMessage, MessageRole

log = structlog.get_logger()


def _to_gemini_messages(messages: list[ChatMessage]) -> list[dict]:
  """Convert ChatMessage list to Gemini content format."""
  result = []
  for msg in messages:
    role = "model" if msg.role == MessageRole.ASSISTANT else "user"
    result.append({"role": role, "parts": [{"text": msg.content}]})
  return result


class GeminiProvider(LLMProvider):
  """Gemini Flash implementation using google-generativeai SDK."""

  def __init__(self, api_key: str, model: str = "gemini-2.0-flash"):
    genai.configure(api_key=api_key)
    self._model_id = model
    self._model = genai.GenerativeModel(model)

  async def complete(self, messages: list[ChatMessage], system_prompt: str) -> str:
    """Non-streaming completion via asyncio.to_thread (SDK is synchronous)."""
    model = genai.GenerativeModel(self._model_id, system_instruction=system_prompt)
    contents = _to_gemini_messages(messages)

    def _call():
      response = model.generate_content(contents)
      return response.text

    return await asyncio.to_thread(_call)

  async def complete_streaming(
    self,
    messages: list[ChatMessage],
    system_prompt: str,
  ) -> AsyncGenerator[str, None]:
    """Streaming completion — yields text chunks."""
    model = genai.GenerativeModel(self._model_id, system_instruction=system_prompt)
    contents = _to_gemini_messages(messages)

    def _stream():
      return model.generate_content(contents, stream=True)

    stream = await asyncio.to_thread(_stream)
    for chunk in stream:
      if chunk.text:
        yield chunk.text

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio using Gemini's multimodal capabilities."""
    model = genai.GenerativeModel(self._model_id)

    def _call():
      part = {"inline_data": {"data": audio_bytes, "mime_type": mime_type}}
      response = model.generate_content(["Transcribe this audio exactly, word for word.", part])
      return response.text

    return await asyncio.to_thread(_call)

  @property
  def model_name(self) -> str:
    return self._model_id
