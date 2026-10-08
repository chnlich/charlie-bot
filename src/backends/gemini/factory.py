"""The factory of the gemini backend type (registered in ``src/backends/gemini/__init__.py``)."""

from typing import Any

from src.backends.gemini import gemini_cli
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return gemini_cli.GeminiCliBackend(model=models.option_default_model(option, subject="backend "), **kwargs)
