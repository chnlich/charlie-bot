"""Chat event persistence for CharlieBot sessions."""

import json
import uuid
from collections.abc import Callable
from datetime import datetime
from pathlib import Path

import structlog

from src.core.finalize_effects import _MASTER_OUTPUT_TYPES, _is_terminal_worker_summary
from src.core.json_utils import atomic_write_text
from src.core.memo import BoundedMemo, StatSignatureMemo
from src.core.models import SessionMetadata, parse_utc_datetime, utc_now
from src.core.ndjson import (
    append_ndjson,
    count_ndjson_lines,
    parse_ndjson_file,
    parse_ndjson_range,
    parse_ndjson_tail,
)

log = structlog.get_logger()

# Bound on _archive_events_memo in files, not sessions: one scroll spans a
# session's few weekly archive files, so the cap bounds parsed-archive memory
# across every session that paginates.
_ARCHIVE_MEMO_LIMIT = 16
# Bound on _live_range_memo in files, not bytes: one scroll touches one live
# file, so the cap bounds parsed-corpus memory across sessions paginating
# concurrently. An entry holds one slot per physical line because
# parse_ndjson_range numbers ranges over physical lines (blank and malformed
# lines consume an index).
_LIVE_RANGE_MEMO_LIMIT = 4


def chat_events_path(session_dir: Path) -> Path:
  """Return the path to a session's chat_events.jsonl under its session directory."""
  return session_dir / "data" / "chat_events.jsonl"


def _universal_newline_segments(buf: bytes) -> tuple[list[str], bool]:
  """Split *buf* into physical lines the way text-mode iteration would, and say whether the
  content ends on a line boundary.

  The PEP 278 translation (``\\r\\n`` and lone ``\\r`` to ``\\n``) applied manually keeps the
  byte-offset extension and the text-mode line semantics from ever drifting apart: a byte
  offset measured on raw bytes must land on the same physical-line boundary a text-mode
  reader would have stopped at.
  """
  if not buf:
    return [], True
  text = buf.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
  ends_with_newline = text.endswith("\n")
  segments = text.split("\n")
  if ends_with_newline:
    segments.pop()
  return segments, ends_with_newline


def _live_range_event(segment: str, session_id: str) -> dict | None:
  """Parse one physical line; a blank or malformed line parses to None and consumes its index."""
  stripped = segment.strip()
  if not stripped:
    return None
  try:
    return json.loads(stripped)
  except json.JSONDecodeError as e:
    log.debug("live_range_parse_skip", session_id=session_id, error=str(e))
    return None


class _FinalizeFold:
  """Per-session derived finalize-judgment state over the cached live events.

  The two finalize idempotency judgments (``src/core/finalize_effects``) are
  pure scans over the whole event list; the fold answers both in O(1):

  - ``summary_marks`` maps each thread_id to the master-output count at the
    moment its last terminal worker_summary appended;
  - ``master_outputs`` counts every master-output event appended since.

  A thread's summary is present iff it holds a mark; the master woke after
  that summary iff the count has moved past the mark. The rules come from
  finalize_effects' own predicates, so the fold and the pure scans cannot
  drift; the parity test pins equivalence over randomized appends.
  """

  __slots__ = ("summary_marks", "master_outputs")

  def __init__(self) -> None:
    self.summary_marks: dict[str, int] = {}
    self.master_outputs = 0

  @classmethod
  def build(cls, events: list[dict]) -> "_FinalizeFold":
    """Derive the fold from a freshly parsed event list (one pass, in the loading thread)."""
    fold = cls()
    for event in events:
      fold.absorb(event)
    return fold

  def absorb(self, event: dict) -> None:
    """Advance the fold by one appended event, the same mutation the list took."""
    if event.get("type") in _MASTER_OUTPUT_TYPES:
      self.master_outputs += 1
      return
    thread_id = event.get("thread_id")
    if isinstance(thread_id, str) and _is_terminal_worker_summary(event, thread_id):
      self.summary_marks[thread_id] = self.master_outputs

  def summary_present(self, thread_id: str) -> bool:
    return thread_id in self.summary_marks

  def master_woke(self, thread_id: str) -> bool:
    mark = self.summary_marks.get(thread_id)
    return mark is not None and self.master_outputs > mark


