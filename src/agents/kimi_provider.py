"""Kimi k2.5 LLM provider (fallback) for CharlieBot.

Kimi uses an OpenAI-compatible API endpoint.
"""

from typing import AsyncGenerator

from openai import AsyncOpenAI
import structlog

from src.agents.llm_provider import LLMProvider
from src.core.models import ChatMessage, MessageRole

log = structlog.get_logger()


def _to_openai_messages(messages: list[ChatMessage], system_prompt: str) -> list[dict]:
  """Convert ChatMessage list to OpenAI messages format, prepending system prompt."""
  result: list[dict] = [{"role": "system", "content": system_prompt}]
  for msg in messages:
    result.append({"role": msg.role.value, "content": msg.content})
  return result


class KimiProvider(LLMProvider):
  """Kimi k2.5 via OpenAI-compatible API (fallback provider)."""

  def __init__(self, api_key: str, base_url: str, model: str = "moonshot-v1-8k"):
    self._client = AsyncOpenAI(api_key=api_key, base_url=base_url)
    self._model_id = model

  async def complete(self, messages: list[ChatMessage], system_prompt: str) -> str:
    oai_messages = _to_openai_messages(messages, system_prompt)
    response = await self._client.chat.completions.create(
      model=self._model_id,
      messages=oai_messages,
    )
    return response.choices[0].message.content or ""

  async def complete_streaming(
    self,
    messages: list[ChatMessage],
    system_prompt: str,
  ) -> AsyncGenerator[str, None]:
    oai_messages = _to_openai_messages(messages, system_prompt)
    stream = await self._client.chat.completions.create(
      model=self._model_id,
      messages=oai_messages,
      stream=True,
    )
    async for chunk in stream:
      delta = chunk.choices[0].delta.content
      if delta:
        yield delta

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    raise NotImplementedError("Audio transcription requires the Gemini provider.")

  @property
  def model_name(self) -> str:
    return self._model_id
