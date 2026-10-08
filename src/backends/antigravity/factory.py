"""The factory of the antigravity backend type (registered in ``src/backends/antigravity/__init__.py``)."""

from typing import Any

from src.backends.antigravity import antigravity_cli
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return antigravity_cli.AntigravityCliBackend(print_timeout=option.print_timeout, **kwargs)
