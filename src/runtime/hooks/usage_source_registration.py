"""The registry of usage sources: where each backend's token usage is logged, registered by the backend's own package.

A backend package calls ``register_source`` and ``attribute_backend_type`` from its ``register()``. The usage
package asks ``sources()`` and ``source_for()`` for what registered. This module imports no backend module and no
``dataclasses``, so registering costs no heavy import. ``src/runtime/hooks/usage_sources.py`` holds the contract of
a source's implementation module and the records it returns.
"""

from typing import NamedTuple


class UsageSource(NamedTuple):
  """One place usage is logged.

  ``name`` is the ledger's source value and the usage page's card title. ``id_prefixes`` name a
  backend id that has left the config. ``run_logs_only`` means the usage lives only in CharlieBot's
  own run logs. ``module`` is the implementation module, imported on first use; None when
  CharlieBot's own logs are the only home.
  """

  name: str
  id_prefixes: tuple[str, ...]
  run_logs_only: bool
  module: str | None


_sources: dict[str, UsageSource] = {}
_type_sources: dict[str, str] = {}


def register_source(source: UsageSource) -> None:
  """Add *source* after the sources already registered; a repeated name or prefix raises."""
  if source.name in _sources:
    raise ValueError(f"usage source {source.name!r} is already registered")
  for prefix in source.id_prefixes:
    owner = next((s.name for s in _sources.values() if prefix in s.id_prefixes), None)
    if owner is not None:
      raise ValueError(f"usage source {source.name!r}: id prefix {prefix!r} already belongs to {owner!r}")
  _sources[source.name] = source


def attribute_backend_type(backend_type: str, source: str) -> None:
  """Attribute the usage of backends of *backend_type* to the source named *source*.

  The source may register after the attribution; ``source_for`` resolves it on lookup.
  """
  if backend_type in _type_sources:
    raise ValueError(f"backend type {backend_type!r} is already attributed to {_type_sources[backend_type]!r}")
  _type_sources[backend_type] = source


def sources() -> tuple[UsageSource, ...]:
  """Every registered source, in registration order."""
  return tuple(_sources.values())


def source_for(backend_type: str) -> UsageSource | None:
  """The source *backend_type* is attributed to, or None for a type with no usage source."""
  name = _type_sources.get(backend_type)
  if name is None:
    return None
  source = _sources.get(name)
  if source is None:
    raise ValueError(f"backend type {backend_type!r} is attributed to usage source {name!r}, which is not registered")
  return source
