"""The two finalize-effect judgments as pure predicates over loaded chat events.

- worker_summary persist: a terminal worker_summary event for this thread_id
  is already in the chat stream;
- master wake: a master output event (assistant / master_done /
  assistant_error) appears AFTER the thread's terminal summary.

Each is effect-keyed (never prerequisite-keyed): the summary persists before
the master triggers, so a wake keyed on "summary present" would leave a kill
in that gap neither waking nor retrying.

No production or test code calls these: the module's only reader is the
standing M76 collector (docs/perf_baseline.md@5175adf09), which drives both scans in
its fold-absent fallback.
"""

from src.infra import event_types as ET

# Master output events that count as "the master woke". Deliberately excludes
# resume_context_dropped (context-recovery noise, not an answer) and the
# trigger_master ERROR payload (its type is plain "error", not in this set).
_MASTER_OUTPUT_TYPES = frozenset({ET.ASSISTANT, ET.MASTER_DONE, ET.ASSISTANT_ERROR})


def _is_terminal_worker_summary(event: dict, thread_id: str) -> bool:
  return (
      event.get("type") == ET.WORKER_SUMMARY and event.get("thread_id") == thread_id and
      event.get("status") != "running")


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
