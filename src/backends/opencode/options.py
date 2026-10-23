"""The opencode option model: the ``backends.options[]`` entry of type "opencode"."""

from collections.abc import Iterable
from typing import Literal

import pydantic

from src.infra import backend_models


class OpencodeBackend(backend_models.BackendOption):
  type: Literal["opencode"] = "opencode"
  proxy_url: str | None = None  # per-backend HTTP/HTTPS proxy URL
  context_limit: int | None = pydantic.Field(default=None, gt=0)
  # Top-level fields added to every request body this model sends; the default adds none.
  extra_body: dict[str, pydantic.JsonValue] = pydantic.Field(default_factory=dict)


def model_overrides_config(backend_options: Iterable[backend_models.BackendOption]) -> dict:
  """The opencode config that overrides each named model's context limit and request-body fields.

  An option that sets neither contributes nothing. Options for one model must agree on each field
  they both set; a disagreement raises ValueError naming both options.
  """
  providers: dict = {}
  declared: dict[tuple[str, str, str], tuple[str, object]] = {}
  for option in backend_options:
    if not isinstance(option, OpencodeBackend) or (option.context_limit is None and not option.extra_body):
      continue
    provider, model = backend_models.option_default_model(option, subject="backend ").split("/", 1)
    entry = providers.setdefault(provider, {"models": {}})["models"].setdefault(model, {})
    if option.context_limit is not None:
      _declare(declared, option.id, provider, model, "context_limit", option.context_limit)
      entry["limit"] = {"context": option.context_limit}
    if option.extra_body:
      _declare(declared, option.id, provider, model, "extra_body", option.extra_body)
      entry["body"] = option.extra_body
  return {"providers": providers} if providers else {}


def _declare(
    declared: dict[tuple[str, str, str], tuple[str, object]], option_id: str, provider: str, model: str, field: str,
    value: object) -> None:
  """Record the first option's value for one field of one model; a later, different value raises ValueError."""
  previous_id, previous_value = declared.setdefault((provider, model, field), (option_id, value))
  if previous_value != value:
    raise ValueError(
        f"opencode options '{previous_id}' and '{option_id}' declare conflicting {field} values "
        f"for {provider}/{model}: {previous_value} and {value}")
