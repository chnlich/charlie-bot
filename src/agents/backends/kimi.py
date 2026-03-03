"""KimiBackend — ClaudeCodeBackend configured to use Kimi's Anthropic-compatible endpoint."""

from src.agents.backends.claude_code import ClaudeCodeBackend

_MOONSHOT_BASE_URL = "https://api.moonshot.cn/anthropic"


class KimiBackend(ClaudeCodeBackend):
  """Runs Claude Code CLI against Kimi's Anthropic-compatible endpoint.

  Identical to ClaudeCodeBackend but injects the Moonshot env vars so that
  the ``claude`` binary routes all API calls to api.moonshot.cn instead of
  api.anthropic.com.
  """

  def __init__(self, *, api_key: str, **kwargs):
    # Pop model before super().__init__ — Kimi sets model via ANTHROPIC_MODEL
    # env var, NOT --model CLI flag.
    self._kimi_model = kwargs.pop("model", "kimi-k2.5")
    self._api_key = api_key
    super().__init__(model=None, **kwargs)

  def _prepare_env(self, env: dict) -> dict:
    return {
        **env,
        "ANTHROPIC_BASE_URL": _MOONSHOT_BASE_URL,
        "ANTHROPIC_AUTH_TOKEN": self._api_key,
        "ANTHROPIC_MODEL": self._kimi_model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": self._kimi_model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": self._kimi_model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": self._kimi_model,
        "CLAUDE_CODE_SUBAGENT_MODEL": self._kimi_model,
    }
