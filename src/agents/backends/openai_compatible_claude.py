"""OpenAICompatibleClaudeBackend — Claude Code via CharlieBot's Anthropic proxy."""

from src.agents.backends.claude_code import AnthropicEndpointBackend


class OpenAICompatibleClaudeBackend(AnthropicEndpointBackend):
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
    super().__init__(base_url=proxy_base_url.rstrip("/"), auth_token=auth_token, model=model, **kwargs)
