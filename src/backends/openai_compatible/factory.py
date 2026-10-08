"""The factory of the cc-openai-compatible backend type (registered in this package's ``__init__.py``)."""

from typing import Any

from src.backends.openai_compatible import openai_compatible_claude
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  proxy_base_url = f"{cfg.server_base_url}/api/anthropic-proxy/openai-compatible/{option.id}"
  return openai_compatible_claude.OpenAICompatibleClaudeBackend(
      proxy_base_url=proxy_base_url,
      auth_token=str(config.get_credentials().require("charliebot", "access_key")),
      model=models.option_default_model(option, subject="backend "),
      **kwargs,
  )
