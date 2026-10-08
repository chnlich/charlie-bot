"""Session management for CharlieBot."""

import asyncio
from collections.abc import Callable
from datetime import datetime

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig, get_config
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import (
    SessionCallbacks,
    SessionMetadata,
    parse_utc_datetime,
    utc_now,
)
from src.runtime import (
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_search,
    session_sidebar,
    session_store,
)
from src.runtime.hooks import backend_types
from src.runtime.session_fork import HISTORY_LOCATION_NOTE
from src.runtime.session_usage import SessionUsageResolver

log = LazyStructlogLogger()

# The clone-style instruction context-reset notes append after the
# history note: the fresh native conversation must read the chat log first and
# open its reply with where things stand, instead of silently losing every
# convention the earlier turns set.
CONTEXT_RESET_INSTRUCTION = (
    "Get oriented from that log before you respond: open your reply with two or "
    "three sentences on where things stand and the conventions in force, then "
    "respond to what follows.")


def backend_switch_reset_reason(native_backend: str | None, option_id: str) -> str:
  """The reset note's reason when the backend change itself forces the fresh conversation.

  The master CC and task-tree turn-start rules share this one home,
  so their notes cannot drift apart.
  """
  return (f"this session switched from backend {native_backend} to {option_id}, "
          "which starts its own conversation")


def context_reset_note(reason: str, task_goal: str | None = None) -> str:
  """The whole bracketed note that opens a fresh native conversation's prompt.

  The one assembly site for task-tree turn-start notes: the reason names why the previous
  conversation was left, the history note says where that history stays
  readable, and the instruction tells the new model to read the log and open
  with where things stand before responding. The task goal names what the
  fresh context is working on.
  """
  task_part = "" if task_goal is None else f" The task is: {task_goal}."
  return (f"[Context reset: {reason}.{task_part} {HISTORY_LOCATION_NOTE} "
          f"{CONTEXT_RESET_INSTRUCTION}]")


