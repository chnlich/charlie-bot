"""The antigravity option model: the ``backends.options[]`` entry of type "antigravity"."""

from typing import Literal

from src.infra import backend_models


class AntigravityBackend(backend_models.BackendOption):
  type: Literal["antigravity"] = "antigravity"
  print_timeout: str | None = None  # antigravity only: agy --print turn budget (Go duration, e.g. "1h")
