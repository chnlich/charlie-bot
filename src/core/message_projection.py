"""Memoized message-list projection for paginated history reads.

The authority is a single call to the existing reference implementation
``events_to_view`` from ``src.api.message_utils``. Aggregation logic is NOT
duplicated here — ``MessageProjection`` merely memoizes the result and offers
ordinal-based slicing so pagination costs O(page) with zero file reads.

Pagination is turn-aligned: a requested page start is snapped back to the
start of the turn it falls inside, so every returned page begins with a
complete turn (or with the whole history when it has no separator at all).

The projection is append-incremental: ``stable_closed_prefix_len`` splits the
raw event stream at the last point no open OpenCode run interval crosses, the
closed prefix is fed to the aggregator exactly once, and the still-open tail
is re-evaluated per call through a cloned aggregator, so presenting the view
costs O(open tail) rather than O(history) per session append. Both halves
feed the same ``_stable_history_projection`` + ``MessageAggregator`` pipeline
as the whole-list reference, so the served view always equals
``events_to_view(all_events)`` (see the per-append parity test).

A published projection is immutable: ``advanced`` returns a new object, so a
cache swap is one atomic reference store and readers never tear across a
concurrent advance.

Vocabulary: "closed prefix" = raw events whose projected order can no longer
change; "region" = offered events after it, whose order a completing run
interval may still rewrite.
"""

import bisect

from src.api.message_utils import (
    _stable_history_projection,
    stable_closed_prefix_len,
)
from src.core.message_aggregator import MessageAggregator

__all__ = ["MessageProjection"]


class MessageProjection:
  """Dense, ordinal-addressable view of a session's full message history.

  ``committed`` and ``pending_draft`` are the two return values of
  ``events_to_view(all_events)`` at every ``event_count`` the projection has
  ingested. ``history`` is by definition equal to
  ``events_to_messages(all_events)`` because it feeds the same reference path.

  ``separator_ordinals`` is the precomputed table of ordinals whose message
  role is ``separator``; page starts snap to the ordinal after the nearest
  preceding separator. The table is final up to the closed prefix and
  re-derived over the open region on every ingest.
  """

  __slots__ = (
      "_agg",
      "_committed_final",
      "_history",
      "_offset",
      "_region_events",
      "_seps_final",
      "committed",
      "event_count",
      "pending_draft",
      "separator_ordinals",
  )

  def __init__(self, events: list[dict], event_index_offset: int = 0) -> None:
    self._offset = event_index_offset
    self._agg = MessageAggregator(event_index_offset=event_index_offset)
    self._committed_final: list[dict] = []
    self._seps_final: list[int] = []
    self._region_events: list[dict] = []
    self.event_count = event_index_offset
    self._ingest(events)

  def advanced(self, events: list[dict]) -> 'MessageProjection':
    """Return a copy of this projection advanced by the appended *events*.

    The copy shares no mutable state with the original — racing callers each
    advance their own copy and swap it into the cache atomically, so a lost
    race wastes work instead of serving doubled or torn views.
    """
    copied = MessageProjection.__new__(MessageProjection)
    copied._offset = self._offset
    copied._agg = self._agg.clone()
    copied._committed_final = list(self._committed_final)
    copied._seps_final = list(self._seps_final)
    copied._region_events = list(self._region_events)
    copied.event_count = self.event_count
    copied._ingest(events)
    return copied

  def _ingest(self, events: list[dict]) -> None:
    """Feed events appended after the last ingest and advance ``event_count``.

    Events before the closed boundary commit once; events in the still-open
    region are re-evaluated per call through a clone of the committed
    aggregator's state, so a run interval completing later lands deferrals
    exactly where the whole-list reference places them. Internal: mutates the
    receiver, so it only runs before publication (construction or ``advanced``).
    """
    self._region_events.extend(events)
    self.event_count += len(events)
    closed = stable_closed_prefix_len(self._region_events)
    if closed:
      prefix = self._region_events[:closed]
      del self._region_events[:closed]
      fed_base = self.event_count - len(self._region_events) - closed - self._offset
      for delta in self._agg.feed_indexed([(fed_base + idx, ev) for idx, ev in _stable_history_projection(prefix)]):
        if delta["type"] == "message":
          msg = delta["message"]
          if msg["role"] == "separator":
            self._seps_final.append(len(self._committed_final))
          self._committed_final.append(msg)

    view_agg = self._agg.clone()
    region_committed: list[dict] = []
    region_seps: list[int] = []
    region_base = self.event_count - len(self._region_events) - self._offset
    for delta in view_agg.feed_indexed([
        (region_base + idx, ev) for idx, ev in _stable_history_projection(self._region_events)
    ]):
      if delta["type"] == "message":
        msg = delta["message"]
        if msg["role"] == "separator":
          region_seps.append(len(self._committed_final) + len(region_committed))
        region_committed.append(msg)

    self.committed = [*self._committed_final, *region_committed]
    self.separator_ordinals = self._seps_final + region_seps
    self.pending_draft = view_agg.pending_draft_message()
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
