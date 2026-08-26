"""Idempotency judgments for the three finalize effects.

Each of the finalize chain's effects is re-runnable because each is keyed on
its OWN effect's observable result, never on a prerequisite step:

- worker_summary persist: a terminal worker_summary event for this thread_id
  is already in the chat stream;
- reviewer spawn: a thread with review_of == the original thread's id exists;
- master wake: a master output event (assistant / master_done /
  assistant_error) appears AFTER the thread's terminal summary.

Effect-keyed (not prerequisite-keyed) matters for the wake judgment in
particular: the summary is persisted before the master is triggered, so keying
on "summary present" would leave a kill in that gap neither waking nor
retrying. Keying on the effect makes "did it happen?" and "should I do it?"
the same state.

All predicates are pure functions over already-loaded chat/thread data so both
the live finalize path and the startup reconcile pass apply identical rules.
"""

from src.core import event_types as ET
from src.core.models import ThreadMetadata

# Master output events that count as "the master woke". Deliberately excludes
# resume_context_dropped (context-recovery noise, not an answer) and the
# trigger_master ERROR payload (its type is plain "error", not in this set).
_MASTER_OUTPUT_TYPES = frozenset({ET.ASSISTANT, ET.MASTER_DONE, ET.ASSISTANT_ERROR})


def _is_terminal_worker_summary(event: dict, thread_id: str) -> bool:
  return (
      event.get("type") == ET.WORKER_SUMMARY
      and event.get("thread_id") == thread_id
      and event.get("status") != "running"
  )


def terminal_summary_present(chat_events: list[dict], thread_id: str) -> bool:
  """Whether the chat stream already holds this thread's terminal worker_summary."""
  return any(_is_terminal_worker_summary(ev, thread_id) for ev in chat_events)


def master_woke_after_summary(chat_events: list[dict], thread_id: str) -> bool:
  """Whether any master output followed this thread's LAST terminal summary."""
  last_summary_idx: int | None = None
  for idx, ev in enumerate(chat_events):
    if _is_terminal_worker_summary(ev, thread_id):
      last_summary_idx = idx
  if last_summary_idx is None:
    return False
  return any(ev.get("type") in _MASTER_OUTPUT_TYPES for ev in chat_events[last_summary_idx + 1:])


def reviewer_thread_exists(
    threads: list[ThreadMetadata],
    original_thread_id: str,
    *,
    exclude_thread_id: str | None = None,
) -> bool:
  """Whether a reviewer thread for *original_thread_id* already exists.

  ``exclude_thread_id`` is the thread whose finalize is currently running: on
  the failed-reviewer retry path the FAILED reviewer itself matches the
  review_of check, and it must not block its own replacement.
  """
  return any(
      t.review_of == original_thread_id and t.id != exclude_thread_id for t in threads)
