"""Usage sources: where each backend's token usage is logged, registered by the backend's own package.

The usage package (src/features/usage) reads usage only through this module and the registry beside it
(``usage_source_registration.py``), so it names no backend. A backend package registers one ``UsageSource``
from its ``register()``; the usage package asks ``usage_source_registration.sources()`` for the list and
``implementation()`` for the code that reads one source's logs.

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
  sweep(scope) -> SourceSweep
      The source's part of the cold-storage sweep (src/features/usage/storage_cool.py): it deletes
      the source's conversation logs that no reader can reach again, and reports what it freed or,
      in a dry run, what it would free. ``scope`` carries what the sweep reads (``SweepScope``).
      The function loads the sweep code on its first call, so a capture never loads it. The sweep
      runs after the ledger capture, so the usage of a log is recorded before the log goes.

A backend type attributes its usage to a source with ``usage_source_registration.attribute_backend_type``, and a
backend id that has left the config attributes through the source's ``id_prefixes``.
"""

import abc
import datetime
import importlib
import re
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import ModuleType
from typing import TYPE_CHECKING, Any

from src.infra import log_once
from src.runtime.hooks import usage_source_registration

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig

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


# The sweep types. src/features/usage/storage_cool.py builds the scope, calls ``sweep_all`` and reads
# the results; each backend's ``sweep`` builds a ``SourceSweep`` from its own ``SweepCounter``s.

# Canonical UUID form: a CharlieBot session id as it appears verbatim in a backend's file names, and
# the thread id a Codex rollout name embeds.
SESSION_ID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")


@dataclass(frozen=True)
class CategoryResult:
  """One output category of the sweep: what it freed, or in a dry run what it would free."""

  name: str
  unit: str
  count: int
  bytes: int


@dataclass(frozen=True)
class FreelistResult:
  """The free pages a source's database holds, in bytes, which a manual vacuum hands back.

  ``name`` labels the line the sweep report prints for them.
  """

  name: str
  bytes: int


@dataclass(frozen=True)
class SourceSweep:
  """What one source's ``sweep`` reports: its categories in report order, and its database's free
  pages, or None for a source that keeps none."""

  categories: tuple[CategoryResult, ...]
  freelist: FreelistResult | None


@dataclass(frozen=True)
class SessionFacts:
  """The metadata fields of one CharlieBot session that the sweep judges on.

  ``cold`` is the verdict of the cold rule (archived and idle long enough); ``cc_session_id`` is the
  backend session id that the session's metadata references, or None.
  """

  id: str
  cold: bool
  cc_session_id: str | None


class SweepCounter:
  """Accumulates one category's count and freed bytes; ``result()`` freezes them."""

  def __init__(self, name: str, unit: str) -> None:
    self.name = name
    self.unit = unit
    self.count = 0
    self.bytes = 0

  def add(self, size: int) -> None:
    self.count += 1
    self.bytes += size

  def add_bytes(self, size: int) -> None:
    """Bytes without a count: the unit belongs to a larger whole (a directory)."""
    self.bytes += size

  def delete_file(self, path: Path, dry_run: bool, *, count: bool = True) -> None:
    """Delete one file (or account for it in a dry run), best effort per file.

    ``count=False`` folds the bytes into a category whose unit is larger than a
    file (a transcript directory).
    """
    try:
      size = path.stat().st_size
    except OSError as e:
      log.warning("storage_cool_file_stat_failed", path=str(path), error=str(e))
      return
    if not dry_run:
      try:
        path.unlink()
      except OSError as e:
        log.warning("storage_cool_file_delete_failed", path=str(path), error=str(e))
        return
    if count:
      self.add(size)
    else:
      self.add_bytes(size)

  def result(self) -> CategoryResult:
    return CategoryResult(self.name, self.unit, self.count, self.bytes)


