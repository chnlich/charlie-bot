"""Thread management for CharlieBot Worker tasks."""

import asyncio
import os
from collections.abc import Iterator
from pathlib import Path

import aiofiles

from src.infra.config import CharlieBotConfig
from src.infra.json_utils import write_model_json_atomically
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import StatSignatureMemo
from src.infra.models import ThreadMetadata
from src.runtime.runs import DATA_DIR_NAME, EVENTS_LOG_NAME, METADATA_NAME, THREADS_DIR_NAME
from src.runtime.sidebar_state import mark_sidebar_dirty

log = LazyStructlogLogger()

# Backstop cap on ThreadManager's parse memo: each walk drops the entries for
# files it did not see, so the resident set tracks the walked thread files and
# the cap only bounds a burst of walks across many sessions.
_THREAD_LIST_MEMO_LIMIT = 1024


def thread_events_log_path(session_dir: Path, thread_id: str) -> Path:
  """Return the path to a thread's events.jsonl under its session directory."""
  return session_dir / THREADS_DIR_NAME / thread_id / DATA_DIR_NAME / EVENTS_LOG_NAME


def iter_thread_meta_stats(threads_dir: str | Path) -> Iterator[tuple[str, os.stat_result]]:
  """Yield ``(metadata.json path, stat)`` for every thread directory under *threads_dir*.

  Raises OSError when *threads_dir* itself cannot be scanned (missing or
  unreadable): that verdict belongs to the caller. A metadata.json absent or
  unreadable at stat time is skipped, the same "no thread row" verdict for
  every stat failure. Paths are scandir's plain strings — this scan runs per
  poll, and the pathlib join/str wrappers cost more CPU than the stat
  syscalls they drive.
  """
  with os.scandir(threads_dir) as entries:
    for entry in entries:
      if not entry.is_dir():
        continue
      meta_path = entry.path + "/" + METADATA_NAME
      try:
        yield meta_path, os.stat(meta_path)
      except OSError:
        continue


class ThreadManager:
  """Creates and manages Worker threads."""

  def __init__(self, cfg: CharlieBotConfig) -> None:
    self._cfg = cfg
    # StatSignatureMemo keyed by metadata.json path. Re-validating every thread
    # file on each list walk costs ~176 us per thread; the
    # stat-before-read contract keeps a rewrite from serving old data (a
    # rewrite always moves (mtime_ns, size)). Each walk drops the entries for
    # files it did not see, so a deleted thread never lingers, and concurrent
    # executor walks serialize on the memo's lock. Callers only read the
    # returned metas: the update path re-reads through the uncached
    # get_thread, so no in-place mutation of a memoized instance exists to
    # leak.
    self._list_memo: StatSignatureMemo[str, ThreadMetadata] = StatSignatureMemo(_THREAD_LIST_MEMO_LIMIT)

  async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata | None:
    path = self._metadata_path(session_id, thread_id)
    if not path.exists():
      return None
    async with aiofiles.open(path) as f:
      raw = await f.read()
    return ThreadMetadata.model_validate_json(raw)

  async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
    threads_dir = self._cfg.sessions_dir / session_id / THREADS_DIR_NAME

    def load_all() -> list[ThreadMetadata | None]:
      # One executor hop for the whole scan: a per-file aiofiles read costs
      # ~0.5 ms in thread-pool hand-off, so per-file reads make the list walk
      # scale linearly with thread count.
      if not threads_dir.is_dir():
        return []
      return self._metas_from_stats(iter_thread_meta_stats(threads_dir), str(threads_dir))

    threads = [t for t in await asyncio.to_thread(load_all) if t is not None]
    threads.sort(key=lambda t: t.created_at, reverse=True)
    return threads

  def list_threads_from_stats(self, pairs: Iterator[tuple[str, os.stat_result]],
                              threads_dir: str) -> list[ThreadMetadata | None]:
    """Parse-merge the pre-walked ``(metadata.json path, stat)`` pairs of one session.

    *pairs* must be a fresh walk of exactly the files the caller's freshness
    signature derives from (``iter_thread_meta_stats`` shape), so the returned
    metas and that signature describe the same instant. The returned list
    aligns position-for-position with *pairs*: a file the walk statted but
    that vanished before its read yields ``None`` at its position (the same
    no-thread-row verdict the walk's stat failure gives), so callers pairing
    metas back with pairs stay aligned. The parse memo is ``list_threads``'s:
    a hit costs no read, a miss reads the file, and this session's files
    absent from *pairs* drop out of the memo. *threads_dir* is the walked
    directory's path — the drop's scope, and the only witness for an empty
    walk (a vanished threads dir drops the session's every memoized file).
    """
    return self._metas_from_stats(pairs, threads_dir)

  def _metas_from_stats(self, pairs: Iterator[tuple[str, os.stat_result]],
                        threads_dir: str) -> list[ThreadMetadata | None]:
    walked: set[str] = set()
    metas: list[ThreadMetadata | None] = []
    for meta_path, st in pairs:
      walked.add(meta_path)
      meta = self._list_memo.fresh(meta_path, st)
      if meta is not None:
        metas.append(meta)
        continue
      try:
        with open(meta_path, encoding="utf-8") as f:
          meta = ThreadMetadata.model_validate_json(f.read())
      except OSError as e:
        # The walk statted this file, so a read failure here means the file
        # vanished (session GC) between the walk and this read; the walk's
        # stat failure gets the same no-thread-row verdict.
        log.debug("thread_metadata_read_failed", path=meta_path, error=str(e))
        meta = None
      else:
        self._list_memo.record(meta_path, st, meta)
      metas.append(meta)
    # The vanished-file drop covers this walk's own directory only: a metadata
    # path under another session's threads dir belongs to that session's walk,
    # whose next walk drops its own vanished files. A whole-memo drop here
    # evicts them, so every interleaved session's walk re-reads and re-parses
    # its full thread set (measured 3.2x on the six-session churn one sidebar
    # poll pays, the memo holding one session's files after any rebuild).
    prefix = threads_dir.rstrip("/") + "/"
    self._list_memo.drop_where(lambda key: key.startswith(prefix) and key not in walked)
    return metas

  def thread_dir(self, session_id: str, thread_id: str) -> Path:
    """A thread's canonical on-disk directory (metadata.json and data/)."""
    return self._cfg.sessions_dir / session_id / THREADS_DIR_NAME / thread_id

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return thread_events_log_path(self._cfg.sessions_dir / session_id, thread_id)

  async def save_metadata(self, meta: ThreadMetadata) -> None:
    """Persist thread metadata to disk."""
    await self._save_metadata(meta)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _save_metadata(self, meta: ThreadMetadata) -> None:
    # list_threads reads this file from an executor thread with no
    # coordination, so the write must stay atomic (a validation failure 500s
    # the whole list endpoint).
    path = self._metadata_path(meta.session_id, meta.id)
    await write_model_json_atomically(path, meta)
    # Single funnel behind save_metadata: thread status
    # transitions (running -> terminal) land here. The mark carries the
    # published path so the list poll's incremental proof stats exactly this
    # file; it must follow the rename above.
    mark_sidebar_dirty(meta.session_id, str(path))

  def _metadata_path(self, session_id: str, thread_id: str) -> Path:
    return self.thread_dir(session_id, thread_id) / METADATA_NAME
