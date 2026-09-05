"""KimiBackend — Claude Code against Kimi's Anthropic-compatible endpoint."""

from src.agents.backends.claude_code import AnthropicEndpointBackend

_MOONSHOT_BASE_URL = "https://api.moonshot.cn/anthropic"


class KimiBackend(AnthropicEndpointBackend):
  """Runs Claude Code CLI against Kimi's Anthropic-compatible endpoint.

  The Moonshot base URL routes every API call to api.moonshot.cn instead of
  api.anthropic.com; the model rides the env vars, not a ``--model`` flag.
  """

  def __init__(self, *, api_key: str, model: str, **kwargs):
    if not model:
      raise ValueError("kimi backend requires a model")
    super().__init__(base_url=_MOONSHOT_BASE_URL, auth_token=api_key, model=model, **kwargs)
