"""KimiBackend — ClaudeCodeBackend configured to use Kimi's Anthropic-compatible endpoint."""

from src.agents.backends.claude_code import ClaudeCodeBackend, claude_model_env

_MOONSHOT_BASE_URL = "https://api.moonshot.cn/anthropic"


class KimiBackend(ClaudeCodeBackend):
  """Runs Claude Code CLI against Kimi's Anthropic-compatible endpoint.

  Identical to ClaudeCodeBackend but injects the Moonshot env vars so that
  the ``claude`` binary routes all API calls to api.moonshot.cn instead of
  api.anthropic.com.
  """

  def __init__(self, *, api_key: str, model: str, **kwargs):
    if not model:
      raise ValueError("kimi backend requires a model")
    self._api_key = api_key
    self._env_model = model
    # Kimi sets model via ANTHROPIC_MODEL env var, not --model CLI flag.
    super().__init__(model=None, **kwargs)

  def _prepare_env(self, env: dict) -> dict:
    return {
        **super()._prepare_env(env),
        "ANTHROPIC_BASE_URL": _MOONSHOT_BASE_URL,
        "ANTHROPIC_AUTH_TOKEN": self._api_key,
        **claude_model_env(self._env_model),
    }
