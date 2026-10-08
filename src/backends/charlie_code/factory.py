"""The factory of the charlie-code backend type (registered in ``src/backends/charlie_code/__init__.py``)."""

from typing import Any

from src.backends.charlie_code import charlie_code
from src.infra import config, models
from src.runtime.agent_process import base


def build(option: models.BackendOption, cfg: config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
  return charlie_code.CharlieCodeBackend(
      model=models.option_default_model(option, subject="backend "),
      api_base=option.api_base,
      context_window=option.context_window,
      image_input=option.image_input,
      stream=option.stream,
      timeout_seconds=option.timeout_seconds,
      top_p=option.top_p,
      temperature=option.temperature,
      proxy_url=option.proxy_url,
      api_key=str(config.get_credentials().require(option.credential, "api_key")) if option.credential else None,
      **kwargs)
