"""The config registry: what packages add to the config schema.

A backend package registers its option model, and a package may register a config section, legacy
keys and a config check. All of them register from the package's ``register()`` function (the
packages listed in ``src/app/registrations.py``) before the first config parse. Config loading
(``src/infra/config.py``) reads this registry and imports no package.

Vocabulary:

- An *option model* is the pydantic model of one ``backends.options[]`` entry. It has a
  ``type: Literal[<backend type>]`` field and subclasses ``BackendOption``
  (``src/infra/backend_models.py``).
- A *section* is a top-level config key that a package owns. The parsed section is an attribute of
  the parsed config: ``cfg.<key>``.
- A *legacy key* is a retired top-level key. Its value names where the key moved. The format is
  that of ``LEGACY_KEYS`` in ``src/infra/config.py``: a plain dotted value points into the
  sectioned mapping, and a key under ``CREDENTIALS_PREFIX`` moved into ``credentials.yaml``.
- A *check* is a function ``(cfg) -> None``. It raises ``ValueError`` when the whole config is
  invalid.

Every model and check is a "module:attr" string. It imports on the first config parse, so a
registration costs no import. This module imports infra only.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from src.infra import deferred

CREDENTIALS_PREFIX = "credentials: "

_option_models: dict[str, str] = {}
_option_classes: dict[str, type] = {}
_section_models: dict[str, str] = {}
_section_classes: dict[str, type] = {}
_legacy_keys: dict[str, str] = {}
_checks: list[str] = []


def register_option_model(backend_type: str, model: str) -> None:
  """Register ``model``, a "module:Class" string, as the option model of ``backend_type``.

  The class is a pydantic model with a ``type: Literal[<backend_type>]`` field. It imports on the
  first config parse. A second registration of one type raises ValueError.
  """
  if backend_type in _option_models:
    raise ValueError(f"backend type {backend_type!r} is already registered")
  _option_models[backend_type] = model


def option_model(backend_type: str) -> type:
  """The option model class of ``backend_type``; ValueError when the type is unregistered."""
  option_class = _option_classes.get(backend_type)
  if option_class is not None:
    return option_class
  path = _option_models.get(backend_type)
  if path is None:
    raise ValueError(
        f"backend type {backend_type!r} is not registered; registered types: {', '.join(_option_models) or 'none'}")
  option_class = deferred.import_attr(path)
  declared = option_class.model_fields["type"].default
  if declared != backend_type:
    raise ValueError(f"{path} declares type {declared!r}; it is registered as backend type {backend_type!r}")
  _option_classes[backend_type] = option_class
  return option_class


def registered_backend_types() -> tuple[str, ...]:
  """Every backend type with a registered option model, in registration order."""
  return tuple(_option_models)


def register_legacy_keys(legacy_keys: Mapping[str, str]) -> None:
  """Add ``legacy_keys`` to the map of retired top-level keys; a key that is already present raises ValueError."""
  for key in legacy_keys:
    if key in _legacy_keys:
      raise ValueError(f"legacy key {key!r} is already registered")
  _legacy_keys.update(legacy_keys)


def legacy_keys() -> dict[str, str]:
  """The retired top-level keys that packages registered."""
  return dict(_legacy_keys)


def register_config_section(key: str, model: str, *, legacy_keys: Mapping[str, str] = {}) -> None:
  """Register ``key``, a top-level config key owned by a package, and its pydantic ``model``, a "module:Class" string.

  The parsed section is an attribute of the parsed config: ``cfg.<key>``. A section the file omits
  holds the model's defaults. ``legacy_keys`` go through ``register_legacy_keys``. A second
  registration of one key raises ValueError.
  """
  if key in _section_models:
    raise ValueError(f"config section {key!r} is already registered")
  register_legacy_keys(legacy_keys)
  _section_models[key] = model


def section_models() -> dict[str, type]:
  """The registered section models by key, in registration order; each model imports on the first call."""
  for key, path in _section_models.items():
    if key not in _section_classes:
      _section_classes[key] = deferred.import_attr(path)
  return dict(_section_classes)


def register_config_check(fn: str) -> None:
  """Register ``fn``, a "module:function" string: ``function(cfg) -> None``, run after the whole config validates.

  Checks run in registration order. A check raises ValueError to fail the load; a hot reload then
  keeps the previous config. A second registration of one function raises ValueError.
  """
  if fn in _checks:
    raise ValueError(f"config check {fn!r} is already registered")
  _checks.append(fn)


def run_config_checks(cfg: Any) -> None:
  """Run every registered check on ``cfg``, in registration order."""
  for fn in _checks:
    deferred.import_attr(fn)(cfg)
