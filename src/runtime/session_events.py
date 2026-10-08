"""Session chat events: reads and writes of chat_events.jsonl, the live aggregators, the projection cache, broadcast.

``SessionEvents`` is the one owner of a session's chat-event stream in memory. Writes funnel through
``save_chat_event`` (one append) or ``persist_and_broadcast`` (append, then broadcast through the session's live
``MessageAggregator``); reads serve the events cache (``load_chat_events_sync``) or the disk
(``load_chat_events_tail``, ``load_chat_events_range``). The message-projection cache and the sidebar broadcast live
here because ``drop_session_runtime_state`` clears every per-session memory of the stream in one place. The process
builds one block (``events()``); tests build their own and install it with ``set_events()``.
"""

import asyncio
from pathlib import Path
from typing import Any

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig, get_config
from src.infra.gc_control import gc_off
from src.infra.locks import lock_for
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import BoundedMemo
from src.infra.models import SessionMetadata
from src.infra.tasks import create_logged_task
from src.runtime import session_store
from src.runtime.chat_events import ChatEventStore
from src.runtime.hooks import turn_contributions
from src.runtime.hooks.sidebar_contributions import sidebar_contributions
from src.runtime.message_aggregator import MessageAggregator
from src.runtime.message_projection import MessageProjection
from src.runtime.streaming import SIDEBAR_CHANNEL, session_channel, streaming_manager

# Raw event types whose render content is produced by the per-session
# MessageAggregator as `message`/`stream` deltas. We persist these events but
# do not broadcast them raw -- the deltas are the wire format.
RAW_EVENTS_REPLACED_BY_DELTAS: frozenset[str] = frozenset(
    {ET.ASSISTANT, ET.USER, ET.SCHEDULED_TRIGGER, ET.CHILD_REPORT, ET.TASK_CLOSED, ET.TASK_REOPENED})

log = LazyStructlogLogger()

# The window must cover the tabs' session rotation, so a re-entry never re-pays
# the cold build: the switch diagnostic rotated among 21 distinct sessions in a
# 16 h sample. A retained projection shares the events cache's strings (~0.5 MB
# per big session measured), so the window's memory rides the unbounded events
# cache's profile.
_PROJECTION_LRU_LIMIT = 64
# The aggregator init feeds the caught-up corpus in on-loop slices of this
# many events, one yield between slices (the _CatchupWalk shape, server.py):
# the per-event feed cost is ~1 us (20534-event worst live corpus), so a
# slice's hold stays near the poll cadences' 5 ms resolution, while a
# whole-corpus single-span feed parks the loop behind GIL handoffs for its
# full span (measured 23 ms worst hold per pass).
_AGGREGATOR_INIT_SLICE_EVENTS = 256