# Bounds both the successor-chain walk (a cycle must never spin) and the
# delivery retry loop that re-resolves racing elones, keeping the two in agreement.
_SUCCESSOR_CHAIN_HOP_LIMIT = 100


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
      sidebar: session_sidebar.SessionSidebar,
      listing: session_listing.SessionListing,
      search: session_search.SessionSearch,
      lifecycle: session_lifecycle.SessionLifecycle,
      fork: session_fork.SessionFork,
  ) -> None:
    self._cfg = cfg
    self.task_tree_manager = None
    self.store = store
    self.events = events
    self.sidebar = sidebar
    self.listing = listing
    self.search = search
    self.lifecycle = lifecycle
    self.fork = fork
    events.turn_sessions = self
    self._session_usage = SessionUsageResolver(
        cfg,
        events.chat_events.events_cache,
        events.get_chat_events_path,
        events.load_chat_events_sync,
    )

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def resolve_successor_chain(self, session_id: str) -> SessionMetadata | None:
    """Walk ``successor_session_id`` from *session_id* to the chain end.

    Uses ``read_metadata_fresh`` at every hop (never the TTL cache, which may
    be stale relative to a concurrent elone). Returns the chain-end metadata, or
    None when *session_id* itself has no metadata on disk. When a hop's
    successor id has no metadata (that session was permanently deleted), stops
    and returns the last session that does exist, logging one structured error.
    Raises RuntimeError if the chain exceeds ``_SUCCESSOR_CHAIN_HOP_LIMIT``
    hops (a cycle must never spin).
    """
    current = await self.store.read_metadata_fresh(session_id)
    if current is None:
      return None
    hops = 0
    while current.successor_session_id is not None:
      if hops >= _SUCCESSOR_CHAIN_HOP_LIMIT:
        raise RuntimeError(
            f"successor chain from {session_id} exceeds {_SUCCESSOR_CHAIN_HOP_LIMIT} hops;"
            " aborting to avoid a cycle")
      successor_id = current.successor_session_id
      successor = await self.store.read_metadata_fresh(successor_id)
      if successor is None:
        log.error(
            "successor_chain_broken",
            session_id=session_id,
            missing_session_id=successor_id,
        )
        return current
      current = successor
      hops += 1
    return current

  async def deliver_to_successor(self, session_id: str, event: dict) -> str | None:
    """Persist *event* into the session currently ending *session_id*'s succession chain.

    Resolves the chain end via ``resolve_successor_chain``, then holds that tail's
    lock while re-resolving (an elone may have landed while we waited), confirming the
    tail's directory still exists, and persisting. Returns the id of the session
    actually written to, or None when nothing could be written (chain end's metadata
    was permanently deleted). Redirected events (chain end differs from *session_id*)
    are stamped with ``event['origin_session_id'] = session_id``.
    """
    # One opening attempt plus up to _SUCCESSOR_CHAIN_HOP_LIMIT tail-chases;
    # fail loud rather than looping forever on a steady stream of landing elones.
    for _ in range(_SUCCESSOR_CHAIN_HOP_LIMIT + 1):
      tail = await self.resolve_successor_chain(session_id)
      if tail is None:
        log.error("deliver_to_successor_origin_missing", session_id=session_id)
        return None

      async with self.store.lock_for(tail.id):
        # Re-resolve from the tail with a fresh read: an elone may have landed
        # while we waited for the lock. If so, release and repeat from the top.
        fresh_tail = await self.store.read_metadata_fresh(tail.id)
        if fresh_tail is None:
          log.error("deliver_to_successor_tail_missing", session_id=session_id, tail_id=tail.id)
          return None
        if fresh_tail.successor_session_id is not None:
          continue
        # The tail's metadata.json exists (confirmed above), so append_ndjson will
        # never recreate a directory for a permanently deleted session.
        if not self.store.metadata_path(fresh_tail.id).exists():
          log.error(
              "deliver_to_successor_metadata_missing",
              session_id=session_id,
              tail_id=fresh_tail.id,
          )
          return None

        if fresh_tail.id != session_id:
          event["origin_session_id"] = session_id
        await self.events.persist_and_broadcast(fresh_tail.id, event)
        return fresh_tail.id

    raise RuntimeError(
        f"deliver_to_successor from {session_id} retried {_SUCCESSOR_CHAIN_HOP_LIMIT}"
        " times; aborting to avoid a loop")

  async def _persist_anchor_fresh(
      self, session_id: str, mutate: Callable[[SessionMetadata], bool]) -> SessionMetadata | None:
    """Run one authorized anchor channel: fresh read-modify-write under the per-session lock.

    *mutate* projects the new anchor values onto the fresh metadata and returns
    whether anything changed — only a changed save goes to disk. The read-back
    re-reads disk under the same lock, so what a channel returns is the on-disk
    state at return time, not the written value (a concurrent writer landing in
    between wins). ``anchor_write=True`` marks the save an authorized anchor
    channel (see ``save_metadata``). Returns the re-read metadata, None when
    the session does not exist.
    """
    async with self.store.lock_for(session_id):
      fresh = await self.store.get_session_bypassing_cache(session_id)
      if fresh is None:
        return None
      if mutate(fresh):
        await self.store.save_metadata(fresh, lock_held=True, anchor_write=True)
      return await self.store.get_session_bypassing_cache(session_id)

  async def persist_cc_session_id(
      self, session_id: str, cc_session_id: str, *, native_backend: str | None = None) -> str | None:
    """Persist a cc_session_id without clobbering unrelated metadata fields.

    Only ``cc_session_id`` changes (and ``cc_session_started_at`` when the
    on-disk id actually changes), plus ``native_backend`` when the caller hands
    the producing backend id — the consumer's round-end write, so the
    continuation rule knows which backend the id belongs to; ``None`` (the
    default) leaves that field untouched, keeping the two-argument callers
    working. The save runs on every call, id changed or not — the consumer owns
    the resume anchor and hands it every round (the persist-with-readback step
    in ``master_cc_queue``). Never falls back to a whole-object save — that
    clobbers concurrent single-field writes like ``has_unread``.
    """

    def set_anchor(meta: SessionMetadata) -> bool:
      if meta.cc_session_id != cc_session_id:
        meta.cc_session_id = cc_session_id
        meta.cc_session_started_at = utc_now()
      if native_backend is not None:
        meta.native_backend = native_backend
      return True

    saved = await self._persist_anchor_fresh(session_id, set_anchor)
    return saved.cc_session_id if saved is not None else None

  async def persist_native_backend(self, session_id: str, native_backend: str) -> str | None:
    """Record the backend that produced the session's held native conversation.

    The switch endpoint's pre-rule backfill: a session from before the
    native_backend rule holds a ``cc_session_id`` with an empty
    ``native_backend``, and switching records the effective current backend as
    its producer through this authorized anchor channel *before* the backend
    field changes, so the next turn's continuation rule judges the held id
    against the backend that actually produced it. Only ``native_backend``
    changes, so concurrent single-field writes survive; an unchanged value
    writes nothing.
    """

    def set_backend(meta: SessionMetadata) -> bool:
      if meta.native_backend == native_backend:
        return False
      meta.native_backend = native_backend
      return True

    saved = await self._persist_anchor_fresh(session_id, set_backend)
    return saved.native_backend if saved is not None else None

  async def persist_native_anchor_provenance(
      self, session_id: str, *, prompt_hash: str, native_backend: str, model: str | None) -> None:
    """Write the native-context anchor's provenance through an authorized anchor write.

    The v2 launch's spawn-time channel (``TaskTreeManager.record_native_anchor``):
    a fresh read under the per-session lock sets the instruction hash and the
    backend/model identity the current conversation continues under in one
    authorized save, so a concurrent stale whole-object save cannot roll the
    identity back to a previous conversation's provenance. The clear of a
    deliberately voided anchor stays on ``clear_cc_session_anchor``.
    """

    def set_provenance(meta: SessionMetadata) -> bool:
      meta.native_prompt_hash = prompt_hash
      meta.native_backend = native_backend
      meta.native_model = model
      return True

    await self._persist_anchor_fresh(session_id, set_provenance)

  async def persist_account_label(self, session_id: str, label: str) -> str | None:
    """Persist the pool account holding the session's transcript; returns the label on disk.

    The label is the backend lifecycles' account label: each lifecycle that keeps one records it
    (``backend_types.record_account_label``). Only the label changes, so concurrent
    single-field writes survive; an unchanged label writes nothing.
    """

    saved = await self._persist_anchor_fresh(session_id, lambda meta: backend_types.record_account_label(meta, label))
    if saved is None:
      return None
    keeper = backend_types.account_keeper(saved)
    return keeper.account_label(saved) if keeper is not None else None

  async def clear_cc_session_anchor(self, session_id: str) -> None:
    """Intentionally clear the session's resume anchor; the authorized clear channel.

    The weekly recycle's deliberate fresh start goes through here -- an
    unchanneled whole-object save would leave the old anchor on disk (the guard
    corrects it back) and the recycle would silently do nothing behind its
    suppressed next-round alarm.
    """

    def clear(meta: SessionMetadata) -> bool:
      meta.cc_session_id = None
      meta.cc_session_started_at = None
      return True

    await self._persist_anchor_fresh(session_id, clear)

  async def context_state(self, session_id: str, session_meta: SessionMetadata) -> tuple[int | None, datetime | None]:
    """Context size and the time of the last model request, for the account pool.

    ``context_tokens`` is the usage panel's reading (``resolve_session_usage``);
    ``last_request_at`` is the timestamp of the newest assistant or result event,
    the moment the prompt cache was last renewed. Either is None when unknown.
    """
    usage = await self.resolve_session_usage(session_id, session_meta)
    context_tokens = usage.get(ET.CONTEXT_TOKENS) if isinstance(usage, dict) else None

    def newest_request() -> datetime | None:
      for ev in reversed(self.events.load_chat_events_sync(session_id)):
        if ev.get("type") in (ET.ASSISTANT, ET.RESULT) and isinstance(ev.get("timestamp"), str):
          try:
            return parse_utc_datetime(ev["timestamp"])
          except ValueError:
            continue
      return None

    return context_tokens, await asyncio.to_thread(newest_request)

  async def update_thinking_state(self, session_id: str, updated_at: datetime) -> None:
    """Persist updated_at without clobbering unrelated fields."""
    await self.store.save_field_fresh(session_id, "updated_at", updated_at)

  # ---------------------------------------------------------------------------
  # Callback bundle for run_message()
  # ---------------------------------------------------------------------------

  def callbacks(self) -> SessionCallbacks:
    """Return a bundle of session-related callbacks for run_message()."""
    return SessionCallbacks(
        persist_and_broadcast=self.events.persist_and_broadcast,
        mark_unread=self.lifecycle.mark_unread,
        persist_cc_session_id=self.persist_cc_session_id,
        persist_account_label=self.persist_account_label,
        context_state=self.context_state,
        task_tree_activity=self.sidebar.task_tree_activity,
    )

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
  ) -> dict | None:
    """Resolve display usage for a session view as a projection over its events.

    Usage is computed on demand as a fold over the chat-event stream whose
    state carries across resolutions in a per-session memo. See
    ``src/runtime/session_usage.py`` for the tier contract.
    """
    return await self._session_usage.resolve_session_usage(session_id, session_meta)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. using a persistent counter file.

    Reads the next number from sessions_dir/.counter (O(1) instead of listing
    all sessions). Falls back to counting directories when the counter is
    missing, unreadable, or unparsable.
    """
    counter_path = self._cfg.sessions_dir / ".counter"

    def _read_and_increment() -> int:
      self._cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
      # FileNotFoundError is an OSError, so a missing counter takes the same
      # count-dirs fallback as an unreadable or unparsable one; an exists()
      # pre-check would only open a check-then-read race.
      try:
        n = int(counter_path.read_text().strip())
      except (ValueError, OSError):
        n = self._count_session_dirs()
      counter_path.write_text(str(n + 1))
      return n

    n = await asyncio.to_thread(_read_and_increment)
    return f"Session {n}"

  def _count_session_dirs(self) -> int:
    """Count existing session directories for backward-compat counter init."""
    if not self._cfg.sessions_dir.exists():
      return 0
    return sum(1 for d in self._cfg.sessions_dir.iterdir() if d.is_dir())


# The process owner of the session manager; built on the first ``session_manager()`` call.
_session_manager: SessionManager | None = None


def session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(
        get_config(),
        session_store.store(),
        session_events.events(),
        session_sidebar.sidebar(),
        session_listing.listing(),
        session_search.search(),
        session_lifecycle.lifecycle(),
        session_fork.fork(),
    )
  return _session_manager
