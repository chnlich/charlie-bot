"""The cc-kimi option model: the ``backends.options[]`` entry of type "cc-kimi"."""

from typing import Literal

from src.infra.backend_models import BackendOption


class CcKimiBackend(BackendOption):
  type: Literal["cc-kimi"] = "cc-kimi"
  credential: str
