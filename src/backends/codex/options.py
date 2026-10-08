"""The codex option model: the ``backends.options[]`` entry of type "codex"."""

from typing import Literal

from pydantic import Field

from src.infra.backend_models import BackendOption


class CodexBackend(BackendOption):
  type: Literal["codex"] = "codex"
  model_reasoning_effort: str | None = None  # per-backend reasoning effort override
  model_auto_compact_token_limit: int | None = Field(default=None, gt=0)  # per-backend auto-compact token limit