@dataclass(frozen=True)
class SweepScope:
  """Everything a source's ``sweep`` reads; the usage package builds one per sweep.

  ``facts`` maps a session id to its ``SessionFacts``; a scoped run keeps only the scoped session,
  and none when it is not cold. ``references`` maps each backend session id that CharlieBot metadata
  references to the sessions that reference it, threads included. A record goes under one rule: every
  referencing session is cold, or nothing references it and its own idle clock is past
  ``orphan_idle_days``. ``session_id`` is the scoped session id, or None for a whole-host run.
  ``vacuum`` and ``force`` are the options as given: a dry run vacuums nothing, and ``force`` has no
  effect without ``vacuum``. A dry run deletes nothing and issues no SQL that changes a database.
  """

  cfg: CharlieBotConfig
  now: datetime.datetime
  dry_run: bool
  session_id: str | None
  facts: dict[str, SessionFacts]
  references: dict[str, list[SessionFacts]]
  orphan_idle_days: int
  vacuum: bool
  force: bool

  def referenced_and_cold(self, backend_session: str) -> bool:
    """The referenced half of the rule: CharlieBot metadata references the record and every
    referencing session is cold."""
    referencing = self.references.get(backend_session)
    return bool(referencing) and all(owner.cold for owner in referencing)

  def idle_past(self, idle_since_epoch: float) -> bool:
    """The orphan half of the rule: the record's own idle clock has passed ``orphan_idle_days``."""
    return self.now.timestamp() - idle_since_epoch >= self.orphan_idle_days * 86400

  def scoped_backend_sessions(self) -> set[str]:
    """Every backend session id the scoped session references; empty when the run is unscoped."""
    if self.session_id is None:
      return set()
    owner = self.facts.get(self.session_id)
    backend_sessions = {owner.cc_session_id} if owner and owner.cc_session_id is not None else set()
    backend_sessions.update(
        backend_session for backend_session, owners in self.references.items() if any(
            reference.id == self.session_id for reference in owners))
    return backend_sessions


def sorted_scan(root: Path, listing: Iterable[Path]) -> list[Path] | None:
  """Materialize one sweep directory listing sorted, or None once the failure is logged.

  ``root`` is the directory the warning names: the listing's own root for
  ``iterdir``, the walked tree for ``rglob``.
  """
  try:
    return sorted(listing)
  except OSError as e:
    log.warning("storage_cool_dir_scan_failed", dir=str(root), error=str(e))
    return None


def sweep_root_entries(roots: Iterable[Path], listing: Callable[[Path], Iterable[Path]]) -> Iterator[Path]:
  """Yield each sweep root's sorted entries, skipping a missing root or an unreadable listing.

  ``sorted_scan`` already logs a listing failure; the sweep's best-effort
  contract is that one unreadable directory never stops the run.
  """
  for root in roots:
    if not root.is_dir():
      continue
    entries = sorted_scan(root, listing(root))
    if entries is None:
      continue
    yield from entries


def implementation(source: usage_source_registration.UsageSource) -> ModuleType:
  """The module that reads *source*'s logs, imported on first use."""
  if source.module is None:
    raise ValueError(f"usage source {source.name!r} has no implementation module")
  return importlib.import_module(source.module)


def quota_accounts() -> list[QuotaAccount]:
  """The accounts of every registered source that defines ``quota_accounts()``, in source registration order."""
  accounts: list[QuotaAccount] = []
  for source in usage_source_registration.sources():
    if source.module is not None:
      module = implementation(source)
      if hasattr(module, "quota_accounts"):
        accounts.extend(module.quota_accounts())
  return accounts


def sweep_all(scope: SweepScope) -> list[SourceSweep]:
  """The sweep of every registered source that defines ``sweep()``, in source registration order."""
  sweeps: list[SourceSweep] = []
  for source in usage_source_registration.sources():
    if source.module is not None:
      module = implementation(source)
      if hasattr(module, "sweep"):
        sweeps.append(module.sweep(scope))
  return sweeps
