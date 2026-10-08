"""The durable write seam for control facts: the sink the task-tree owner builds.

Task-tree control events (``src/runtime/control_events.py`` holds their ids and headers) are appended to
a session's ``chat_events.jsonl`` before the action they describe takes effect. The task-tree owner
(:mod:`src.runtime.task_sessions`) builds one :class:`ControlEventSink` over the ``SessionManager`` and the
run owner (:mod:`src.runtime.runs`) writes through it as a ``control_events.RunEventSink``. Input delivery
(``session_dispatch.py``) and completion checks (``task_completion.py``) join this seam in their own delivery
stages; until then the sink persists without queueing.
"""

from src.infra import log_once
from src.runtime import sessions

log = log_once.LazyStructlogLogger()


class ControlEventSink:
  """The durable write seam for control facts.

  Every control event reaches ``chat_events.jsonl`` through one ``append``
  call, under the caller's hold of the tree control write lock. The default
  sink persists through the SessionManager's single append funnel; the input
  delivery stage (``session_dispatch.py``) extends the seam with queue-aware
  persistence without changing the owners' call sites.
  """

  def __init__(self, session_mgr: sessions.SessionManager) -> None:
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
