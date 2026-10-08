"""The ``code_server:`` config section model and the values read from the code-server config file it names."""

from pathlib import Path
from typing import TYPE_CHECKING

from pydantic import BaseModel, ConfigDict

from src.infra.yaml_utils import load_yaml

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig


class CodeServerConfig(BaseModel):
  """``code_server:`` section: code-server integration."""

  model_config = ConfigDict(extra='forbid')

  # code-server integration
  bin: str | None = None
  config: str = "configs/code-server.yaml"


def code_server_config_path(cfg: CharlieBotConfig) -> Path:
  path = Path(cfg.code_server.config).expanduser()
  if path.is_absolute():
    return path
  return cfg.charlie_bot_repo / path


def code_server_listen_port(cfg: CharlieBotConfig) -> int:
  config_path = code_server_config_path(cfg)
  data = load_yaml(config_path, default={})
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
