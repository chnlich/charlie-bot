"""Session management for CharlieBot."""

import asyncio
import json
import mmap
import os
import shutil
from collections.abc import Callable
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, BinaryIO

from src.infra import event_types as ET

if TYPE_CHECKING:
  import numpy as np
from src.infra.config import CharlieBotConfig, get_config
from src.infra.json_utils import atomic_write_stream
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import (
    EventRef,
    SessionCallbacks,
    SessionMetadata,
    SessionStatus,
    parse_utc_datetime,
    utc_now,
    utc_now_iso,
)
from src.infra.process import cleanup_session_cgroup
from src.runtime import (
    session_events,
    session_listing,
    session_search,
    session_sidebar,
    session_store,
    sidebar_state,
)
from src.runtime.chat_events import ARCHIVE_FILE_GLOB, chat_event_archives_dir
from src.runtime.control_events import ACTOR_USER, build_task_created_event
from src.runtime.hooks import backend_types
from src.runtime.hooks.sidebar_contributions import sidebar_contributions
from src.runtime.session_usage import SessionUsageResolver

log = LazyStructlogLogger()

# The fork/elone API routes (src/runtime/api/sessions.py) open their auto-injected
# bootstrap prompts with an opener plus the note, and a feature that summarizes
# sessions filters such injected messages by opener-prefix match; the clone/elone
# bootstraps and the context-reset note (context_reset_note below) share the
# history note. Both sides import this one copy so an edit cannot drift them apart.
FORK_BOOTSTRAP_OPENER = "This session continues a prior conversation."
ELONE_BOOTSTRAP_OPENER = "You're taking over because the user wasn't satisfied with the previous session."
HISTORY_LOCATION_NOTE = "Earlier turns' history remains readable in this session's chat log, data/chat_events.jsonl in the working directory."
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

_REFERENCE_LINE_WS = b" \t\r\n\x0b\x0c"
# Newline detection is position-local, so the frame scan can sweep in chunks
# without changing its result; the 1 MiB chunks keep the compare's bool scratch
# in cache where a whole-file mask allocates one bool per source byte.
_REFERENCE_SCAN_CHUNK = 1 << 20


def _reference_scan(arr: np.ndarray) -> tuple[np.ndarray, bool]:
  """Return ``(0x0A positions, every byte <= 0x7F)`` for the corpus's uint8 view.

  Both reductions are position-local, so one chunked sweep answers them
  together: the ASCII reduction reads the window the newline reduction just
  pulled into cache instead of paying a second whole-corpus pass (the fork of
  a gigabyte-class corpus is memory-bandwidth-bound; measured 140 -> 117 ms
  on the 1051.3 MB heaviest fork corpus). The ASCII verdict must still cover
  every byte: it is the proof that skips the utf-8 decode whose
  UnicodeDecodeError an undecodable corpus must raise.
  """
  # numpy rides the fork's history-copy stream (the M99 server import floor):
  # the module sits on the sessions chain every server start pulls, and the
  # vectorized scan serves only this copy fast path.
  import numpy as np
  parts: list[np.ndarray] = []
  ascii_ok = True
  for offset in range(0, arr.size, _REFERENCE_SCAN_CHUNK):
    window = arr[offset:min(offset + _REFERENCE_SCAN_CHUNK, arr.size)]
    found = np.flatnonzero(window == 0x0A)
    if found.size:
      parts.append(found + offset)
    if ascii_ok and window.max() > 0x7F:
      ascii_ok = False
  if not parts:
    return np.empty(0, dtype=np.intp), ascii_ok
  nls = parts[0] if len(parts) == 1 else np.concatenate(parts)
  return nls, ascii_ok


