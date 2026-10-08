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
  quota_accounts() -> list[QuotaAccount]
      The source's accounts on the quota panel, in panel order, derived from the live config on
      every call. The function loads the quota code on its first call, so a capture never loads it.
      An account that stays in the config comes back as the same object, so its state (a 429
      backoff, for one) survives between calls.

A backend type attributes its usage to a source with ``attribute_backend_type``, and a backend id
that has left the config attributes through the source's ``id_prefixes``.
"""

import abc
import datetime
import importlib
from dataclasses import dataclass
from enum import StrEnum
from types import ModuleType
from typing import Any

from src.infra import log_once

log = log_once.LazyStructlogLogger()


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


# Keys of the quota panel payload and of one window entry in it. A source's accounts build the payload
# from their provider's quota data, src/backends/claude_code/claude_accounts.py folds it into the
# account readings, and web/static/js/ext_usage.js renders it: one home per key keeps the producers,
# the consumer and the browser in step. A window's ``utilization`` is a percentage as reported;
# ``resets_at`` is an ISO-8601 UTC string, empty when upstream reported none; ``scope_label`` names a
# model-scoped window and is absent on plan-wide ones.
PANEL_WINDOWS = "windows"
PANEL_FETCHED_AT = "fetched_at"
PANEL_PROVIDER = "provider"
PANEL_WINDOW_MINUTES = "window_minutes"
PANEL_UTILIZATION = "utilization"
PANEL_RESETS_AT = "resets_at"
PANEL_SCOPE_LABEL = "scope_label"

# The poller re-reads an unchanged response every round, so one sighting of an unmapped shape is the
# whole alarm; every later round repeats a fired alarm.
_UNKNOWN_LIMIT_SHAPES_SEEN = log_once.WarnOnceRegistry()


def as_utilization(value: Any) -> float | None:
  """Percentage used, or None when upstream did not report one.

  Absent usage stays absent: rendering it as 0.0 would claim a full quota,
  which is the most dangerous wrong answer this strip can give.
  """
  if isinstance(value, bool) or not isinstance(value, (int, float)):
    return None
  return float(value)


def warn_unknown_limit_shape(*, provider: str, account: str, slot: str | int, reason: str) -> None:
  """Log an unrecognized limit shape the first time the process sees it.

  A caller relies on exactly one ``ext_usage_unknown_limit_shape`` event per
  (provider, account, slot, reason) per process: the first sighting carries the
  full signal, and the poller's next round re-transforming the same response is
  not a new shape. ``slot`` is a field name, or the entry index when the entry
  does not name itself.
  """
  _UNKNOWN_LIMIT_SHAPES_SEEN.log(
      log.warning,
      "ext_usage_unknown_limit_shape", (provider, account, str(slot), reason),
      provider=provider,
      account=account,
      slot=slot,
      reason=reason)


class QuotaAccount(abc.ABC):
  """One account of a source on the quota panel, kept by the source's module between rounds.

  ``provider`` is the panel key of the account's entries (``"claude"``, ``"codex"``); with ``label`` it
  forms the cache key ``<provider>:<label>``. ``last_error`` says why the last ``fetch`` returned None.
  ``mark_login_required`` and ``mark_expired`` carry the marks only some providers put on an entry; the
  defaults mark nothing.
  """

  provider: str
  label: str
  last_error: str

  @abc.abstractmethod
  async def fetch(self) -> dict[str, Any] | None:
    """The account's payload, or None with ``last_error`` set; the poller awaits one fetch at a time."""

  def mark_login_required(self, entry: dict[str, Any]) -> None:
    """Set or clear ``login_required`` on the cached *entry* itself, after each fetch of this account."""

  def mark_expired(self, entry: dict[str, Any], now: datetime.datetime) -> dict[str, Any]:
    """The cached *entry* as one emit shows it at *now*: the entry itself, or a copy with expired windows marked.

    A call never writes into *entry*: the poller keeps showing the entry that was fetched.
    """
    return entry


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


def quota_accounts() -> list[QuotaAccount]:
  """The accounts of every registered source that defines ``quota_accounts()``, in source registration order."""
  accounts: list[QuotaAccount] = []
  for source in sources():
    if source.module is not None:
      module = implementation(source)
      if hasattr(module, "quota_accounts"):
        accounts.extend(module.quota_accounts())
  return accounts
