"""Session fork: fork and elone, which copy one session's chat history into a new manager session.

``SessionFork`` creates the child session, writes its ``chat_events.jsonl`` as the parent's raw event lines followed
by the ``clone_start`` marker and the child's creation fact, and lets every sidebar contribution copy its own files.
The child of a manager source keeps the source's task-tree position: a clone lands as a sibling under the same
parent, and ``elone_session`` replaces the source in place — the whole subtree re-parents to the child, the source
closes with the archive format, and a scheduled task bound to the source rebinds to the child. The history copy
streams the parent's lines through an mmap and a chunked numpy scan, so a gigabyte-class corpus never enters the
Python heap. The task-tree owner registers ``tree_index_invalidator`` and itself. The process builds one block
(``fork()``); tests build their own and install it with ``set_fork()``.
"""

import asyncio
import json
import mmap
import os
import uuid
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import TYPE_CHECKING, BinaryIO, Protocol

from src.infra import event_types as ET

if TYPE_CHECKING:
  import numpy as np
from src.infra.config import CharlieBotConfig, get_config
from src.infra.json_utils import atomic_write_stream
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import EventRef, SessionMetadata, SessionStatus, utc_now, utc_now_iso
from src.runtime import session_events, session_store, sidebar_state
from src.runtime.chat_events import ARCHIVE_FILE_GLOB, chat_event_archives_dir
from src.runtime.control_events import ACTOR_USER, build_task_created_event
from src.runtime import task_errors
from src.runtime.hooks.sidebar_contributions import sidebar_contributions

log = LazyStructlogLogger()

# The fork/elone API routes (src/runtime/api/sessions.py) open their auto-injected
# bootstrap prompts with an opener plus the note, and a feature that summarizes
# sessions filters such injected messages by opener-prefix match; the clone/elone
# bootstraps and the context-reset note (``session_anchors.context_reset_note``) share the
# history note. Both sides import this one copy so an edit cannot drift them apart.
FORK_BOOTSTRAP_OPENER = "This session continues a prior conversation."
ELONE_BOOTSTRAP_OPENER = "You're taking over because the user wasn't satisfied with the previous session."
HISTORY_LOCATION_NOTE = "Earlier turns' history remains readable in this session's chat log, data/chat_events.jsonl in the working directory."

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


class TaskTreeOwner(Protocol):
  """The task-tree owner's surface this fork consumes; ``TaskTreeManager`` satisfies it.

  The owner registers itself at wiring time (``fork.tree = self``). The
  protocol keeps this module import-free of the task-tree layer above it: the
  owner judges the derived task state and writes the tree's facts under its
  own write lock, calling back into this fork for the history copy and the
  stored-status archive.
  """

  async def clone_blocker(self, source_id: str) -> str | None:
    """The clone refusal for one source, or None when the clone may write."""
    ...

  async def run_elone_replacement(
      self,
      source: SessionMetadata,
      event_index: int,
      backend: str | None,
      publish: Callable[[], Awaitable[SessionMetadata]],
      archive: Callable[[str], Awaitable[None]],
  ) -> SessionMetadata:
    """Replace one manager source with its elone child and return the child.

    The owner validates, redelivers and moves under its own write lock, calls
    *publish* once to create the child, and calls *archive(new_id)* to land
    the source's stored-status archive and successor pointer. A failed step
    raises with the steps that completed listed.
    """
    ...


