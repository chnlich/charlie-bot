"""The opencode option model: the ``backends.options[]`` entry of type "opencode"."""

from typing import Literal

from src.infra.backend_models import BackendOption


class OpencodeBackend(BackendOption):
  type: Literal["opencode"] = "opencode"
  proxy_url: str | None = None  # per-backend HTTP/HTTPS proxy URL