def _fast_reference_frames(data: bytes | mmap.mmap, take: int, nls: np.ndarray) -> tuple[int, int, int, bool] | None:
  """Vectorized frame check for the first ``take`` raw lines of ``data``.

  ``nls`` is the corpus's 0x0A positions from the caller's one sweep
  (:func:`_reference_scan`) — this check only slices them, and recomputing
  here would buy the sweep's whole-corpus pass back.

  Returns ``(raw, start, end, needs_newline)`` when every in-budget frame is a
  non-blank ``{}``-wrapped line: the count of raw frames consumed and the
  ``[start, end)`` byte window whose content — plus one ``\\n`` when
  ``needs_newline`` — equals what a per-frame pass would append. ``None`` when
  any in-budget frame is blank, CR-terminated, or not ``{}``-wrapped; the
  caller's per-frame pass reports or folds those one by one.
  """
  import numpy as np
  arr = np.frombuffer(data, dtype=np.uint8)
  if take <= 0:
    return (0, 0, 0, False)
  terminated = nls[:take]
  starts = np.concatenate(([0], terminated[:-1] + 1)) if terminated.size else terminated
  ends = terminated
  needs_newline = False
  if terminated.size < take:
    tail_start = int(terminated[-1]) + 1 if terminated.size else 0
    if tail_start < len(data):
      starts = np.concatenate((starts, [tail_start]))
      ends = np.concatenate((ends, [len(data)]))
      needs_newline = True
  raw = starts.size
  if raw == 0:
    return (0, 0, 0, False)
  frame_ok = (ends > starts) & (arr[starts] == 0x7B) & (arr[ends - 1] == 0x7D)  # '{' ... '}'
  if not frame_ok.all():
    return None
  # Frame bytes plus their on-disk terminators are one contiguous window; only
  # an unterminated final frame lacks its separator inside it.
  end = int(ends[-1]) + (0 if needs_newline else 1)
  return (raw, int(starts[0]), end, needs_newline)


def _stream_reference_lines(out: BinaryIO, data: bytes | mmap.mmap, take: int) -> tuple[int, int]:
  """Copy the non-blank lines among the first ``take`` raw line frames of ``data`` into ``out``.

  Returns ``(raw, appended)``: raw frames spent against the budget (blank
  frames included) and lines written. Bytes move without per-line copies: a
  bulk vectorized pass answers the common all-``{}`` shape with one window
  write, and the per-frame fallback keeps CR folding and corrupt-line
  rejection exact. A corrupt corpus stays loud: a non-blank frame whose
  stripped content is not wrapped in ``{}`` raises, and so do undecodable
  bytes anywhere in the file.
  """
  # Validity gate: undecodable bytes must raise UnicodeDecodeError, and the
  # decoded result is otherwise unused. ASCII bytes are always valid UTF-8, so
  # an ASCII proof passes validity and only a non-ASCII corpus pays the full
  # decode. An mmap lacks isascii(); the fused sweep answers for the whole
  # mapping, and a non-ASCII mapping materializes once so the decode raises
  # the identical error.
  import numpy as np
  if isinstance(data, mmap.mmap):
    nls, ascii_ok = _reference_scan(np.frombuffer(data, dtype=np.uint8))
    if not ascii_ok:
      data = bytes(data)
      data.decode("utf-8")
  else:
    nls = _reference_scan(np.frombuffer(data, dtype=np.uint8))[0]
    if not data.isascii():
      data.decode("utf-8")
  fast = _fast_reference_frames(data, take, nls)
  if fast is not None:
    raw, start, end, needs_newline = fast
    if end > start:
      # The window's exported pointer must not outlive the write: the source's
      # mmap closes after this call, and closing refuses while a view exists.
      window = memoryview(data)[start:end]
      try:
        out.write(window)
      finally:
        window.release()
      if needs_newline:
        out.write(b"\n")
    return raw, raw

  pos = 0
  raw = 0
  appended = 0
  end_of_data = len(data)
  while raw < take and pos < end_of_data:
    newline = data.find(b"\n", pos)
    end = end_of_data if newline < 0 else newline
    raw += 1
    first = pos
    while first < end and data[first] in _REFERENCE_LINE_WS:
      first += 1
    if first < end:
      last = end - 1
      while data[last] in _REFERENCE_LINE_WS:
        last -= 1
      if data[first] != ord("{") or data[last] != ord("}"):
        snippet = data[pos:end].decode("utf-8", errors="replace").strip()[:80]
        raise ValueError(f"parent event line is not a serialized event object: {snippet!r}")
      # The CR of a CRLF pair folds before the write; every other
      # original byte (edge whitespace included) is kept. The per-line slice
      # keeps no pointer exported past the write (an mmap closes after this
      # stream returns, and closing refuses while a view exists).
      write_end = end - 1 if end > pos and data[end - 1] == 0x0D else end
      out.write(data[pos:write_end])
      out.write(b"\n")
      appended += 1
    pos = end + 1
  return raw, appended


