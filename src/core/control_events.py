"""Durable task-tree control facts: stable ids, event headers, and the write sink.

Task-tree control events (``src/core/event_types.py``'s task/run constants) are
facts, appended to a session's ``chat_events.jsonl`` before the action they
describe takes effect. This module homes the pieces every control owner shares:

- the stable-id scheme (a request_id binds to one node/run id forever, so a
  replayed request returns the original product instead of a duplicate);
- the common event header (``id``/``type``/``timestamp``/``actor``/
  ``source_session_id``) every control event carries;
- :class:`ControlEventSink`, the seam the task-tree owner
  (:mod:`src.core.task_sessions`) and the run owner (:mod:`src.core.runs`)
  write durable facts through. Input delivery (``session_dispatch.py``) and
  completion checks (``task_completion.py``) join this seam in their own
  delivery stages; until then the sink persists without queueing.
"""

import hashlib
import uuid
from typing import TYPE_CHECKING

from src.core import event_types as ET

if TYPE_CHECKING:
  from src.core.sessions import SessionManager

# Deterministic namespace for the tree's stable ids: uuid5 keeps (parent,
# request_id) -> one node id and (session, request_id) -> one run id across
# processes, restarts, and concurrent duplicate requests.
TASK_ID_NAMESPACE = uuid.uuid5(uuid.NAMESPACE_URL, "charliebot/session-task-tree")

# actor values of the common control-event header. The header's actor marks the
# real initiator of the operation; it is record provenance, never caller
# identity (a payload's self-reported actor is not proof of a human caller —
# identity comes from the verified credential, see src.core.run_token).
ACTOR_USER = "user"
ACTOR_AGENT = "agent"
ACTOR_SYSTEM = "system"


def stable_task_id(task_parent_id: str | None, request_id: str) -> str:
  """The node id (parent, request_id) deterministically binds to."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"task:{task_parent_id or 'root'}:{request_id}"))


def stable_run_id(session_id: str, request_id: str) -> str:
  """The run id (session, request_id) deterministically binds to."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"run:{session_id}:{request_id}"))


def stable_close_request_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) close request binds to: repeated
  recovery replays the same task_close_requested fact, never a second one."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"close-request:{session_id}:{request_id}"))


def stable_close_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) close binds to: a duplicate close
  operation id replays the original task_closed fact instead of a new transition,
  across later reopen/close epochs alike."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-closed:{session_id}:{request_id}"))


def stable_reopen_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) reopen binds to (same replay rule)."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-reopened:{session_id}:{request_id}"))


def stable_child_report_id(child_session_id: str, source_event_id: str, recipient_session_id: str) -> str:
  """The child_report id one (child event, fixed recipient) pair derives to.

  The recipient is part of the identity: task_closed.report_to fixes delivery
  ownership at close time, so retries, recovery, and reparenting can never
  retarget or duplicate a historical report."""
  return str(uuid.uuid5(
      TASK_ID_NAMESPACE, f"child-report:{child_session_id}:{source_event_id}:{recipient_session_id}"))


def sha256_hex(text: str) -> str:
  """The SHA-256 hex digest of *text* (UTF-8) — the prompt-body and task-spec fingerprint."""
  return hashlib.sha256(text.encode("utf-8")).hexdigest()


def build_control_event(
    event_type: str,
    *,
    actor: str,
    source_session_id: str | None = None,
    event_id: str | None = None,
    **payload: object,
) -> dict:
  """One control event with the common header plus its typed payload fields."""
  event: dict = {
      "id": event_id or str(uuid.uuid4()),
      "type": event_type,
      "timestamp": _utc_now_iso(),
      "actor": actor,
      "source_session_id": source_session_id,
  }
  event.update(payload)
  return event


def _utc_now_iso() -> str:
  from src.core.models import utc_now
  return utc_now().isoformat()


class ControlEventSink:
  """The durable write seam for control facts.

  Every control event reaches ``chat_events.jsonl`` through one ``append``
  call, under the caller's hold of the tree control write lock. The default
  sink persists through the SessionManager's single append funnel; the input
  delivery stage (``session_dispatch.py``) extends the seam with queue-aware
  persistence without changing the owners' call sites.
  """

  def __init__(self, session_mgr: "SessionManager") -> None:
    self._session_mgr = session_mgr

  async def append(self, session_id: str, event: dict) -> None:
    await self._session_mgr.save_chat_event(session_id, event)

  def load_events(self, session_id: str) -> list[dict]:
    """The session's parsed chat events (the durable fact stream control events ride)."""
    return self._session_mgr.load_chat_events_sync(session_id)

  async def append_task_created(
      self,
      session_id: str,
      *,
      actor: str,
      task_parent_id: str | None,
      task_spec_hash: str | None,
      request_id: str,
  ) -> dict:
    """Append the creation fact and return it (its id becomes created_by_event.event_id)."""
    event = build_control_event(
        ET.TASK_CREATED,
        actor=actor,
        source_session_id=session_id,
        request_id=request_id,
        task_parent_id=task_parent_id,
        task_spec_hash=task_spec_hash,
    )
    await self.append(session_id, event)
    return event
