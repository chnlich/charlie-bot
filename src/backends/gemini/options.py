"""The gemini option model: the ``backends.options[]`` entry of type "gemini"."""

from typing import Literal

from src.infra.backend_models import BackendOption


class GeminiBackend(BackendOption):
  type: Literal["gemini"] = "gemini"
