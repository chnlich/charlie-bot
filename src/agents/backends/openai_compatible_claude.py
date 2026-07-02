"""OpenAICompatibleClaudeBackend — Claude Code via CharlieBot's Anthropic proxy."""

from src.agents.backends.claude_code import ClaudeCodeBackend


class OpenAICompatibleClaudeBackend(ClaudeCodeBackend):
  """Runs Claude Code against CharlieBot's Anthropic-to-OpenAI-compatible proxy.

  The upstream OpenAI-compatible endpoint is resolved per backend id by the
  proxy route; this backend only points Claude Code at the proxy and declares
  the Claude-facing model.
  """

  def __init__(self, *, proxy_base_url: str, auth_token: str, model: str, **kwargs):
    if not proxy_base_url:
      raise ValueError("cc-openai-compatible backend requires proxy_base_url")
    if not auth_token:
      raise ValueError("cc-openai-compatible backend requires charliebot_access_key for proxy auth")
    if not model:
      raise ValueError("cc-openai-compatible backend requires a model")
    self._proxy_base_url = proxy_base_url.rstrip("/")
    self._auth_token = auth_token
    self._env_model = model
    super().__init__(model=None, **kwargs)

  def _prepare_env(self, env: dict) -> dict:
    return {
        **super()._prepare_env(env),
        "ANTHROPIC_BASE_URL": self._proxy_base_url,
        "ANTHROPIC_AUTH_TOKEN": self._auth_token,
        "ANTHROPIC_MODEL": self._env_model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": self._env_model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": self._env_model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": self._env_model,
        "CLAUDE_CODE_SUBAGENT_MODEL": self._env_model,
    }