class ChatEventStore:
  """Persistence and cache operations for per-session chat_events.jsonl."""

  def __init__(
      self,
      session_dir_fn: Callable[[str], Path],
      metadata_path_fn: Callable[[str], Path],
      metadata_cache: dict[str, tuple[SessionMetadata, float, tuple[int, int] | None]],
  ) -> None:
    self._session_dir = session_dir_fn
    self._metadata_path = metadata_path_fn
    self._metadata_cache = metadata_cache
    # In-memory cache: session_id -> list[dict] of parsed NDJSON events.
    # Populated on first read, kept in sync by save_chat_event().
    self._events_cache: dict[str, list[dict]] = {}
    # Finalize-judgment fold per cached session (glossary on _FinalizeFold):
    # built at load, advanced O(1) per append, dropped with the cache entry.
    self._finalize_folds: dict[str, _FinalizeFold] = {}
    # Parsed-archive memo: path -> (mtime_ns, size, events). Archive files are
    # append-only within their week and frozen after, so an unchanged
    # (mtime_ns, size) means unchanged bytes; an append re-parses one file.
    self._archive_events_memo: StatSignatureMemo[Path, list[dict]] = StatSignatureMemo(_ARCHIVE_MEMO_LIMIT)
    # Archive file-list memo: archives dir -> (mtime_ns, size, sorted paths).
    # Membership changes only by creating or removing a directory entry, and
    # either moves the directory's own mtime_ns, so an unchanged signature
    # proves the name list current; a same-week append moves only the file's
    # own signature.
    self._archive_files_memo: StatSignatureMemo[Path, list[Path]] = StatSignatureMemo(_ARCHIVE_MEMO_LIMIT)
    # Live-file range memo: path -> (mtime_ns, size, inode, per-physical-line
    # events with None holes for blank/malformed lines, covered byte size,
    # ends on a line boundary). Gated to archive_offset > 0 sessions:
    # unarchived sessions paginate through the message projection, so their
    # range callers (recap extract, bulk reads) would pay a whole-file parse to
    # retain a list a moving divider never reuses.
    self._live_range_memo: BoundedMemo[Path, tuple[int, int, int, list[dict | None], int,
                                                   bool]] = BoundedMemo(_LIVE_RANGE_MEMO_LIMIT)

  @property
  def events_cache(self) -> dict[str, list[dict]]:
    return self._events_cache

  def cached_event_count(self, session_id: str) -> int:
    return len(self._events_cache[session_id])

  def clear_cache(self, session_id: str) -> None:
    self._events_cache.pop(session_id, None)
    self._finalize_folds.pop(session_id, None)

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl."""
    return self._chat_events_path(session_id)

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    if 'id' not in event:
      event['id'] = str(uuid.uuid4())
    if 'timestamp' not in event:
      event['timestamp'] = utc_now().isoformat()
    await append_ndjson(self._chat_events_path(session_id), event)
    # Keep in-memory cache in sync
    if session_id in self._events_cache:
      self._events_cache[session_id].append(event)
      self._finalize_folds[session_id].absorb(event)

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    if session_id in self._events_cache:
      return self._events_cache[session_id]
    events = parse_ndjson_file(self._chat_events_path(session_id))
    self._events_cache[session_id] = events
    self._finalize_folds[session_id] = _FinalizeFold.build(events)
    return events

  def peek_cached_events(self, session_id: str) -> list[dict] | None:
    """Return the cached events list without reading disk; None when the cache is cold.

    The projection hit check calls this instead of load_chat_events_sync so a
    cold cache stays cold: a miss here sends the caller to the threaded read
    path rather than parsing the whole file on the event loop.
    """
    return self._events_cache.get(session_id)

  def finalize_summary_present(self, session_id: str, thread_id: str) -> bool:
    """O(1) fold read of the summary-present judgment (glossary on _FinalizeFold).

    The fold exists exactly while the events cache entry does (load builds it,
    append advances it, clear drops it), so the caller peeks the cache warm
    first; a cold read here raises loudly instead of parsing on the loop.
    """
    return self._finalize_folds[session_id].summary_present(thread_id)

  def finalize_master_woke(self, session_id: str, thread_id: str) -> bool:
    """O(1) fold read of the master-woke judgment (glossary on _FinalizeFold)."""
    return self._finalize_folds[session_id].master_woke(thread_id)

  def load_chat_events_tail(self, session_id: str, limit: int = 200) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk. Does NOT populate _events_cache.

    Returns (events, total_line_count, has_more).
    """
    events, total, has_more = parse_ndjson_tail(self._chat_events_path(session_id), limit)
    return events, total, has_more

  def get_chat_event_count_sync(self, session_id: str, session_meta: SessionMetadata | None = None) -> int:
    """Return the current global chat event count without parsing event payloads."""
    if session_meta is not None:
      archive_offset = session_meta.archive_offset
    else:
      archive_offset = self.read_archive_offset_sync(session_id)
    if session_id in self._events_cache:
      return archive_offset + len(self._events_cache[session_id])
    return archive_offset + count_ndjson_lines(self._chat_events_path(session_id))

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end). Returns (events, has_more).

    Indices are global (archive_offset + line_in_live_file). When the requested
    range starts before the live file, archived chat_events files under
    ``data/archives/`` are read in chronological order to fill the gap. The
    archived halves serve from their (mtime_ns, size) memos; an unarchived
    session's warm events cache serves its half as a slice.
    """
    if end <= start:
      return [], start > 0
    archive_offset = self.read_archive_offset_sync(session_id)
    live_path = self._chat_events_path(session_id)
    if end <= archive_offset:
      return self._load_archive_range(session_id, start, end), start > 0
    if start >= archive_offset:
      rel_start = start - archive_offset
      rel_end = end - archive_offset
      if archive_offset > 0:
        lines = self._live_range_lines(live_path, session_id)
        return [e for e in lines[rel_start:rel_end] if e is not None], start > 0
      cached = self._events_cache.get(session_id)
      if cached is not None:
        # Unarchived: the global index is the cache's own parsed-event index.
        # save_chat_event is the single append funnel and every whole-file
        # rewrite (archive rotation, fork, delete) drops the cache in the same
        # flow, so a warm cache is the file's parsed truth — the same trust
        # load_chat_events_sync's consumers (projection, usage, finalize
        # folds) already place in it. parse_ndjson_range's islice counts
        # physical lines instead, so the disk read both re-parses the whole
        # prefix per call (the recap's per-divider cost) and skews its window
        # by any malformed lines the cached count never charged.
        return cached[rel_start:rel_end], start > 0
      events, _ = parse_ndjson_range(live_path, rel_start, rel_end)
      return events, start > 0
    archive_events = self._load_archive_range(session_id, start, archive_offset)
    live_end = end - archive_offset
    lines = self._live_range_lines(live_path, session_id)
    live_events = [e for e in lines[:live_end] if e is not None]
    return archive_events + live_events, start > 0

  def read_archive_offset_sync(self, session_id: str) -> int:
    """Synchronously read the archive_offset from metadata.json.

    Used by sync read paths (``load_chat_events_range`` and SessionManager's
    aggregator seed / projection guard) so they don't have to go async just to
    learn the live/archive split. Falls back to 0 if the metadata file is
    missing or unreadable.
    """
    cached = self._metadata_cache.get(session_id)
    if cached is not None:
      return cached[0].archive_offset
    path = self._metadata_path(session_id)
    if not path.exists():
      return 0
    try:
      raw = path.read_text(encoding="utf-8")
      if not raw.strip():
        return 0
      return SessionMetadata.model_validate_json(raw).archive_offset
    except (OSError, ValueError) as e:
      log.debug("archive_offset_read_failed", session_id=session_id, error=str(e))
      return 0

  def _load_archive_range(self, session_id: str, start: int, end: int) -> list[dict]:
    """Read events at global indices [start, end) from archive files.

    Archives live in ``<session>/data/archives/chat_events.<YYYY>-W<WW>.jsonl``.
    Files are walked in chronological order (filename sort happens to match).
    Only files overlapping the range are concatenated: each file's parsed list
    is memoized, so its length is known without a copy and a page turn pays one
    sub-slice extend instead of extending every file's whole list.
    """
    if end <= start:
      return []
    events: list[dict] = []
    pos = 0
    for path in self._archive_files(session_id):
      file_events = self._archive_file_events(path, session_id)
      file_end = pos + len(file_events)
      if pos < end and file_end > start:
        events.extend(file_events[max(start, pos) - pos:min(end, file_end) - pos])
      pos = file_end
      if pos >= end:
        break
    return events

  def _archive_files(self, session_id: str) -> list[Path]:
    """Return the session's archive files in chronological order, memoized on the dir's stat.

    The signature is taken before the glob, so a rotation racing this read keys
    its entry to the older directory state and the next call re-scans.
    """
    archives_dir = self._session_dir(session_id) / "data" / "archives"
    try:
      st = archives_dir.stat()
    except OSError as e:
      log.debug("archive_dir_stat_failed", path=str(archives_dir), error=str(e))
      return []
    paths = self._archive_files_memo.fresh(archives_dir, st)
    if paths is None:
      paths = sorted(archives_dir.glob("chat_events.*.jsonl"))
      self._archive_files_memo.record(archives_dir, st, paths)
    return paths

  def _archive_file_events(self, path: Path, session_id: str) -> list[dict]:
    """Return one archive file's parsed events, memoized on (mtime_ns, size).

    Range reads re-enter here on every page turn; the memo keeps unchanged
    archives at zero disk reads. An unreadable file logs and contributes
    nothing, the pre-memo reader's behavior on open failure.
    """
    try:
      st = path.stat()
    except OSError as e:
      log.debug("archive_read_failed", path=str(path), error=str(e))
      return []
    events = self._archive_events_memo.fresh(path, st)
    if events is not None:
      return events
    events: list[dict] = []
    try:
      with open(path, encoding="utf-8") as f:
        for line in f:
          stripped = line.strip()
          if stripped:
            try:
              events.append(json.loads(stripped))
            except json.JSONDecodeError as e:
              log.debug("archive_parse_skip", session_id=session_id, error=str(e))
    except OSError as e:
      log.debug("archive_read_failed", path=str(path), error=str(e))
      return []
    self._archive_events_memo.record(path, st, events)
    return events

  def _live_range_lines(self, path: Path, session_id: str) -> list[dict | None]:
    """Return the live file's per-physical-line parsed events, memoized on (mtime_ns, size).

    A scroll through an archived session's live half re-enters here on every
    page turn; the memo keeps an unchanged file at one stat per turn. An
    appended line re-parses only the appended tail: chat files mutate only by
    append between archive rewrites, and a rewrite publishes through
    ``os.replace`` and so swaps the inode, so a same-inode size growth extends
    the previous parse from the byte offset its content actually covers. An
    entry whose read raced a landing append keys its pre-read stat, so it is
    reachable only through that covered offset, never as a hit for newer bytes.
    A covered content ending mid-line blocks extension until a full re-parse
    lands on a line boundary, so a completed append is never glued onto a
    half-parsed last line. An unreadable file logs and contributes nothing, the
    pre-memo reader's behavior on open failure.
    """
    try:
      st = path.stat()
    except OSError as e:
      log.debug("live_range_read_failed", path=str(path), error=str(e))
      return []
    memo = self._live_range_memo.get(path)
    if memo is not None and memo[0] == st.st_mtime_ns and memo[1] == st.st_size:
      return memo[3]
    extend_from = (memo[3], memo[4]) if (
        memo is not None and memo[2] == st.st_ino and st.st_size >= memo[4] and memo[5]) else None
    if extend_from is not None:
      base_lines, covered = extend_from
      buf = None
      try:
        with open(path, "rb") as f:
          f.seek(covered)
          buf = f.read()
      except OSError as e:
        log.debug("live_range_read_failed", path=str(path), error=str(e))
      if buf is not None:
        segments, ends = _universal_newline_segments(buf)
        lines = base_lines + [_live_range_event(segment, session_id) for segment in segments]
        self._live_range_memo.store(path, (st.st_mtime_ns, st.st_size, st.st_ino, lines, covered + len(buf), ends))
        return lines
    try:
      with open(path, "rb") as f:
        buf = f.read()
    except OSError as e:
      log.debug("live_range_read_failed", path=str(path), error=str(e))
      return []
    segments, ends = _universal_newline_segments(buf)
    lines = [_live_range_event(segment, session_id) for segment in segments]
    self._live_range_memo.store(path, (st.st_mtime_ns, st.st_size, st.st_ino, lines, len(buf), ends))
    return lines

  def _chat_events_path(self, session_id: str) -> Path:
    return chat_events_path(self._session_dir(session_id))

  def archive_old_chat_events_sync(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Split live chat_events.jsonl at cutoff_utc, append the head to a weekly archive."""
    live_path = self._chat_events_path(session_id)
    if not live_path.exists():
      return {"events_archived": 0, "archive_file": None}

    archived_raw: list[str] = []
    kept_raw: list[str] = []
    split_reached = False
    with open(live_path, encoding="utf-8") as f:
      for line in f:
        raw = line.rstrip("\n")
        if split_reached:
          kept_raw.append(raw)
          continue
        stripped = raw.strip()
        if not stripped:
          continue
        try:
          event = json.loads(stripped)
        except json.JSONDecodeError as e:
          log.debug("chat_event_archive_parse_skip", session_id=session_id, error=str(e))
          split_reached = True
          kept_raw.append(raw)
          continue
        ts_raw = event.get("timestamp")
        if not ts_raw:
          split_reached = True
          kept_raw.append(raw)
          continue
        try:
          ts = parse_utc_datetime(ts_raw)
        except ValueError as e:
          log.debug("chat_event_archive_ts_parse_skip", session_id=session_id, error=str(e))
          split_reached = True
          kept_raw.append(raw)
          continue
        if ts < cutoff_utc:
          archived_raw.append(raw)
        else:
          split_reached = True
          kept_raw.append(raw)

    if not archived_raw:
      log.info("chat_events_archive_noop", session_id=session_id)
      return {"events_archived": 0, "archive_file": None}

    iso = cutoff_utc.isocalendar()
    archives_dir = self._session_dir(session_id) / "data" / "archives"
    archives_dir.mkdir(parents=True, exist_ok=True)
    archive_path = archives_dir / f"chat_events.{iso.year}-W{iso.week:02d}.jsonl"
    with open(archive_path, "a", encoding="utf-8") as f:
      for raw in archived_raw:
        f.write(raw + "\n")

    atomic_write_text(live_path, "".join(raw + "\n" for raw in kept_raw))

    return {
        "events_archived": len(archived_raw),
        "archive_file": str(archive_path),
    }
