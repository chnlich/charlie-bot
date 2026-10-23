"""The opencode option model: the ``backends.options[]`` entry of type "opencode"."""

from collections.abc import Iterable
from typing import Literal

import pydantic

from src.infra import backend_models


class OpencodeBackend(backend_models.BackendOption):
  type: Literal["opencode"] = "opencode"
  proxy_url: str | None = None  # per-backend HTTP/HTTPS proxy URL
  context_limit: int | None = pydantic.Field(default=None, gt=0)


def model_context_config(backend_options: Iterable[backend_models.BackendOption]) -> dict:
  """The opencode v2 context overrides; conflicting limits for one model raise ValueError."""
  providers: dict = {}
  declared: dict[tuple[str, str], OpencodeBackend] = {}
  for option in backend_options:
    if not isinstance(option, OpencodeBackend) or option.context_limit is None:
      continue
    provider, model = backend_models.option_default_model(option, subject="backend ").split("/", 1)
    key = (provider, model)
    previous = declared.get(key)
    if previous is not None and previous.context_limit != option.context_limit:
      raise ValueError(
          f"opencode options '{previous.id}' and '{option.id}' declare conflicting context_limit values "
          f"for {provider}/{model}: {previous.context_limit} and {option.context_limit}")
    declared[key] = option
    models = providers.setdefault(provider, {"models": {}})["models"]
    models[model] = {"limit": {"context": option.context_limit}}
  return {"providers": providers} if providers else {}
