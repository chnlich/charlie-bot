"""Memoized message-list projection for paginated history reads.

The authority is a single call to the existing reference implementation
``events_to_view`` from ``src.api.message_utils``. Aggregation logic is NOT
duplicated here — ``MessageProjection`` merely memoizes the result and offers
ordinal-based slicing so pagination costs O(page) with zero file reads.
"""

from src.api.message_utils import events_to_view

__all__ = ["MessageProjection"]


class MessageProjection:
  """Dense, ordinal-addressable view of a session's full message history.

  ``committed`` and ``pending_draft`` are the two return values of
  ``events_to_view(all_events)``. ``history`` is by definition equal to
  ``events_to_messages(all_events)`` because it calls the same reference path.
  """

  __slots__ = ("committed", "pending_draft", "_history")

  def __init__(self, events: list[dict], event_index_offset: int = 0) -> None:
    self.committed, self.pending_draft = events_to_view(events, event_index_offset=event_index_offset)
    if self.pending_draft is not None:
      self._history: list[dict] = [*self.committed, self.pending_draft]
    else:
      self._history = list(self.committed)

  @property
  def history(self) -> list[dict]:
    """Committed messages plus the pending draft (if any).

    By definition equal to ``events_to_messages(all_events)``.
    """
    return self._history

  def tail(self, limit: int) -> tuple[list[dict], int, bool]:
    """Return the last *limit* messages.

    Returns (page, oldest_ordinal, has_more) where ``oldest_ordinal`` is the
    ordinal of the first message in the page and ``has_more`` is True when
    older messages exist.
    """
    total = len(self._history)
    start = max(0, total - limit)
    return self._history[start:], start, start > 0

  def slice_before(self, before: int, limit: int) -> tuple[list[dict], int, bool]:
    """Return up to *limit* messages with ordinals strictly below *before*.

    ``before`` is a message ordinal clamped to ``[0, len(history)]``. Returns
    (page, next_before, has_more) where ``next_before = max(0, before - limit)``
    and ``has_more = next_before > 0``.
    """
    total = len(self._history)
    before = max(0, min(before, total))
    lo = max(0, before - limit)
    return self._history[lo:before], lo, lo > 0
