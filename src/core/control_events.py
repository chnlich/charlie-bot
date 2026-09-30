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

from src.core.log_once import LazyStructlogLogger

log = LazyStructlogLogger()

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


def stable_close_blocked_notice_id(session_id: str, request_id: str) -> str:
  """The input id of the one notice telling a requester its (session, request_id)
  close request stayed blocked: recovery replays of the same request re-derive it
  and admit nothing new."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"close-blocked-notice:{session_id}:{request_id}"))


def stable_close_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) close binds to: a duplicate close
  operation id replays the original task_closed fact instead of a new transition,
  across later reopen/close epochs alike."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-closed:{session_id}:{request_id}"))


def stable_reopen_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) reopen binds to (same replay rule)."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-reopened:{session_id}:{request_id}"))


def stable_input_ack_event_id(session_id: str, request_id: str) -> str:
  """The event id one (session, request_id) input acknowledgement binds to: a
  replayed acknowledgement returns the original fact, never a second one."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"input-ack:{session_id}:{request_id}"))


def stable_child_report_id(child_session_id: str, source_event_id: str, recipient_session_id: str) -> str:
  """The child_report id one (child event, fixed recipient) pair derives to.

  The recipient is part of the identity: task_closed.report_to fixes delivery
  ownership at close time, so retries, recovery, and reparenting can never
  retarget or duplicate a historical report."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"child-report:{child_session_id}:{source_event_id}:{recipient_session_id}"))


def stable_withheld_event_id(run_id: str, reason: str) -> str:
  """The run_launch_withheld event id one (run, reason) pair derives to.

  The same Run withheld for the same reason re-derives the same id, so repeated
  launch attempts and recovery passes append the durable record once; a
  different reason (the precondition moved) records again under its own id."""
  return str(uuid.uuid5(TASK_ID_NAMESPACE, f"run-launch-withheld:{run_id}:{reason}"))


def sha256_hex(text: str) -> str:
  """The SHA-256 hex digest of *text* (UTF-8) — the prompt-body and task-spec fingerprint."""
  return hashlib.sha256(text.encode("utf-8")).hexdigest()


def derived_delegate_request_id(session_id: str, task_type: str, description: str) -> str:
  """The delegation request id one (session, task type, spec body) binds to.

  The server derives this default when a delegation carries no explicit
  request_id; the CLI derives the same value so its sent-but-lost readback
  binds to the child the server created. An explicit request_id names
  intentional same-spec siblings and overrides this derivation on both sides.
  """
  return "delegate-" + sha256_hex("\x00".join([session_id, task_type, description]))[:24]


def build_control_event(
    event_type: str,
    *,
    actor: str,
    source_session_id: str,
    event_id: str | None = None,
    **payload: object,
) -> dict:
  """One control event with the common header plus its typed payload fields."""
  # Lazy: the pydantic model stack stays off this module's import path; the
  # callers that stamp control facts pay it, not the boot chain.
  from src.core.models import utc_now_iso

  event: dict = {
      "id": event_id or str(uuid.uuid4()),
      "type": event_type,
      "timestamp": utc_now_iso(),
      "actor": actor,
      "source_session_id": source_session_id,
  }
  event.update(payload)
  return event


class ControlEventSink:
  """The durable write seam for control facts.

  Every control event reaches ``chat_events.jsonl`` through one ``append``
  call, under the caller's hold of the tree control write lock. The default
  sink persists through the SessionManager's single append funnel; the input
  delivery stage (``session_dispatch.py``) extends the seam with queue-aware
  persistence without changing the owners' call sites.
  """

  def __init__(self, session_mgr: SessionManager) -> None:
    self._session_mgr = session_mgr

  async def append(self, session_id: str, event: dict) -> None:
    await self._session_mgr.save_chat_event(session_id, event)
    # The durable fact is written; notify connected UIs best-effort. A
    # notification failure never fails the operation (the fact is already on
    # disk and catch-up reconciles the client), it is only logged.
    await self.notify_tree_changed(session_id, event.get("type"))

  async def notify_tree_changed(self, session_id: str, event_type: str | None) -> None:
    """Best-effort sidebar notification for one changed node's durable facts."""
    try:
      await self._session_mgr.broadcast_task_tree_changed(session_id, event_type)
    except Exception:
      log.exception("task_tree_changed_broadcast_failed", session_id=session_id, event_type=event_type)

  def load_events(self, session_id: str) -> list[dict]:
    """The session's parsed chat events (the durable fact stream control events ride)."""
    return self._session_mgr.load_chat_events_sync(session_id)
