"""The cc-anthropic-compatible option model."""

from typing import Literal

import pydantic

from src.infra import backend_models


class CcAnthropicCompatibleBackend(backend_models.BackendOption):
  """An Anthropic-compatible endpoint and its declared context window."""
  type: Literal["cc-anthropic-compatible"] = "cc-anthropic-compatible"
  api_base: str = pydantic.Field(min_length=1)
  context_window: int = pydantic.Field(gt=0)
  credential: str | None = None

  @pydantic.field_validator("api_base")
  @classmethod
  def api_base_is_root(cls, value: str) -> str:
    """Reject an API version suffix because Claude Code appends ``/v1/messages``."""
    if value.rstrip("/").endswith("/v1"):
      raise ValueError("api_base must be the endpoint root without /v1")
    return value
