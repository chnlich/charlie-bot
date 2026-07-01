"""Chat event persistence for CharlieBot sessions."""

import json
import os
import uuid
from datetime import datetime
from pathlib import Path
from typing import Callable

import structlog

from src.core.models import SessionMetadata, parse_utc_datetime, utc_now
from src.core.ndjson import count_ndjson_lines, parse_ndjson_file, parse_ndjson_range, parse_ndjson_tail, append_ndjson

log = structlog.get_logger()


class ChatEventStore:
  """Persistence and cache operations for per-session chat_events.jsonl."""

  def __init__(
      self,
      session_dir_fn: Callable[[str], Path],
      metadata_path_fn: Callable[[str], Path],
      metadata_cache: dict[str, tuple[SessionMetadata, float]],
  ):
    self._session_dir = session_dir_fn
    self._metadata_path = metadata_path_fn
    self._metadata_cache = metadata_cache
    # In-memory cache: session_id -> list[dict] of parsed NDJSON events.
    # Populated on first read, kept in sync by save_chat_event().
    self._events_cache: dict[str, list[dict]] = {}

  @property
  def events_cache(self) -> dict[str, list[dict]]:
    return self._events_cache

  def has_cache(self, session_id: str) -> bool:
    return session_id in self._events_cache

  def cached_event_count(self, session_id: str) -> int:
    return len(self._events_cache[session_id])

  def clear_cache(self, session_id: str) -> None:
    self._events_cache.pop(session_id, None)

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

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    if session_id in self._events_cache:
      return self._events_cache[session_id]
    events = parse_ndjson_file(self._chat_events_path(session_id))
    self._events_cache[session_id] = events
    return events

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
      archive_offset = self._read_archive_offset_sync(session_id)
    if session_id in self._events_cache:
      return archive_offset + len(self._events_cache[session_id])
    return archive_offset + count_ndjson_lines(self._chat_events_path(session_id))

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end). Returns (events, has_more).

    Indices are global (archive_offset + line_in_live_file). When the requested
    range starts before the live file, archived chat_events files under
    ``data/archives/`` are read in chronological order to fill the gap.
    """
    if end <= start:
      return [], start > 0
    archive_offset = self._read_archive_offset_sync(session_id)
    live_path = self._chat_events_path(session_id)
    if end <= archive_offset:
      return self._load_archive_range(session_id, start, end), start > 0
    if start >= archive_offset:
      rel_start = start - archive_offset
      rel_end = end - archive_offset
      events, _ = parse_ndjson_range(live_path, rel_start, rel_end)
      return events, start > 0
    archive_events = self._load_archive_range(session_id, start, archive_offset)
    live_end = end - archive_offset
    live_events, _ = parse_ndjson_range(live_path, 0, live_end)
    return archive_events + live_events, start > 0

  def _read_archive_offset_sync(self, session_id: str) -> int:
    """Synchronously read the archive_offset from metadata.json.

    Used by sync read paths (e.g. ``load_chat_events_range``) so they don't
    have to go async just to learn the live/archive split. Falls back to 0 if
    the metadata file is missing or unreadable.
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
    """
    if end <= start:
      return []
    archives_dir = self._session_dir(session_id) / "data" / "archives"
    if not archives_dir.exists():
      return []
    archive_files = sorted(archives_dir.glob("chat_events.*.jsonl"))
    events: list[dict] = []
    cursor = 0
    for path in archive_files:
      if cursor >= end:
        break
      try:
        with open(path, "r", encoding="utf-8") as f:
          for line in f:
            if cursor >= end:
              break
            if cursor >= start:
              stripped = line.strip()
              if stripped:
                try:
                  events.append(json.loads(stripped))
                except json.JSONDecodeError as e:
                  log.debug("archive_parse_skip", session_id=session_id, error=str(e))
            cursor += 1
      except OSError as e:
        log.debug("archive_read_failed", path=str(path), error=str(e))
    return events

  def _chat_events_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "chat_events.jsonl"

  def _archive_old_chat_events_sync(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Split live chat_events.jsonl at cutoff_utc, append the head to a weekly archive."""
    live_path = self._chat_events_path(session_id)
    if not live_path.exists():
      return {"events_archived": 0, "archive_file": None}

    archived_raw: list[str] = []
    kept_raw: list[str] = []
    split_reached = False
    with open(live_path, "r", encoding="utf-8") as f:
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

    tmp_path = live_path.with_suffix(live_path.suffix + ".tmp")
    with open(tmp_path, "w", encoding="utf-8") as f:
      for raw in kept_raw:
        f.write(raw + "\n")
    os.replace(tmp_path, live_path)

    return {
        "events_archived": len(archived_raw),
        "archive_file": str(archive_path),
    }
