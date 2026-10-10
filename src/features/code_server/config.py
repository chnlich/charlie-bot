"""The ``code_server:`` config section model and the values read from the code-server config file it names."""

import pathlib
from typing import TYPE_CHECKING

import pydantic

from src.infra import yaml_utils

if TYPE_CHECKING:
  from src.infra import config


class CodeServerConfig(pydantic.BaseModel):
  """``code_server:`` section: code-server integration."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # code-server integration
  bin: str | None = None
  config: str = "configs/code-server.yaml"


def code_server_config_path(cfg: config.CharlieBotConfig) -> pathlib.Path:
  path = pathlib.Path(cfg.code_server.config).expanduser()
  if path.is_absolute():
    return path
  return cfg.charlie_bot_repo / path


def code_server_listen_port(cfg: config.CharlieBotConfig) -> int:
  config_path = code_server_config_path(cfg)
  data = yaml_utils.load_yaml(config_path, default={})
  if not isinstance(data, dict):
    raise ValueError(f"code-server config must be a YAML mapping: {config_path}")
  bind_addr = data.get("bind-addr")
  if not isinstance(bind_addr, str) or ":" not in bind_addr:
    raise ValueError(f"code-server config must define bind-addr: {config_path}")
  port_text = bind_addr.rsplit(":", 1)[1]
  try:
    return int(port_text)
  except ValueError as exc:
    raise ValueError(f"code-server bind-addr port must be an integer: {bind_addr}") from exc
