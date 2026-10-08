"""Runtime hooks for packages that own durable sequences.

Packages register a controller by module path. The module imports no feature
package; a controller module is imported only when a runtime path first asks
for the registered controllers.
"""

from __future__ import annotations

import importlib
from datetime import datetime
from typing import TYPE_CHECKING, Protocol

if TYPE_CHECKING:
  import asyncio
  from collections.abc import Iterable
  from pathlib import Path

  from src.infra.config import CharlieBotConfig
  from src.infra.models import RunRecord, SessionMetadata
  from src.runtime.sessions import SessionManager


class SequenceBinding(Protocol):
  """One node's binding to a sequence; cron: the loaded task whose session_id names the node."""

  name: str
  dedicated_backend: bool

  def backend_lock_detail(self, target_backend: str) -> str:
    """The 400 detail when a backend switch is refused."""
    ...

  async def on_wake(
      self,
      meta: SessionMetadata,
      input_events: list[dict],
      *,
      sessions: SessionManager,
  ) -> str | None:
    """Run the node's wake duties and return an optional turn-input prefix."""
    ...

  async def on_archive(self) -> None:
    """Run the sequence's archive duty for this node."""
    ...


class SequenceRuns(Protocol):
  """The run store members the sequence controllers call."""

  def run_dir(self, session_id: str, run_id: str) -> Path:
    ...

  async def get_run(self, session_id: str, run_id: str) -> RunRecord | None:
    ...

  def list_run_records_sync(self, session_id: str) -> list[RunRecord]:
    ...

  def load_events_sync(self, session_id: str) -> list[dict]:
    ...

  def terminal_outcome(self, events: list[dict], run_id: str) -> str | None:
    ...

  async def terminal_outcome_of(self, session_id: str, run_id: str) -> str | None:
    ...

  def run_has_terminal_fact(self, run: RunRecord, events: list[dict]) -> bool:
    ...

  async def register_run_locked(self, record: RunRecord, *, task_spec_text: str | None) -> RunRecord:
    ...


class SequenceDispatch(Protocol):
  """The input dispatcher members the sequence controllers call."""

  executor: object | None

  def report_source_event(self, child_session_id: str, child_kind: str) -> dict:
    ...

  async def deliver_child_report_locked(
      self,
      child_session_id: str,
      *,
      source_event: dict,
      outcome: str,
      summary: str,
      result_refs: list[str] | None,
      recipient: str | None,
  ) -> tuple[dict, bool]:
    ...

  async def wake_parent(self, parent_id: str, *, report: dict) -> asyncio.Task | None:
    ...


class SequenceCompletion(Protocol):
  """The completion manager members the sequence controllers call."""

  async def evaluate_automatic_completion(
      self,
      session_id: str,
      *,
      run_id: str,
      summary: str | None,
      result_refs: list[str] | None,
      request_id: str | None,
  ) -> tuple[int, dict]:
    ...


class SequenceSessions(Protocol):
  """The session service members the sequence controllers call."""

  async def prime_aggregator(self, session_id: str) -> int:
    ...

  async def announce_appended_event(self, session_id: str, event: dict, *, epoch: int) -> None:
    ...

  async def deliver_to_successor(self, session_id: str, event: dict) -> str | None:
    ...


class SequenceTree(Protocol):
  """The task tree members the sequence controllers call; ``TaskTreeManager`` implements them."""

  control_lock: asyncio.Lock
  runs: SequenceRuns
  dispatch: SequenceDispatch
  completion: SequenceCompletion
  _cfg: CharlieBotConfig  # read by the cron backend resolution

  @property
  def sessions(self) -> SequenceSessions:
    ...

  async def load_meta(self, session_id: str) -> SessionMetadata | None:
    ...

  def task_state(self, session_id: str) -> str:
    ...


class SequenceController(Protocol):
  """A package's runtime interface for one kind of durable sequence."""

  owner_prefix: str

  async def redrive(self, session_id: str, tree: SequenceTree, cfg: CharlieBotConfig) -> None:
    """Redrive the sequence that owns a durable Run reference."""
    ...

  async def reconcile_interrupted(self, cfg: CharlieBotConfig, tree: SequenceTree) -> None:
    """Reconcile interrupted sequences before ordinary Run recovery, when needed."""
    return

  def binding(self, session_id: str) -> SequenceBinding | None:
    """The sequence binding for *session_id*, if one exists."""
    ...

  def owns_session(self, meta: SessionMetadata) -> bool:
    """Whether *meta* carries this controller's legacy ownership marker."""
    ...

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
    """Return this controller's per-row fields for one listing batch."""
    ...


_registered: dict[str, str] = {}
_loaded: dict[str, SequenceController] = {}


def register_sequence_controller(name: str, controller: str) -> None:
  """Register a controller's ``module:attr`` path for lazy import on first use."""
  if name in _registered:
    raise ValueError(f"sequence controller {name!r} is already registered")
  _registered[name] = controller


def _load_controller(name: str) -> SequenceController:
  controller = _loaded.get(name)
  if controller is not None:
    return controller
  path = _registered[name]
  module_name, separator, attr = path.partition(":")
  if not separator or not module_name or not attr:
    raise ValueError(f"{path!r} is not a 'module:attr' string")
  controller = getattr(importlib.import_module(module_name), attr)()
  _loaded[name] = controller
  return controller


def sequence_controllers() -> tuple[SequenceController, ...]:
  """The registered controllers, imported and instantiated on first use."""
  return tuple(_load_controller(name) for name in _registered)


def controller_for(owner_ref: str) -> SequenceController | None:
  """The controller whose owner prefix matches *owner_ref*, if registered."""
  matches = [controller for controller in sequence_controllers() if owner_ref.startswith(controller.owner_prefix)]
  if len(matches) > 1:
    raise ValueError(f"multiple sequence controllers own {owner_ref!r}")
  return matches[0] if matches else None


def binding_for(session_id: str) -> SequenceBinding | None:
  """The first controller binding for *session_id*; two bindings are an error."""
  binding = None
  for controller in sequence_controllers():
    found = controller.binding(session_id)
    if found is None:
      continue
    if binding is not None:
      raise ValueError(f"multiple sequence controllers bind session {session_id!r}")
    binding = found
  return binding


def sequence_listing_fields(session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
  """Merge each controller's listing fields into a row map keyed by session id."""
  ids = tuple(sorted(set(session_ids)))
  fields_by_session = {session_id: {} for session_id in ids}
  for controller in sequence_controllers():
    for session_id, fields in controller.listing_fields(ids, now_utc).items():
      fields_by_session.setdefault(session_id, {}).update(fields)
  return fields_by_session