class SessionEvents:
  """Session chat events: the events cache and its disk reads and writes, the live aggregators, broadcast."""

  def __init__(self, cfg: CharlieBotConfig, store: session_store.SessionStore) -> None:
    self._cfg = cfg
    self._store = store
    self.chat_events = ChatEventStore(store.session_dir, store.metadata_path, store.metadata_cache)
    # Per-session MessageAggregator instance carrying live streaming state
    # (assistant_buf, tools_buf). Lazy-initialized from disk on first
    # persist_and_broadcast for a session after server start, then maintained
    # in memory across calls so consecutive assistant chunks accumulate into
    # a single bubble and tool-only events attach to the prior text bubble.
    self._aggregators: dict[str, MessageAggregator] = {}
    # Serializes the lazy disk catch-up behind _get_or_init_aggregator: the
    # init loads and feeds the whole live corpus, so two events arriving
    # back-to-back for the same session must not double-init.
    self._aggregator_init_locks: dict[str, asyncio.Lock] = {}
    # Bumped by every drop_session_runtime_state so an in-flight catch-up
    # can detect that the corpus it read is no longer current and discard
    # its result instead of resurrecting dropped runtime state.
    self._aggregator_epoch: dict[str, int] = {}
    # Per-session MessageProjection cache (LRU, cap _PROJECTION_LRU_LIMIT). A
    # hit requires the cached event_count to equal the live event count
    # (get_message_projection, projection_memo_hit), so a stale projection is
    # never served. Route access spans both the event loop (the memo-hit
    # fast path) and asyncio.to_thread (the threaded build/advance), and the
    # to_thread side must not observe a half-updated map, so the memo
    # mechanics are BoundedMemo's locked ones, not a bare OrderedDict's.
    self._projection_cache: BoundedMemo[str, MessageProjection] = BoundedMemo(_PROJECTION_LRU_LIMIT)

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl.

    See ``src/runtime/chat_events.py`` for the path layout.
    """
    return self.chat_events.get_chat_events_path(session_id)

  async def broadcast_sidebar(self, session_id: str, event_type: str, **fields: Any) -> None:
    """Broadcast one session-scoped sidebar event.

    Every session-scoped sidebar event carries the same channel and
    ``session_id`` key; the helper is what keeps the senders agreeing on that
    payload shape.
    """
    await streaming_manager.broadcast(SIDEBAR_CHANNEL, {"type": event_type, "session_id": session_id, **fields})

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl.

    See ``src/runtime/chat_events.py`` for the id/timestamp injection and cache-sync contract.
    """
    await self.chat_events.save_chat_event(session_id, event)

  async def _feed_and_broadcast(
      self, session_id: str, event: dict, aggregator: MessageAggregator, archive_offset: int) -> None:
    """Stamp the event index, feed the aggregator, and broadcast what comes out.

    The index is ``archive_offset + cached count - 1`` over the post-append
    events cache. Deltas go out first; a raw event whose type is in
    ``RAW_EVENTS_REPLACED_BY_DELTAS`` never follows them because the deltas
    replace it on the wire, while every other event type flows raw for the
    state side-effects clients hang off it (for example ``master_done`` → stopThinking).
    """
    event["event_index"] = archive_offset + self.chat_events.cached_event_count(session_id) - 1

    channel = session_channel(session_id)
    deltas = list(aggregator.feed(event))
    for delta in deltas:
      await streaming_manager.broadcast(channel, delta)
    if event.get("type") not in RAW_EVENTS_REPLACED_BY_DELTAS:
      await streaming_manager.broadcast(channel, event)

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    """Persist event, then broadcast it through the session's aggregator.

    Callers rely on the event being durable and on the wire output matching
    the announce path's; both ride the shared ``_feed_and_broadcast``.
    """
    # Prime the events cache + aggregator before persisting so event_index
    # injection works on the very first call after server start (and so the
    # aggregator state matches what SSR/SPA-switch produced for the same
    # events).
    aggregator = await self._get_or_init_aggregator(session_id)
    meta = await self._store.get_session(session_id)
    archive_offset = meta.archive_offset if meta else 0
    await self.save_chat_event(session_id, event)
    await self._feed_and_broadcast(session_id, event, aggregator, archive_offset)

    # Each turn contribution reacts to the round's terminal event after the
    # broadcast and in its own task: the funnel neither waits on a contribution
    # nor breaks when one fails, and a round re-attached after a restart reaches
    # them through this same point. A contribution decides for itself whether
    # the round is one it acts on. A missing session has no round to react to.
    if event.get("type") == ET.MASTER_DONE and meta is not None:
      for contribution in turn_contributions.turn_contributions():
        create_logged_task(
            contribution.after_turn(meta, event, cfg=self._cfg),
            name=f"after-turn-{type(contribution).__name__}-{session_id}")

  async def prime_aggregator(self, session_id: str) -> int:
    """Ensure the live aggregator exists before a durable append; return its epoch.

    The announce path pairs this with ``announce_appended_event``: an
    aggregator initialized BEFORE the append cannot have consumed it, so the
    announce feed is never a duplicate. The epoch detects a rebuild that
    happened in between (its catch-up already covers the event).
    """
    await self._get_or_init_aggregator(session_id)
    return self._aggregator_epoch.get(session_id, 0)

  async def announce_appended_event(self, session_id: str, event: dict, *, epoch: int) -> None:
    """Broadcast one ALREADY-PERSISTED event through the live aggregator.

    The durable append happened before this call and outside this method; a
    notification failure here is repaired by catch-up/reconciliation, never by
    persisting a second copy. A rebuilt aggregator (epoch moved) has already
    consumed the event during its catch-up, so the feed is skipped.
    """
    aggregator = self._aggregators.get(session_id)
    if aggregator is None or self._aggregator_epoch.get(session_id, 0) != epoch:
      log.debug("announce_skipped_rebuilt_aggregator", session_id=session_id, type=event.get("type"))
      return
    try:
      meta = await self._store.get_session(session_id)
      await self._feed_and_broadcast(session_id, event, aggregator, meta.archive_offset if meta else 0)
    except Exception:
      # The event is already durable; a notification failure is repaired by
      # catch-up/reconciliation, never by persisting a second copy.
      log.exception("announce_failed", session_id=session_id, type=event.get("type"))

  async def broadcast_task_tree_changed(self, session_id: str, event_type: str | None) -> None:
    """Notify connected UIs that one node's durable task facts changed.

    Sidebar-channel shape (``type``/``session_id`` + ``fact_type``), delivered
    to every open session socket. The notification carries no task state —
    clients re-read the affected rows through the tree/read APIs, so a
    duplicate or out-of-order delivery changes nothing.
    """
    await self.broadcast_sidebar(session_id, ET.TASK_TREE_CHANGED, fact_type=event_type)

  async def broadcast_only(self, session_id: str, event: dict) -> None:
    """Broadcast an event on the session channel without persisting it as a chat event.

    Used for state-change notifications (e.g. ``plan_updated``) that must not
    pollute the chat history or replay on reconnect.
    """
    channel = session_channel(session_id)
    await streaming_manager.broadcast(channel, event)

  async def _get_or_init_aggregator(self, session_id: str) -> MessageAggregator:
    """Return the live aggregator for *session_id*, lazy-initialized from disk.

    On first use after server start, the aggregator catches up to the current
    on-disk state by silently consuming all persisted events. Their deltas are
    discarded -- subscribed clients have already rendered them via SSR or
    SPA-switch which both use the same aggregator logic. The corpus load runs
    in a thread (a cold parse is one C-heavy pass); the feed runs on the event
    loop in slices with a yield between slices -- the `_CatchupWalk` shape
    (server.py) -- so no single span parks the loop behind GIL handoffs for
    the init's full duration.

    A drop landing while the init runs must win: the epoch read at the start
    is re-checked after every slice-boundary yield (the yield sits between the
    slice's feed and the re-check), so a drop landing in the load, in a feed,
    or in a boundary yield discards the unfinished init -- its events cache
    re-primed by the dropped run is cleared -- and the init reruns against the
    new state; the final re-check and the publication share no yield.
    """
    aggregator = self._aggregators.get(session_id)
    if aggregator is not None:
      return aggregator
    while True:
      epoch = self._aggregator_epoch.get(session_id, 0)
      lock = lock_for(self._aggregator_init_locks, session_id)
      async with lock:
        # A concurrent first event for the same session may have finished the
        # init while this caller waited on the lock.
        aggregator = self._aggregators.get(session_id)
        if aggregator is not None:
          return aggregator
        aggregator = await self._init_live_aggregator(session_id, epoch)
        if aggregator is None:
          self.chat_events.clear_cache(session_id)
          continue
        self._aggregators[session_id] = aggregator
        return aggregator

  def _load_aggregator_init_inputs(self, session_id: str) -> tuple[list[dict], int]:
    """Load the init corpus off the event loop: the events and the aggregator's index offset."""
    return self.load_chat_events_sync(session_id), self.chat_events.read_archive_offset_sync(session_id)

  async def _init_live_aggregator(self, session_id: str, epoch: int) -> MessageAggregator | None:
    """Build and catch up the live aggregator; None when a drop won mid-init.

    Live-file events are fed in `_AGGREGATOR_INIT_SLICE_EVENTS` slices with a
    yield between slices. The slice loop re-reads the list length, so an
    append that lands mid-feed is fed like the threaded form's list iteration
    reached it; a drop instead aborts at the next slice boundary and the
    caller's epoch-moved path reruns.
    """
    # The init parses and folds the whole live corpus in one bounded span, and
    # the generational passes its dict churn triggers paused the event loop
    # up to ~74 ms at the session's first streamed event after a server start
    # (measured 20534-event worst corpus, 2026-09-15). GC is process-global
    # and the init runs on server threads, so the disable spans the whole init.
    # The parse's dicts stay referenced by the events cache and the feed's
    # discards refcount-clear, so no collect rides the re-enable.
    with gc_off(collect=False):
      return await self._catch_up_aggregator(session_id, epoch)

  async def _catch_up_aggregator(self, session_id: str, epoch: int) -> MessageAggregator | None:
    """Catch up one aggregator to the on-disk corpus; called under the init's gc boundary."""
    # Live file only holds events from index archive_offset onward; seed the
    # aggregator's offset so the deltas it emits carry the same GLOBAL
    # event_index that persist_and_broadcast stamps on the raw event.
    events, archive_offset = await asyncio.to_thread(self._load_aggregator_init_inputs, session_id)
    aggregator = MessageAggregator(event_index_offset=archive_offset, emit_stream_deltas=False)
    start = 0
    while start < len(events):
      end = min(start + _AGGREGATOR_INIT_SLICE_EVENTS, len(events))
      for ev in events[start:end]:
        for _ in aggregator.feed(ev):
          pass
      start = end
      # The yield sits before the re-check, so the check the publication
      # follows is never separated from its feed by a yield: a drop landing
      # in the load, in a feed, or in the boundary yield itself is detected
      # here, and no drop window survives between the last check and the
      # return below.
      await asyncio.sleep(0)
      if self._aggregator_epoch.get(session_id, 0) != epoch:
        return None
    # The same instance carries the live feed after the catch-up, so the
    # stream-delta suppression above must not survive publication.
    aggregator.emit_stream_deltas = True
    return aggregator

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up.

    See ``src/runtime/chat_events.py`` for the cache contract.
    """
    return self.chat_events.load_chat_events_sync(session_id)

  def load_chat_events_tail(self, session_id: str, limit: int) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk, bypassing the read-through cache.

    See ``src/runtime/chat_events.py`` for the return shape.
    """
    return self.chat_events.load_chat_events_tail(session_id, limit)

  def get_chat_event_count_sync(self, session_id: str, session_meta: SessionMetadata | None = None) -> int:
    """Return the current global chat event count without parsing event payloads.

    See ``src/runtime/chat_events.py`` for the count's index-space contract.
    """
    return self.chat_events.get_chat_event_count_sync(session_id, session_meta)

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end).

    See ``src/runtime/chat_events.py`` for the index and archive contract.
    """
    return self.chat_events.load_chat_events_range(session_id, start, end)

  def get_message_projection(self, session_id: str) -> MessageProjection | None:
    """Return the memoized message-list projection for *session_id*.

    Lazily builds from ``events_to_view(load_chat_events_sync(session_id))``
    and caches per session (LRU, cap ``_PROJECTION_LRU_LIMIT``); appends
    advance the projection incrementally by atomically swapping in an
    advanced copy instead of rebuilding. Returns None when
    ``archive_offset != 0`` — those sessions fall back entirely to the
    event-index cursor path and never mix the two cursor domains.
    """
    if self.chat_events.read_archive_offset_sync(session_id) != 0:
      return None
    live = self.load_chat_events_sync(session_id)
    cached = self._projection_cache.get(session_id)
    if cached is not None and cached.event_count == len(live):
      return cached
    if cached is None or len(live) < cached.event_count:
      # A shrink means the live file was rewritten; the append-incremental
      # advance cannot roll state back, so only this path pays a full build.
      cached = MessageProjection(list(live))
    else:
      # Swapping one reference into the cache is atomic, and published
      # projections are immutable: concurrent advances race on copies and a
      # loser wastes work instead of corrupting shared state.
      cached = cached.advanced(live[cached.event_count:])
    self._projection_cache.store(session_id, cached)
    return cached

  def projection_memo_hit(self, session_id: str) -> MessageProjection | None:
    """Return the memoized projection when the getter's fast path provably applies, else None.

    Pure memory — a dict read, a cache peek, one len compare — so callers can
    answer a warm poll on the event loop and pay the executor round-trip only
    on a miss (cold events cache, appended or shrunk corpus), where
    ``get_message_projection`` reads or advances. A cache entry exists only
    for unarchived sessions (the getter returns None before storing for
    ``archive_offset != 0``), so a hit needs no archive-offset check.
    """
    cached = self._projection_cache.get(session_id)
    if cached is None:
      return None
    live = self.chat_events.peek_cached_events(session_id)
    if live is None or cached.event_count != len(live):
      return None
    return cached

  def drop_session_runtime_state(self, session_id: str) -> None:
    """Drop a session's live runtime state: chat-event cache, aggregator, projection, and each contribution's state."""
    self.chat_events.clear_cache(session_id)
    self._aggregators.pop(session_id, None)
    self._aggregator_init_locks.pop(session_id, None)
    self._aggregator_epoch[session_id] = self._aggregator_epoch.get(session_id, 0) + 1
    self._projection_cache.drop(session_id)
    for contribution in sidebar_contributions():
      contribution.drop_runtime_state(session_id)


# The process owner of the chat-event block; built on the first ``events()`` call.
_events: SessionEvents | None = None


def events() -> SessionEvents:
  """The process-wide session events block."""
  global _events
  if _events is None:
    _events = SessionEvents(get_config(), session_store.store())
  return _events


def set_events(replacement: SessionEvents | None) -> None:
  """Replace the process events singleton (tests); None restores lazy construction."""
  global _events
  _events = replacement
