"""The opencode option model: the ``backends.options[]`` entry of type "opencode"."""

from typing import Literal

from src.infra import backend_models


class OpencodeBackend(backend_models.BackendOption):
  type: Literal["opencode"] = "opencode"
  proxy_url: str | None = None  # per-backend HTTP/HTTPS proxy URL