def _stream_reference_file(out: BinaryIO, source: Path, take: int) -> tuple[int, int]:
  """Stream one parent source's first ``take`` raw line frames into ``out``.

  The source rides an mmap: the frame scan and the window write read the
  mapping directly, so the corpus never enters the Python heap as one object —
  the gigabyte-class live files pay the scan's memory bandwidth, not a
  whole-file memcpy. Chat files mutate only by append between archive rewrites
  and rewrites publish through ``os.replace`` (the ChatEventStore's stated
  rule), so the mapping holds an append-only or already-unlinked inode and
  never truncates under it.
  """
  with source.open("rb") as handle:
    if os.fstat(handle.fileno()).st_size == 0:
      return _stream_reference_lines(out, b"", take)  # mmap refuses an empty file
    mapping = mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_READ)
    try:
      return _stream_reference_lines(out, mapping, take)
    finally:
      mapping.close()


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
  ) -> None:
    self._cfg = cfg
    self.task_tree_manager = None
    self.store = store
    self.events = events
    self.sidebar = sidebar
    self.listing = listing
    self.search = search
    events.turn_sessions = self
    # The task-tree owner registers its projection invalidator here at wiring
    # time (TaskTreeManager.__init__). SessionManager-level writes that move a
    # tree-projection input must drop the tree's rebuildable index — the same
    # policy the task owner applies to its own metadata writes; None only
    # before that wiring exists (no tree consumer constructed in this process).
    self.tree_index_invalidator: Callable[[], None] | None = None
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

  async def fork_session(
      self,
      parent_id: str,
      event_index: int | None = None,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create a new session whose chat log opens with the parent's raw event lines."""
    meta = await self._spawn_with_history(parent_id, event_index, backend, "C")
    self._log_spawn("session_cloned", meta, parent_id, event_index)
    return meta

  async def elone_session(
      self,
      parent_id: str,
      event_index: int,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create an Elon-e session: the child's log opens with the parent's raw
    event lines, the parent is archived.

    The per-parent invariant: each elone overwrites ``successor_session_id`` so
    the pointer names the parent's most recent elone child, and consumers (chain
    resolution, delivery, trigger redirect) follow the pointer and therefore
    land at the most recent takeover. Scheduled tasks have no succession here:
    their bound node is the task's stable binding, and an elone of it is an
    ordinary fork — the task keeps firing on the original node.
    """
    fresh_parent = await self.store.read_metadata_fresh(parent_id)
    if fresh_parent is None:
      raise FileNotFoundError(f"parent session not found: {parent_id}")
    meta = await self._spawn_with_history(parent_id, event_index, backend, "E")

    # Auto-archive the parent, and record the elone successor
    # pointer (re-read under lock so concurrent mutations to the parent aren't
    # clobbered). Latest-wins: a parent's pointer is overwritten to name each
    # new child, so the pointer always names the most recent elone.
    async with self.store.lock_for(parent_id):
      fresh_parent = await self.store.get_session(parent_id)
      if fresh_parent:
        fresh_parent.status = SessionStatus.ARCHIVED
        fresh_parent.successor_session_id = meta.id
        fresh_parent.updated_at = utc_now()
        await self.store.save_metadata(fresh_parent, lock_held=True)
    self.events.drop_session_runtime_state(parent_id)

    self._log_spawn("session_eloned", meta, parent_id, event_index)
    return meta

  async def _spawn_with_history(
      self,
      parent_id: str,
      event_index: int | None,
      backend: str | None,
      name_prefix: str,
  ) -> SessionMetadata:
    """Create a child manager root whose chat log opens with the parent's raw event lines.

    The child's ``data/chat_events.jsonl`` first holds the parent's raw event
    lines for ``[0, end)`` (``end`` is ``event_index + 1`` at a cut point, else
    the parent's event count), then the ``clone_start`` marker, then the
    child's ``task_created`` fact; the child appends its own events after it.
    One history per session, in the file the model reads in place — the same
    log the parent grepped. The parent is a history source only: the child
    carries no ``task_parent_id``.
    """
    parent = await self.store.get_session(parent_id)
    if not parent:
      raise FileNotFoundError(f"parent session not found: {parent_id}")

    count = await asyncio.to_thread(self.events.get_chat_event_count_sync, parent_id, parent)
    if event_index is None:
      end = count
    else:
      if event_index < 0 or event_index >= count:
        raise ValueError(f"event_index {event_index} out of range for parent session {parent_id} with {count} events")
      end = event_index + 1

    meta = SessionMetadata(
        name=f"{name_prefix}{parent.name}",
        schema_version=2,
        profile="manager",
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    created_event = build_task_created_event(
        actor=ACTOR_USER,
        task_id=meta.id,
        request_id=f"history-copy-{meta.id}",
        task_parent_id=None,
        task_spec_hash=None,
    )
    meta.created_by_event = EventRef(session_id=meta.id, event_id=str(created_event["id"]))
    session_dir = self.store.session_dir(meta.id)
    self._create_session_dirs(session_dir)

    events_path = self.events.get_chat_events_path(meta.id)
    clone_event = {
        "type": ET.CLONE_START,
        "parent_session_id": parent_id,
        "parent_session_name": parent.name,
        "timestamp": utc_now_iso(),
    }
    # The child log is born whole — prefix, marker, creation fact — through one
    # atomic stream. An append after the copy would fdatasync the entire corpus
    # inside the fork (measured 10.4 s against the 0.4 s copy on the 1 GB
    # heaviest live corpus), so the born history and its tail share the copied
    # bytes' writeback class; the child's own appends keep the durable funnel.
    tail_lines = "".join(json.dumps(event, ensure_ascii=False) + "\n" for event in (clone_event, created_event))
    await asyncio.to_thread(self._write_history_file_sync, events_path, parent_id, end, tail_lines)
    await self.store.save_metadata(meta)
    if self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    await asyncio.to_thread(self._copy_on_fork_sync, self.store.session_dir(parent_id), session_dir)
    # A contribution's copy lands after save_metadata, and a poll racing between the two
    # could have snapshotted the child without the copied files; re-mark so they are always probed.
    sidebar_state.mark_sidebar_dirty(meta.id)
    await self.events.broadcast_task_tree_changed(meta.id, ET.TASK_CREATED)
    return session_store.stamp_thinking_since(meta)

  @staticmethod
  def _copy_on_fork_sync(parent_dir: Path, child_dir: Path) -> None:
    """Let every sidebar contribution copy its files from the parent's session directory into the child's."""
    for contribution in sidebar_contributions():
      contribution.copy_on_fork(parent_dir, child_dir)

  def _write_history_file_sync(self, path: Path, parent_id: str, end: int, tail_lines: str) -> None:
    """Write the parent's raw event lines for ``[0, end)`` plus *tail_lines*
    (the clone-start marker and the child's creation fact) into ``path``.

    Streams the non-blank lines among the first ``archive_take`` raw archive
    lines (``data/archives/`` in the chronological filename glob) followed by
    the non-blank lines among the first ``end - archive_take`` raw live lines,
    with ``archive_take`` the write-time archive offset capped at ``end``. This
    is the event sequence ``load_chat_events_range`` parses for the range
    ``[0, end)``: both sides read the offset when the read starts — an archive
    pass landing between the caller's event count and this write moves lines
    from the live file to the archive tail, changing the split but not the
    sequence — and both spend raw lines against the budget and skip blanks.
    The corrupt-corpus failures a parsed write would surface through its
    length check stay loud here without paying the parse
    (:func:`_stream_reference_lines`), and so does a take that ends short.
    The clone path points ``path`` at the child's ``data/chat_events.jsonl``:
    the cut point rides the same raw-line budget as the full corpus, so one
    copy path serves both.
    """
    archive_take = min(self.events.chat_events.read_archive_offset_sync(parent_id), end)
    live_take = end - archive_take
    parent_dir = self.store.session_dir(parent_id)
    path.parent.mkdir(parents=True, exist_ok=True)

    def _write(out: BinaryIO) -> None:
      archives_dir = chat_event_archives_dir(parent_dir)
      raw_left = archive_take
      archived = 0
      if raw_left and archives_dir.is_dir():
        for source in sorted(archives_dir.glob(ARCHIVE_FILE_GLOB)):
          if raw_left <= 0:
            break
          # A full-corpus budget spans nearly the whole file (only an archive
          # pass mid-fork shrinks a take below the line count), so the mapping
          # over-covers no bytes worth chunk-accumulating against.
          raw, appended = _stream_reference_file(out, source, raw_left)
          raw_left -= raw
          archived += appended
      if archived != archive_take:
        raise ValueError(f"loaded {archived} archived parent events for requested range [0, {archive_take})")

      live_path = self.events.get_chat_events_path(parent_id)
      live = 0
      if live_take and live_path.exists():
        _, live = _stream_reference_file(out, live_path, live_take)
      if live != live_take:
        raise ValueError(f"loaded {live} live parent events for requested range [0, {live_take})")

      out.write(tail_lines.encode("utf-8"))

    atomic_write_stream(path, _write)

  @staticmethod
  def _create_session_dirs(session_dir: Path) -> None:
    (session_dir / "data").mkdir(parents=True, exist_ok=True)

  @staticmethod
  def _log_spawn(event: str, meta: SessionMetadata, parent_id: str, event_index: int | None) -> None:
    # fork_session and elone_session emit the identical payload shape,
    # distinguished only by event name, so one log-query shape covers both
    # spawn flows; the single helper is what keeps them agreeing.
    log.info(event, new_session=meta.id, parent=parent_id, event_index=event_index, backend=meta.backend)

  async def rename_session(self, session_id: str, new_name: str) -> SessionMetadata | None:
    """Rename a session and return the updated metadata."""
    return await self.store.update_field(session_id, "name", new_name, "session_renamed", new_name=new_name)

  async def switch_backend(self, session_id: str, backend: str) -> SessionMetadata | None:
    """Set the session's backend and return the updated metadata.

    Performs only the metadata write — the caller (API layer) is responsible
    for resume-domain validation and for persisting the audit event. Returns
    ``None`` when the session is missing. Broadcasts a sidebar update so other
    open tabs refresh their header.
    """
    meta = await self.store.update_field(session_id, "backend", backend, "session_backend_switched", backend=backend)
    if meta:
      await self.events.broadcast_sidebar(session_id, ET.BACKEND_SWITCHED, backend=backend)
    return meta

  async def mark_read(self, session_id: str) -> SessionMetadata | None:
    """Clear the unread flag for a session."""
    return await self._set_unread_flag(session_id, has_unread=False)

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when master/workers produce output)."""
    await self._set_unread_flag(session_id, has_unread=True)

  async def _set_unread_flag(self, session_id: str, has_unread: bool) -> SessionMetadata | None:
    """Write the unread flag and broadcast only when it actually flips.

    Unlike ``SessionStore.update_field`` this must not bump ``updated_at``: the sidebar
    sorts newest-first on that field, and a read/unread flip is not user
    activity worth reordering the list over.
    """
    async with self.store.lock_for(session_id):
      meta = await self.store.get_session(session_id)
      if not meta or meta.has_unread == has_unread:
        return meta
      meta.has_unread = has_unread
      await self.store.save_metadata(meta, lock_held=True)
      # The unread flag is a tree-row projection input (SessionRow.has_unread):
      # drop the rebuildable index so a tree page read after this flip is built
      # from the fresh flag, not from a pre-flip index snapshot.
      if self.tree_index_invalidator is not None:
        self.tree_index_invalidator()
    await self.events.broadcast_sidebar(session_id, ET.UNREAD_CHANGED, has_unread=has_unread)
    return meta

  async def archive_session(self, session_id: str) -> SessionMetadata | None:
    """Mark a session as archived (does not delete files).

    The stored status is a tree-projection input: an archived parent archives
    its task subtree through the read-time inheritance, so the write
    drops the rebuildable index through the same hook the unread flip uses:
    without it, a listing within _TREE_INDEX_TTL_SECONDS would compute the
    children's inherited state from the parent's pre-archive status.
    """
    meta = await self.store.update_field(session_id, "status", SessionStatus.ARCHIVED, "session_archived")
    self.events.drop_session_runtime_state(session_id)
    if meta is not None and self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    async with self.store.lock_for(session_id):
      session_dir = self.store.session_dir(session_id)
      if not session_dir.exists():
        return False
      await asyncio.to_thread(shutil.rmtree, session_dir)
      # The session's memory-cap cgroup: removed only when empty (no live
      # member left); a retained directory is logged debug and reclaimed by
      # the kernel once its last process exits.
      await asyncio.to_thread(cleanup_session_cgroup, session_id)
      self.events.drop_session_runtime_state(session_id)
      self.store.invalidate_cache(session_id)
      # The sidebar's whole-body memo keys on (requested ids, generation), so a
      # deletion must bump the generation or a poll still carrying the deleted
      # id serves the ghost row. The mark is never consumed — the fold probes
      # resolved sessions only — and that is fine; the bump is the point.
      sidebar_state.mark_sidebar_dirty(session_id)
      # Popping the lock from the dict while holding it is safe: the popped lock
      # object stays valid for this holder until the ``async with`` exits.
      self.store.metadata_locks.pop(session_id, None)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> SessionMetadata | None:
    """Restore an archived session back to active.

    The status flip moves the tree projection the same way the archive write
    does (descendants inherit the ancestor's restored visibility), so the
    rebuildable index drops through the same hook.
    """
    meta = await self.store.update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")
    if meta is not None and self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    return meta

  async def recycle_history_before(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Move old chat events out of the live log into weekly archive files."""
    archive_result = await asyncio.to_thread(
        self.events.chat_events.archive_old_chat_events_sync, session_id, cutoff_utc)
    events_archived = archive_result["events_archived"]
    archive_file = archive_result["archive_file"]
    if events_archived:
      async with self.store.lock_for(session_id):
        fresh = await self.store.get_session(session_id)
        if fresh is not None:
          fresh.archive_offset += events_archived
          await self.store.save_metadata(fresh, lock_held=True)
      self.events.drop_session_runtime_state(session_id)
    return {"events_archived": events_archived, "archive_file": archive_file}

  async def star_session(self, session_id: str) -> SessionMetadata | None:
    """Star a session."""
    return await self.store.update_field(session_id, "starred", value=True, log_event="session_starred")

  async def unstar_session(self, session_id: str) -> SessionMetadata | None:
    """Unstar a session."""
    return await self.store.update_field(session_id, "starred", value=False, log_event="session_unstarred")

  async def set_group(self, session_id: str, group: str | None) -> SessionMetadata | None:
    """Set or clear the group for a session."""
    meta = await self.store.update_field(session_id, "group", group, "session_group_set")
    if meta:
      await self.events.broadcast_sidebar(session_id, ET.SESSION_GROUP_CHANGED, group=group)
    return meta

  async def rename_group(self, old_name: str, new_name: str) -> int:
    """Rename a group across all sessions. Returns the count of updated sessions."""
    count = await self._rewrite_group(old_name, new_name)
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    count = await self._rewrite_group(group, None)
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def _rewrite_group(self, old_name: str, new_name: str | None) -> int:
    """Set old_name's group to new_name on every matching session. Returns the count updated."""
    # Membership reads only, so the shared cached metas serve directly (read-only);
    # the leaving-the-manager copy is paid per matching row by get_session below.
    all_sessions = await self.store.load_session_metas()
    count = 0
    for meta in all_sessions:
      if meta.group != old_name:
        continue
      async with self.store.lock_for(meta.id):
        fresh = await self.store.get_session(meta.id)
        if not fresh or fresh.group != old_name:
          continue
        fresh.group = new_name
        fresh.updated_at = utc_now()
        await self.store.save_metadata(fresh, lock_held=True)
      count += 1
    return count

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
        mark_unread=self.mark_unread,
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
    )
  return _session_manager
