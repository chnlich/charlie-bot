"""The antigravity option model: the ``backends.options[]`` entry of type "antigravity"."""

from typing import ClassVar, Literal

from src.infra import backend_models


class AntigravityBackend(backend_models.BackendOption):
  type: Literal["antigravity"] = "antigravity"
  # The agy command takes no model, so an entry may omit ``model`` and the runtime routes it with none.
  model_optional: ClassVar[bool] = True
  print_timeout: str | None = None  # antigravity only: agy --print turn budget (Go duration, e.g. "1h")
