"""The factory of the codex backend type (registered in ``src/backends/codex/__init__.py``)."""

from typing import Any

from src.backends.codex import codex
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return codex.CodexBackend(
      model=models.option_default_model(option, subject="backend "),
      model_reasoning_effort=option.model_reasoning_effort,
      model_auto_compact_token_limit=option.model_auto_compact_token_limit,
      **kwargs)
