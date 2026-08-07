"""Memoized message-list projection for paginated history reads.

The authority is a single call to the existing reference implementation
``events_to_view`` from ``src.api.message_utils``. Aggregation logic is NOT
duplicated here — ``MessageProjection`` merely memoizes the result and offers
ordinal-based slicing so pagination costs O(page) with zero file reads.

Pagination is turn-aligned: a requested page start is snapped back to the
start of the turn it falls inside, so every returned page begins with a
complete turn (or with the whole history when it has no separator at all).
"""

import bisect

from src.api.message_utils import events_to_view

__all__ = ["MessageProjection"]


class MessageProjection:
  """Dense, ordinal-addressable view of a session's full message history.

  ``committed`` and ``pending_draft`` are the two return values of
  ``events_to_view(all_events)``. ``history`` is by definition equal to
  ``events_to_messages(all_events)`` because it calls the same reference path.

  ``separator_ordinals`` is the precomputed table of ordinals whose message
  role is ``separator``; page starts snap to the ordinal after the nearest
  preceding separator. The projection is immutable, so the table cannot go
  stale — a session append produces an event-count change, which rebuilds the
  whole projection.
  """

  __slots__ = ("committed", "pending_draft", "event_count", "separator_ordinals", "_history")

  def __init__(self, events: list[dict], event_index_offset: int = 0) -> None:
    self.event_count = event_index_offset + len(events)
    self.committed, self.pending_draft = events_to_view(events, event_index_offset=event_index_offset)
    self.separator_ordinals: list[int] = [
        i for i, msg in enumerate(self.committed) if msg["role"] == "separator"
    ]
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

  def _snap_to_turn_start(self, desired: int) -> int:
    """Move *desired* back to the start of the turn it falls inside.

    Returns the ordinal after the last separator strictly below *desired*, or
    0 when no separator lies below it (including a history with no separator
    at all, whose whole span is then one page — documented behavior, not an
    error). A *desired* that is already a turn start does not move.
    """
    i = bisect.bisect_left(self.separator_ordinals, desired)
    return self.separator_ordinals[i - 1] + 1 if i else 0

  def tail(self, limit: int) -> tuple[list[dict], int, bool]:
    """Return the last turn-aligned page of at least *limit* messages.

    The raw start ``max(0, total - limit)`` is snapped back to its turn
    start, so the page holds at least *limit* messages unless the history is
    exhausted, and ``start == 0`` or ``committed[start - 1]`` is a separator.
    Returns (page, oldest_ordinal, has_more) where ``has_more`` is True when
    older messages exist.
    """
    total = len(self.committed)
    start = self._snap_to_turn_start(max(0, total - limit))
    return self.committed[start:], start, start > 0

  def slice_before(self, before: int, limit: int) -> tuple[list[dict], int, bool]:
    """Return the turn-aligned page of messages below *before*.

    ``before`` is a message ordinal clamped to ``[0, len(committed)]``. The raw
    start ``max(0, before - limit)`` is snapped back to its turn start, so
    the page holds at least *limit* messages unless the history below
    *before* is exhausted. Returns (page, next_before, has_more) where
    ``next_before`` is the snapped page start and ``has_more = next_before > 0``.
    """
    total = len(self.committed)
    before = max(0, min(before, total))
    start = self._snap_to_turn_start(max(0, before - limit))
    return self.committed[start:before], start, start > 0
