"""Session anchors: the backend conversation anchors, the context and thinking state, the session usage.

``SessionAnchors`` owns the writes that keep a session's resume anchor honest: the stored native conversation id
(``cc_session_id``) with its start time, the backend, prompt hash and model that produced it, and the account label of
the pool account holding the transcript. Each write is a fresh read-modify-write under the session's lock that goes
through ``SessionStore.save_metadata(anchor_write=True)``, so a stale whole-object save cannot roll an anchor back.
The block also reads the context size for the account pool, persists ``updated_at`` for the thinking state, and
resolves the usage panel through ``SessionUsageResolver``. The reset-note helpers live here because they describe
a fresh native conversation. The process builds one block (``anchors()``); tests build their own and install it with
``set_anchors()``.
"""

import asyncio
from collections.abc import Callable
from datetime import datetime

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig, get_config
from src.infra.models import SessionMetadata, parse_utc_datetime, utc_now
from src.runtime import session_events, session_store
from src.runtime.hooks import backend_types
from src.runtime.session_fork import HISTORY_LOCATION_NOTE
from src.runtime.session_usage import SessionUsageResolver

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


class SessionAnchors:
  """Session anchors, context state, thinking state and usage over the store and events blocks."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._events = events
    self._session_usage = SessionUsageResolver(
        cfg,
        events.chat_events.events_cache,
        events.get_chat_events_path,
        events.load_chat_events_sync,
    )

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
    async with self._store.lock_for(session_id):
      fresh = await self._store.get_session_bypassing_cache(session_id)
      if fresh is None:
        return None
      if mutate(fresh):
        await self._store.save_metadata(fresh, lock_held=True, anchor_write=True)
      return await self._store.get_session_bypassing_cache(session_id)

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
      for ev in reversed(self._events.load_chat_events_sync(session_id)):
        if ev.get("type") in (ET.ASSISTANT, ET.RESULT) and isinstance(ev.get("timestamp"), str):
          try:
            return parse_utc_datetime(ev["timestamp"])
          except ValueError:
            continue
      return None

    return context_tokens, await asyncio.to_thread(newest_request)

  async def update_thinking_state(self, session_id: str, updated_at: datetime) -> None:
    """Persist updated_at without clobbering unrelated fields."""
    await self._store.save_field_fresh(session_id, "updated_at", updated_at)

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


# The process owner of the anchors block; built on the first ``anchors()`` call.
_anchors: SessionAnchors | None = None


def anchors() -> SessionAnchors:
  """The process-wide session anchors block."""
  global _anchors
  if _anchors is None:
    _anchors = SessionAnchors(get_config(), session_store.store(), session_events.events())
  return _anchors


def set_anchors(replacement: SessionAnchors | None) -> None:
  """Replace the process anchors singleton (tests); None restores lazy construction."""
  global _anchors
  _anchors = replacement
