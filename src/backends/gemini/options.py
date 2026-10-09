"""The gemini option model: the ``backends.options[]`` entry of type "gemini"."""

from typing import Literal

from src.infra import backend_models


class GeminiBackend(backend_models.BackendOption):
  type: Literal["gemini"] = "gemini"