class SessionFork:
  """Session fork and elone over the store and events blocks."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._events = events
    # The task-tree owner registers its projection invalidator here at wiring
    # time (TaskTreeManager.__init__). A new session moves a tree-projection
    # input, so the write must drop the tree's rebuildable index — the same
    # policy the task owner applies to its own metadata writes; None only
    # before that wiring exists (no tree consumer constructed in this process).
    self.tree_index_invalidator: Callable[[], None] | None = None
    # The tree owner registers itself here at the same wiring point (the
    # TaskTreeOwner protocol below); None only before that wiring exists.
    self.tree: TaskTreeOwner | None = None

  async def fork_session(
      self,
      parent_id: str,
      event_index: int | None = None,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create a new session whose chat log opens with the parent's raw event lines.

    A manager source's clone is a sibling at the source's position: it carries the
    source's decomposition edge, task record, group and prompt-rule references, and
    the source with its subtree stays untouched. The source's parent task must be
    open — a closed parent refuses the clone before anything is written.
    """
    if self.tree is not None:
      blocker = await self.tree.clone_blocker(parent_id)
      if blocker is not None:
        raise task_errors.TaskConflictError([blocker])
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
    land at the most recent takeover.

    An elone of a manager source keeps the node's task-tree position: under the
    tree's write lock the child takes over the source's decomposition edge and
    its whole subtree, and the source closes with the archive format
    (``task_closed`` outcome ``archived``, ``report_to`` null). A scheduled task
    bound to the source moves to the child: the binding's yaml ``session_id``
    rebinds through the registered sequence controllers' move duty (the
    ``write_cron_key`` single-key write), and the scheduler bookkeeping
    (``last_scheduled_run``, ``last_scheduled_cron``, ``last_run_status``)
    copies over, so the next fire runs on the child. An elone of a worker
    source is an ordinary fork: the child is a manager root and the source
    keeps only the archived status and the successor pointer.
    """
    fresh_parent = await self._store.read_metadata_fresh(parent_id)
    if fresh_parent is None:
      raise FileNotFoundError(f"parent session not found: {parent_id}")
    if fresh_parent.profile != "manager":
      return await self._elone_ordinary_session(parent_id, event_index, backend)
    tree = self.tree
    if tree is None:
      raise RuntimeError("no task-tree owner is wired over this fork; build the TaskTreeManager first")
    meta = await tree.run_elone_replacement(
        fresh_parent,
        event_index,
        backend,
        publish=lambda: self._spawn_with_history(parent_id, event_index, backend, "E"),
        archive=lambda new_id: self._archive_source(parent_id, new_id))
    self._log_spawn("session_eloned", meta, parent_id, event_index)
    return meta

  async def _archive_source(self, source_id: str, new_id: str) -> None:
    """The stored-status archive and the elone successor pointer (re-read under
    the store lock so concurrent mutations to the source aren't clobbered).
    Latest-wins: a source's pointer is overwritten to name each new child, so
    the pointer always names the most recent takeover."""
    async with self._store.lock_for(source_id):
      fresh_source = await self._store.get_session(source_id)
      if fresh_source:
        fresh_source.status = SessionStatus.ARCHIVED
        fresh_source.successor_session_id = new_id
        fresh_source.updated_at = utc_now()
        await self._store.save_metadata(fresh_source, lock_held=True)
    self._events.drop_session_runtime_state(source_id)

  async def _elone_ordinary_session(
      self,
      parent_id: str,
      event_index: int,
      backend: str | None,
  ) -> SessionMetadata:
    """The worker-source elone: a manager-root child, archived source, successor pointer."""
    meta = await self._spawn_with_history(parent_id, event_index, backend, "E")
    await self._archive_source(parent_id, meta.id)
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
    log the parent grepped. A manager parent's child holds the parent's
    task-tree position (its ``task_parent_id``, task record, group and
    prompt-rule references); a worker parent's child stays a manager root with
    no position, the shape this spawn always produced.
    """
    parent = await self._store.get_session(parent_id)
    if not parent:
      raise FileNotFoundError(f"parent session not found: {parent_id}")

    count = await asyncio.to_thread(self._events.get_chat_event_count_sync, parent_id, parent)
    if event_index is None:
      end = count
    else:
      if event_index < 0 or event_index >= count:
        raise ValueError(f"event_index {event_index} out of range for parent session {parent_id} with {count} events")
      end = event_index + 1

    # A manager source's child keeps the source's tree position: elone replaces
    # the node in place and clone adds a sibling under the same parent, so the
    # copy carries the decomposition edge, the task record and the prompt-rule
    # references. The task spec is copied deep so the two nodes' specs never
    # alias one object.
    holds_position = parent.profile == "manager"
    meta = SessionMetadata(
        name=f"{name_prefix}{parent.name}",
        schema_version=2,
        profile="manager",
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
        task_parent_id=parent.task_parent_id if holds_position else None,
        task=parent.task.model_copy(deep=True) if holds_position and parent.task is not None else None,
        subtree_prompt_ref=parent.subtree_prompt_ref if holds_position else None,
        node_prompt_ref=parent.node_prompt_ref if holds_position else None,
    )
    created_event = build_task_created_event(
        actor=ACTOR_USER,
        task_id=meta.id,
        request_id=f"history-copy-{meta.id}",
        task_parent_id=meta.task_parent_id,
        task_spec_hash=None,
    )
    meta.created_by_event = EventRef(session_id=meta.id, event_id=str(created_event["id"]))
    session_dir = self._store.session_dir(meta.id)
    self._create_session_dirs(session_dir)

    events_path = self._events.get_chat_events_path(meta.id)
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
    await self._store.save_metadata(meta)
    if self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    await asyncio.to_thread(self._copy_on_fork_sync, self._store.session_dir(parent_id), session_dir)
    # A contribution's copy lands after save_metadata, and a poll racing between the two
    # could have snapshotted the child without the copied files; re-mark so they are always probed.
    sidebar_state.mark_sidebar_dirty(meta.id)
    await self._events.broadcast_task_tree_changed(meta.id, ET.TASK_CREATED)
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
    archive_take = min(self._events.chat_events.read_archive_offset_sync(parent_id), end)
    live_take = end - archive_take
    parent_dir = self._store.session_dir(parent_id)
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

      live_path = self._events.get_chat_events_path(parent_id)
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


# The process owner of the fork block; built on the first ``fork()`` call.
_fork: SessionFork | None = None


def fork() -> SessionFork:
  """The process-wide session fork block."""
  global _fork
  if _fork is None:
    _fork = SessionFork(get_config(), session_store.store(), session_events.events())
  return _fork


def set_fork(replacement: SessionFork | None) -> None:
  """Replace the process fork singleton (tests); None restores lazy construction."""
  global _fork
  _fork = replacement
