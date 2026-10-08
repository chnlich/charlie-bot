"""Usage sources: where each backend's token usage is logged, registered by the backend's own package.

The usage package (src/features/usage) reads usage only through this module, so it names no backend.
A backend package registers one ``UsageSource`` from its ``register()``; the usage package asks
``sources()`` for the list and ``implementation()`` for the code that reads one source's logs.

``UsageRecord`` and ``RecordKind`` are the records an implementation returns and the ledger stores.

An implementation module defines only the functions its source supports:

  logs() -> Iterable[tuple[Path, str]]
      The source's log files on this host, each with its account label.
  read(path, account, previous) -> tuple[str, list[UsageRecord]]
      One log file's records and its new signature. ``previous`` is the signature the ledger stores
      for the file, or None. The signature is taken before the read, so an append during the read
      leaves the stored signature outdated and the next capture reads the file again.
  live_cli_sessions() -> set[str]
      The CLI session ids whose own logs still exist. A source that defines it is read under the
      Codex rule (see src/features/usage/token_tally.py).

A backend type attributes its usage to a source with ``attribute_backend_type``, and a backend id
that has left the config attributes through the source's ``id_prefixes``.
"""

import importlib
from dataclasses import dataclass
from enum import StrEnum
from types import ModuleType


class RecordKind(StrEnum):
  """How a usage row was learned: from a CLI-kept log (native) or from a charlie-bot
  capture whose underlying CLI log may be pruned (fallback)."""

  NATIVE = "native"
  FALLBACK = "fallback"


@dataclass(frozen=True, slots=True)
class UsageRecord:
  """One API call's usage as extracted from a captured file.

  ``ts`` is ISO 8601 UTC; ``sessions`` names the CLI sessions the call's log belongs
  to, and a fallback record must carry at least one — exclusion is keyed on them.
  ``in_unsplit`` is input whose cache hit/miss split was never logged; it counts toward
  every total like the split input columns.
  """

  record_id: str
  kind: RecordKind
  source: str
  model: str
  account: str
  ts: str
  in_fresh: int
  cache_write: int
  cache_read: int
  output: int
  in_unsplit: int = 0
  sessions: tuple[str, ...] = ()

  def __post_init__(self) -> None:
    if self.kind == RecordKind.FALLBACK and not self.sessions:
      raise ValueError(f"fallback record {self.record_id!r} carries no sessions")


@dataclass(frozen=True)
class UsageSource:
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


def implementation(source: UsageSource) -> ModuleType:
  """The module that reads *source*'s logs, imported on first use."""
  if source.module is None:
    raise ValueError(f"usage source {source.name!r} has no implementation module")
  return importlib.import_module(source.module)
