"""The backend-option base model the config schema builds its ``backends.options`` field from.

Each backend package defines the option model of its type in its own ``options`` module and
registers it with ``src.runtime.hooks.backend_types``; ``src.infra.config_registry`` resolves a
``type`` string to that class. ``src/infra/config`` imports this module directly so a config read
(every CLI invocation's first ``get_config``) never constructs the session/API models in
``src.infra.models``; that module re-exports the names below for its established import path.
"""

from collections.abc import Mapping
from typing import Any

import pydantic

from src.infra import config_registry


class BackendOption(pydantic.BaseModel):
  """Fields every backend option carries; each type's own fields live on the subclass its package registers.

  A subclass narrows ``type`` to ``Literal[<its backend type>]``. A config entry validates against
  the subclass its ``type`` names, so illegal field/type combinations are unconstructable.
  """
  model_config = pydantic.ConfigDict(extra='forbid')

  id: str
  label: str
  model: str | None = None
  # Overlay filename (no .md) under prompts/model_overlays/. Literal "none" =
  # explicitly fenceless (silent); None = undeclared; a declared-but-unreadable
  # file degrades the wake to a fenceless run. The two latter cases emit one
  # unified backend_overlay_inactive alert, told apart by its reason field —
  # the read failure never raises.
  prompt_overlay: str | None = None
  type: str

  @pydantic.model_validator(mode='after')
  def require_model(self) -> BackendOption:
    if self.model is None:
      raise ValueError(f"backend '{self.id}' (type '{self.type}') requires 'model'")
    return self


def parse_option(entry: Any) -> Any:
  """The registered option model built from a raw ``backends.options[]`` mapping.

  Any other value passes through, and the field's own schema judges it.
  """
  if not isinstance(entry, Mapping):
    return entry
  return config_registry.option_model(entry.get("type")).model_validate(entry)


def option_default_model(option: BackendOption, *, subject: str) -> str:
  """Return the option's configured model.

  Raise ValueError when the model is empty, which the load-time ``require_model`` validator does
  not catch because it rejects only a None model. *subject* prefixes the raise with the caller's
  role and carries its own trailing space, the same convention as ``require_backend_option``.
  """
  if not option.model:
    raise ValueError(f"{subject}'{option.id}' has no default model")
  return option.model
