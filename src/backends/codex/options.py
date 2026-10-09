"""The codex option model: the ``backends.options[]`` entry of type "codex"."""

from typing import Literal

import pydantic

from src.infra import backend_models


class CodexBackend(backend_models.BackendOption):
  type: Literal["codex"] = "codex"
  model_reasoning_effort: str | None = None  # per-backend reasoning effort override
  model_auto_compact_token_limit: int | None = pydantic.Field(
      default=None, gt=0)  # per-backend auto-compact token limit
