"""The factory of the cc-anthropic-compatible backend type."""

from typing import Any

from src.backends.claude_code import claude_code
from src.infra import config, models
from src.runtime.agent_process import base

NO_AUTH_TOKEN = "no-auth"


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  """Build Claude Code against the option's Anthropic-compatible endpoint."""
  auth_token = (
      NO_AUTH_TOKEN if option.credential is None else str(
          config.get_credentials().require(option.credential, "api_key")))
  return claude_code.AnthropicEndpointBackend(
      base_url=option.api_base.rstrip("/"),
      auth_token=auth_token,
      model=models.option_default_model(option, subject="backend "),
      context_window=option.context_window,
      **kwargs,
  )
