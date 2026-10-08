"""The factory of the opencode backend type (registered in ``src/backends/opencode/__init__.py``)."""

from typing import Any

from src.backends.opencode import opencode
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return opencode.OpenCodeBackend(
      model=models.option_default_model(option, subject="backend "), proxy_url=option.proxy_url, **kwargs)
