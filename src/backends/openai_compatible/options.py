"""The cc-openai-compatible option model: the ``backends.options[]`` entry of type "cc-openai-compatible"."""

from typing import Literal

from src.infra import backend_models


class CcOpenAICompatibleBackend(backend_models.BackendOption):
  type: Literal["cc-openai-compatible"] = "cc-openai-compatible"
  api_base: str  # OpenAI-compatible base URL
  credential: str | None = None
