"""KimiBackend — ClaudeCodeBackend configured to use Kimi's Anthropic-compatible endpoint."""

from collections.abc import AsyncIterator
from typing import Awaitable, Callable, Optional

from src.agents.backends.claude_code import ClaudeCodeBackend

_MOONSHOT_BASE_URL = "https://api.moonshot.cn/anthropic"


class KimiBackend(ClaudeCodeBackend):
  """Runs Claude Code CLI against Kimi's Anthropic-compatible endpoint.

  Identical to ClaudeCodeBackend but injects the Moonshot env vars so that
  the ``claude`` binary routes all API calls to api.moonshot.cn instead of
  api.anthropic.com.
  """

  def __init__(
    self,
    api_key: str,
    model: str = "kimi-k2.5",
    extra_flags: Optional[list[str]] = None,
    buffer_limit: Optional[int] = None,
    on_spawn: Optional[Callable[[int], Awaitable[None]]] = None,
  ):
    super().__init__(extra_flags=extra_flags, buffer_limit=buffer_limit, on_spawn=on_spawn)
    self._api_key = api_key
    self._kimi_model = model

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    kimi_env = {
      **env,
      "ANTHROPIC_BASE_URL": _MOONSHOT_BASE_URL,
      "ANTHROPIC_AUTH_TOKEN": self._api_key,
      "ANTHROPIC_MODEL": self._kimi_model,
      "ANTHROPIC_DEFAULT_OPUS_MODEL": self._kimi_model,
      "ANTHROPIC_DEFAULT_SONNET_MODEL": self._kimi_model,
      "ANTHROPIC_DEFAULT_HAIKU_MODEL": self._kimi_model,
      "CLAUDE_CODE_SUBAGENT_MODEL": self._kimi_model,
    }
    async for event in super().run(prompt, cwd, kimi_env):
      yield event
