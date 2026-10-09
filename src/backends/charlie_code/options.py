"""The charlie-code option model: the ``backends.options[]`` entry of type "charlie-code"."""

from typing import Literal

import pydantic

from src.infra import backend_models


class CharlieCodeBackend(backend_models.BackendOption):
  type: Literal["charlie-code"] = "charlie-code"
  api_base: str | None = None  # OpenAI-compatible base URL
  context_window: int | None = pydantic.Field(
      default=None, gt=0)  # compaction context window in tokens (None = charlie-code default)
  credential: str | None = None
  proxy_url: str | None = None  # per-entry HTTP/HTTPS proxy URL injected into the child env
  # entries accept image attachments (sent as --image) by default; false
  # refuses them (set it on text-only endpoints)
  image_input: bool = True
  stream: bool = True  # endpoint is called in streaming mode (default); false emits --no-stream
  timeout_seconds: int | None = pydantic.Field(
      default=None,
      gt=0)  # call budget: silence bound when streaming, whole-call bound when not (None = charlie-code default)
  top_p: float | None = pydantic.Field(default=None, gt=0.0, le=1.0)  # nucleus cutoff (None = charlie-code default)
  temperature: float | None = pydantic.Field(default=None, ge=0.0)  # sampling temperature (None = charlie-code default)
