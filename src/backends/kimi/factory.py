"""The factory of the cc-kimi backend type (registered in ``src/backends/kimi/__init__.py``)."""

from typing import Any

from src.backends.kimi import kimi
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return kimi.KimiBackend(
      api_key=str(config.get_credentials().require(option.credential, "api_key")),
      model=models.option_default_model(option, subject="backend "),
      **kwargs)
