"""Session successor: the succession chain from an archived session to the session that took over its work.

``SessionSuccessor.resolve_successor_chain`` follows ``successor_session_id`` (written by an elone) from a session to
the end of its chain; ``deliver_to_successor`` persists an event into that chain end. Both read metadata fresh from
disk, because a TTL-cached read may predate a concurrent elone. The process builds one block (``successor()``); tests
build their own and install it with ``set_successor()``.
"""

from src.infra.config import CharlieBotConfig, get_config
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionMetadata
from src.runtime import session_events, session_store

log = LazyStructlogLogger()

# Bounds both the successor-chain walk (a cycle must never spin) and the
# delivery retry loop that re-resolves racing elones, keeping the two in agreement.
_SUCCESSOR_CHAIN_HOP_LIMIT = 100


class SessionSuccessor:
  """Successor-chain reads and delivery over the store and events blocks."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._events = events

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
    current = await self._store.read_metadata_fresh(session_id)
    if current is None:
      return None
    hops = 0
    while current.successor_session_id is not None:
      if hops >= _SUCCESSOR_CHAIN_HOP_LIMIT:
        raise RuntimeError(
            f"successor chain from {session_id} exceeds {_SUCCESSOR_CHAIN_HOP_LIMIT} hops;"
            " aborting to avoid a cycle")
      successor_id = current.successor_session_id
      successor = await self._store.read_metadata_fresh(successor_id)
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

      async with self._store.lock_for(tail.id):
        # Re-resolve from the tail with a fresh read: an elone may have landed
        # while we waited for the lock. If so, release and repeat from the top.
        fresh_tail = await self._store.read_metadata_fresh(tail.id)
        if fresh_tail is None:
          log.error("deliver_to_successor_tail_missing", session_id=session_id, tail_id=tail.id)
          return None
        if fresh_tail.successor_session_id is not None:
          continue
        # The tail's metadata.json exists (confirmed above), so append_ndjson will
        # never recreate a directory for a permanently deleted session.
        if not self._store.metadata_path(fresh_tail.id).exists():
          log.error(
              "deliver_to_successor_metadata_missing",
              session_id=session_id,
              tail_id=fresh_tail.id,
          )
          return None

        if fresh_tail.id != session_id:
          event["origin_session_id"] = session_id
        await self._events.persist_and_broadcast(fresh_tail.id, event)
        return fresh_tail.id

    raise RuntimeError(
        f"deliver_to_successor from {session_id} retried {_SUCCESSOR_CHAIN_HOP_LIMIT}"
        " times; aborting to avoid a loop")


# The process owner of the successor block; built on the first ``successor()`` call.
_successor: SessionSuccessor | None = None


def successor() -> SessionSuccessor:
  """The process-wide session successor block."""
  global _successor
  if _successor is None:
    _successor = SessionSuccessor(get_config(), session_store.store(), session_events.events())
  return _successor


def set_successor(replacement: SessionSuccessor | None) -> None:
  """Replace the process successor singleton (tests); None restores lazy construction."""
  global _successor
  _successor = replacement
