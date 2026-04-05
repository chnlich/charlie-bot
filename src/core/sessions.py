"""Session management for CharlieBot."""

import asyncio
import json
import shutil
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import aiofiles
import structlog

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    SessionMetadata,
    SessionStatus,
)
from src.api.message_utils import extract_text_from_message
from src.core.ndjson import append_ndjson, parse_ndjson_file, parse_ndjson_range, parse_ndjson_tail
from src.core.streaming import streaming_manager

log = structlog.get_logger()

_METADATA_CACHE_TTL = 5.0  # seconds
_CODEX_SESSIONS_DIR = Path.home() / ".codex" / "sessions"
_UNSET = object()
_TRANSIENT_METADATA_FIELDS = {
    "has_running_tasks",
    "has_pending_trigger",
    "pending_trigger_count",
    "next_trigger_at",
    "schedule_cron",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_timezone",
    "schedule_project",
    "schedule_allow_failure",
}


def _summarize_event_lines(lines: list[str]) -> str:
  """Parse JSONL event lines and build a user/assistant text summary, truncated to 4000 chars."""
  summary_parts = []
  for line_text in lines:
    try:
      ev = json.loads(line_text)
    except (json.JSONDecodeError, ValueError) as e:
      log.debug('summary_parse_skip', line=line_text[:120], error=str(e))
      continue
    t = ev.get('type')
    if t == 'user' and 'content' in ev:
      summary_parts.append(f'User: {ev["content"]}')
    elif t == 'assistant':
      text = extract_text_from_message(ev.get('message'))
      if text:
        summary_parts.append(f'Assistant: {text}')
  summary = '\n\n'.join(summary_parts)
  if len(summary) > 4000:
    summary = summary[:4000] + '\n\n[... truncated]'
  return summary


def _extract_codex_rollout_usage_event(event: dict[str, Any]) -> dict[str, int] | None:
  """Return context usage from a native Codex token_count event."""
  if event.get("type") != "event_msg":
    return None
  payload = event.get("payload") or {}
  if payload.get("type") != "token_count":
    return None
  info = payload.get("info") or {}
  last_usage = info.get("last_token_usage") or {}
  input_tokens = last_usage.get("input_tokens")
  context_limit = info.get("model_context_window")
  if input_tokens is None or context_limit is None:
    return None
  return {
      # Codex reports the active prompt window in last_token_usage.input_tokens.
      # total_token_usage is cumulative for the whole session and cached_input_tokens
      # is an informational subset, not an additive context-window component.
      "context_tokens": input_tokens,
      "context_limit": context_limit,
  }


def _extract_latest_codex_rollout_usage(path: Path) -> dict[str, int] | None:
  """Scan a native Codex rollout log backwards for the latest usable token_count event."""
  if not path.exists():
    return None

  chunk_size = 8192
  with open(path, "rb") as f:
    f.seek(0, 2)
    pos = f.tell()
    carry = b""

    while pos > 0:
      read_size = min(chunk_size, pos)
      pos -= read_size
      f.seek(pos)
      chunk = f.read(read_size)
      parts = (chunk + carry).split(b"\n")
      carry = parts[0] if pos > 0 else b""
      lines = parts[1:] if pos > 0 else parts

      for raw_line in reversed(lines):
        line = raw_line.strip()
        if not line:
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError as e:
          log.debug("codex_rollout_parse_skip", path=str(path), error=str(e))
          continue
        usage = _extract_codex_rollout_usage_event(event)
        if usage is not None:
          return usage

    if carry.strip():
      try:
        event = json.loads(carry)
      except json.JSONDecodeError as e:
        log.debug("codex_rollout_parse_skip", path=str(path), error=str(e))
      else:
        return _extract_codex_rollout_usage_event(event)

  return None


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    # In-memory cache: session_id -> list[dict] of parsed NDJSON events.
    # Populated on first read, kept in sync by save_chat_event().
    self._events_cache: dict[str, list[dict]] = {}
    # In-memory usage cache: session_id -> usage dict (context_tokens, context_limit, total_cost_usd, model).
    # Incrementally updated on each 'result' event via save_chat_event(), avoiding O(n) rescans.
    self._usage_cache: dict[str, dict] = {}
    # In-memory metadata cache: session_id -> (metadata, monotonic_timestamp).
    # TTL-based to avoid repeated disk reads within the same poll cycle.
    self._metadata_cache: dict[str, tuple[SessionMetadata, float]] = {}
    # Native Codex rollout helpers keyed by native thread id.
    self._codex_rollout_path_cache: dict[str, Path] = {}
    self._codex_rollout_usage_cache: dict[str, tuple[int, int, dict | None]] = {}

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest, backend: str | None = None) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    meta = SessionMetadata(name=name, scheduled_task=req.scheduled_task, backend=backend or "claude-opus-4.6")

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    await self._save_metadata(meta)

    log.info("session_created", session_id=meta.id, name=meta.name)
    return meta

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Load session metadata, using in-memory cache when available."""
    cached = self._metadata_cache.get(session_id)
    if cached is not None:
      meta, ts = cached
      if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
        return meta.model_copy()
      del self._metadata_cache[session_id]
    path = self._metadata_path(session_id)
    if not path.exists():
      return None
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    if not raw.strip():
      log.warning("session_metadata_empty", session_id=session_id, path=str(path))
      return None
    meta = SessionMetadata.model_validate_json(raw)
    self._metadata_cache[session_id] = (meta, time.monotonic())
    return meta.model_copy()

  async def list_sessions(
      self,
      status: Optional[SessionStatus] = None,
      starred: Optional[bool] = None,
      scheduled: Optional[bool] = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled."""
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs))
    sessions: list[SessionMetadata] = []
    for meta in all_meta:
      if not meta:
        continue
      if status is not None and meta.status != status:
        continue
      if starred is not None and meta.starred != starred:
        continue
      if scheduled is not None and bool(meta.scheduled_task) != scheduled:
        continue
      sessions.append(meta)
    return await self._enrich_and_sort(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )

  async def search_sessions(
      self,
      query: str,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Search active sessions by name and chat event content (case-insensitive)."""
    query_lower = query.lower()
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs))
    results: list[SessionMetadata] = []
    for meta in all_meta:
      if not meta or meta.status not in (SessionStatus.ACTIVE, SessionStatus.WAITING):
        continue
      # Check session name first
      if query_lower in (meta.name or '').lower():
        results.append(meta)
        continue
      # Check chat events (offload sync I/O to thread pool)
      events_path = self._chat_events_path(meta.id)
      if events_path.exists():
        try:
          text = await asyncio.to_thread(events_path.read_text, encoding='utf-8')
          if query_lower in text.lower():
            results.append(meta)
        except OSError as e:
          log.debug('search_read_failed', session_id=meta.id, error=str(e))
    return await self._enrich_and_sort(
        results,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )

  async def fork_session(
      self,
      parent_id: str,
      event_index: int | None = None,
      backend: str | None = None,
  ) -> Optional[SessionMetadata]:
    """Create a new session by cloning an existing one.

    If event_index is set, only events [0..event_index] (inclusive) are copied.
    Otherwise all events are copied (full clone).
    """
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    parent_events_path = self._chat_events_path(parent_id)
    if not parent_events_path.exists():
      return None
    lines_text = await asyncio.to_thread(parent_events_path.read_text, encoding='utf-8')
    lines = lines_text.splitlines()

    if event_index is not None:
      lines = lines[:event_index + 1]

    # Create the new session, inheriting the parent backend unless overridden.
    meta = SessionMetadata(
        name=f'Clone: {parent.name}',
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Copy chat events to new session
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '\n'.join(lines) + '\n', encoding='utf-8')

    # Append clone_start banner event
    clone_event = {
        "type": ET.CLONE_START,
        "parent_session_id": parent_id,
        "parent_session_name": parent.name,
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
    await append_ndjson(events_path, clone_event)

    await self._save_metadata(meta)

    log.info(
        'session_cloned',
        new_session=meta.id,
        parent=parent_id,
        event_index=event_index,
        backend=meta.backend,
    )
    return meta

  async def elone_session(
      self,
      parent_id: str,
      event_index: int,
      backend: str | None = None,
  ) -> Optional[SessionMetadata]:
    """Create an Elon-e session: empty history, archive + thumbs-down the parent."""
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    # Create new session with empty history
    meta = SessionMetadata(
        name=f'Elon-e: {parent.name}',
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Create empty chat_events.jsonl
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '', encoding='utf-8')

    await self._save_metadata(meta)

    # Auto-archive and thumbs-down the parent
    parent.status = SessionStatus.ARCHIVED
    parent.rating = 'thumbs_down'
    parent.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(parent)
    self._events_cache.pop(parent_id, None)
    self._usage_cache.pop(parent_id, None)

    log.info(
        'session_eloned',
        new_session=meta.id,
        parent=parent_id,
        event_index=event_index,
        backend=meta.backend,
    )
    return meta

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl."""
    return self._chat_events_path(session_id)

  async def rename_session(self, session_id: str, new_name: str) -> Optional[SessionMetadata]:
    """Rename a session and return the updated metadata."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.name = new_name
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_renamed", session_id=session_id, new_name=new_name)
    return meta

  async def mark_read(self, session_id: str) -> Optional[SessionMetadata]:
    """Clear the unread flag for a session."""
    meta = await self.get_session(session_id)
    if not meta or not meta.has_unread:
      return meta
    meta.has_unread = False
    await self._save_metadata(meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.UNREAD_CHANGED,
            "session_id": session_id,
            "has_unread": False,
        })
    return meta

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when master/workers produce output)."""
    meta = await self.get_session(session_id)
    if not meta or meta.has_unread:
      return
    meta.has_unread = True
    await self._save_metadata(meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.UNREAD_CHANGED,
            "session_id": session_id,
            "has_unread": True,
        })

  async def archive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as archived (does not delete files)."""
    meta = await self._update_field(session_id, "status", SessionStatus.ARCHIVED, "session_archived")
    self._events_cache.pop(session_id, None)
    self._usage_cache.pop(session_id, None)
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    session_dir = self._session_dir(session_id)
    if not session_dir.exists():
      return False
    await asyncio.to_thread(shutil.rmtree, session_dir)
    self._events_cache.pop(session_id, None)
    self._usage_cache.pop(session_id, None)
    self._invalidate_cache(session_id)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore an archived session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")

  async def mark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as waiting for confirmation."""
    return await self._update_field(session_id, "status", SessionStatus.WAITING, "session_mark_waiting")

  async def unmark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore a waiting session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unmark_waiting")

  async def star_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Star a session."""
    return await self._update_field(session_id, "starred", True, "session_starred")

  async def unstar_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Unstar a session."""
    return await self._update_field(session_id, "starred", False, "session_unstarred")

  async def set_group(self, session_id: str, group: Optional[str]) -> Optional[SessionMetadata]:
    """Set or clear the group for a session."""
    meta = await self._update_field(session_id, "group", group, "session_group_set")
    if meta:
      await streaming_manager.broadcast(
          "sidebar", {
              "type": ET.SESSION_GROUP_CHANGED,
              "session_id": session_id,
              "group": group,
          })
    return meta

  async def rename_group(self, old_name: str, new_name: str) -> int:
    """Rename a group across all sessions. Returns the count of updated sessions."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group == old_name:
        meta.group = new_name
        meta.updated_at = datetime.now(timezone.utc)
        await self._save_metadata(meta)
        count += 1
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group == group:
        meta.group = None
        meta.updated_at = datetime.now(timezone.utc)
        await self._save_metadata(meta)
        count += 1
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> None:
    """Persist a cc_session_id without clobbering unrelated metadata fields."""
    fresh = await self.get_session(session_id)
    if fresh:
      fresh.cc_session_id = cc_session_id
      await self._save_metadata(fresh)

  async def update_thinking_state(
      self,
      session_id: str,
      thinking_since: Optional[datetime] | object = _UNSET,
      updated_at: Optional[datetime] = None,
  ) -> None:
    """Persist thinking_since and/or updated_at without clobbering unrelated fields.

    Re-reads fresh metadata from disk before writing, so concurrent changes
    to fields like 'group' are preserved.
    """
    self._invalidate_cache(session_id)
    fresh = await self.get_session(session_id)
    if fresh:
      if thinking_since is not _UNSET:
        fresh.thinking_since = thinking_since
      if updated_at is not None:
        fresh.updated_at = updated_at
      await self._save_metadata(fresh)

  async def clear_thinking_since(self, session_id: str, cc_session_id: Optional[str] = None) -> None:
    """Clear thinking_since without clobbering unrelated metadata fields (e.g. has_unread).

    If cc_session_id is provided, also persist it (avoids race with persist_cc_session_id).
    """
    fresh = await self.get_session(session_id)
    if fresh:
      fresh.thinking_since = None
      if cc_session_id and fresh.cc_session_id != cc_session_id:
        fresh.cc_session_id = cc_session_id
      await self._save_metadata(fresh)

  def list_active_session_ids(self) -> list[SessionMetadata]:
    """Return metadata for active sessions by reading metadata.json files.

    Sync method — returns full SessionMetadata objects so callers avoid
    a second disk read. Populates the metadata cache as a side-effect.
    """
    if not self._cfg.sessions_dir.exists():
      return []
    results: list[SessionMetadata] = []
    now = time.monotonic()
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta_path = d / "metadata.json"
      if not meta_path.exists():
        continue
      try:
        raw = meta_path.read_text(encoding="utf-8")
        meta = SessionMetadata.model_validate_json(raw)
        if meta.status == SessionStatus.ACTIVE:
          self._metadata_cache[d.name] = (meta, now)
          results.append(meta.model_copy())
      except (json.JSONDecodeError, OSError, ValueError) as e:
        log.debug("list_active_ids_skip", dir=d.name, error=str(e))
    return results

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    if 'timestamp' not in event:
      event['timestamp'] = datetime.now(timezone.utc).isoformat()
    await append_ndjson(self._chat_events_path(session_id), event)
    # Keep in-memory cache in sync
    if session_id in self._events_cache:
      self._events_cache[session_id].append(event)
    # Incrementally update usage cache on result events
    if event.get('type') == 'result':
      self._update_usage_cache(session_id, event)

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    """Persist event (injecting timestamp) then broadcast to the session WebSocket channel."""
    await self.save_chat_event(session_id, event)
    await streaming_manager.broadcast(f"session:{session_id}", event)

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    if session_id in self._events_cache:
      return self._events_cache[session_id]
    events = parse_ndjson_file(self._chat_events_path(session_id))
    self._events_cache[session_id] = events
    # Always derive usage from a full load so the cache stays exact.
    usage = self.usage_from_events(events)
    if usage:
      self._usage_cache[session_id] = usage
    else:
      self._usage_cache.pop(session_id, None)
    return events

  def load_chat_events_tail(self, session_id: str, limit: int = 200) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk. Does NOT populate _events_cache.

    Returns (events, total_line_count, has_more).
    """
    events, total, has_more = parse_ndjson_tail(self._chat_events_path(session_id), limit)
    return events, total, has_more

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in line range [start, end). Returns (events, has_more)."""
    return parse_ndjson_range(self._chat_events_path(session_id), start, end)

  def _chat_events_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "chat_events.jsonl"

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
      events: list[dict] | None = None,
  ) -> dict | None:
    """Resolve display usage for a session view.

    Non-Codex backends keep the existing CharlieBot result-derived behavior.
    Codex backends override context usage from the native rollout log when the
    native thread id can be resolved.
    """
    usage = self.get_usage_cached(session_id)
    if usage is None and events is not None:
      usage = self.usage_from_events(events)
      if usage is not None:
        self._usage_cache[session_id] = usage
    if usage is None and events is None:
      await asyncio.to_thread(self.load_chat_events_sync, session_id)
      usage = self.get_usage_cached(session_id)

    if not self._is_codex_backend(session_meta.backend):
      return usage

    native_thread_id = await asyncio.to_thread(
        self._resolve_codex_thread_id,
        session_id,
        session_meta.cc_session_id,
        events,
    )
    if not native_thread_id:
      return usage

    native_usage = await asyncio.to_thread(self._load_codex_rollout_usage, native_thread_id)
    if native_usage is None:
      return usage

    merged_usage = dict(usage or {})
    merged_usage["context_tokens"] = native_usage["context_tokens"]
    merged_usage["context_limit"] = native_usage["context_limit"]
    merged_usage.setdefault("total_cost_usd", 0.0)
    merged_usage.setdefault("model", "")
    self._usage_cache[session_id] = merged_usage
    return merged_usage

  # ---------------------------------------------------------------------------
  # Usage / token tracking
  # ---------------------------------------------------------------------------

  @staticmethod
  def usage_from_events(events: list[dict]) -> dict | None:
    """Extract context-window token usage from pre-loaded events.

    Scans for the most recent 'result' event and accumulates total_cost_usd
    across ALL result events.

    Returns a dict with:
      context_tokens  – input + cache_creation + cache_read from last result
      context_limit   – from modelUsage contextWindow (default 200000)
      total_cost_usd  – sum across every result event
      model           – primary model name
    Returns None if no result events exist.
    """
    if not events:
      return None

    last_result: dict | None = None
    last_usage_result: dict | None = None
    total_cost = 0.0

    for ev in events:
      if ev.get("type") != "result":
        continue
      last_result = ev
      total_cost += ev.get("total_cost_usd", 0.0)
      u = ev.get("usage", {})
      if u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0) > 0:
        last_usage_result = ev

    if last_result is None:
      return None

    usage_source = last_usage_result or last_result
    usage = usage_source.get("usage", {})
    context_tokens = (
        usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0) +
        usage.get("cache_read_input_tokens", 0))

    model_usage = usage_source.get("modelUsage", {})
    context_limit = 200_000
    model = ""
    for model_name, info in model_usage.items():
      model = model_name
      context_limit = info.get("contextWindow", 200_000)
      break

    return {
        "context_tokens": context_tokens,
        "context_limit": context_limit,
        "total_cost_usd": round(total_cost, 4),
        "model": model,
    }

  def get_usage_cached(self, session_id: str) -> dict | None:
    """Return cached usage data for a session, or None if not yet computed."""
    return self._usage_cache.get(session_id)

  def _update_usage_cache(self, session_id: str, result_event: dict) -> None:
    """Incrementally update the usage cache from a single 'result' event."""
    cached = self._usage_cache.get(session_id) or {
        "context_tokens": 0,
        "context_limit": 200_000,
        "total_cost_usd": 0.0,
        "model": "",
    }
    cached["total_cost_usd"] = round(cached["total_cost_usd"] + result_event.get("total_cost_usd", 0.0), 4)
    u = result_event.get("usage", {})
    ctx = u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0)
    if ctx > 0:
      cached["context_tokens"] = ctx
      model_usage = result_event.get("modelUsage", {})
      for model_name, info in model_usage.items():
        cached["model"] = model_name
        cached["context_limit"] = info.get("contextWindow", 200_000)
        break
    self._usage_cache[session_id] = cached

  def _is_codex_backend(self, backend_id: str) -> bool:
    option_getter = getattr(self._cfg, "get_backend_option", None)
    if callable(option_getter):
      option = option_getter(backend_id)
      if option is not None:
        return option.type == "codex"
    return backend_id.startswith("codex")

  @staticmethod
  def _extract_translated_session_id(events: list[dict]) -> str | None:
    for event in events:
      session_id = event.get("session_id")
      if isinstance(session_id, str) and session_id:
        return session_id
    return None

  def _read_translated_session_id(self, session_id: str) -> str | None:
    cached_events = self._events_cache.get(session_id)
    if cached_events is not None:
      cached_session_id = self._extract_translated_session_id(cached_events)
      if cached_session_id:
        return cached_session_id

    path = self._chat_events_path(session_id)
    if not path.exists():
      return None

    with open(path, "r", encoding="utf-8") as f:
      for raw_line in f:
        line = raw_line.strip()
        if not line:
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError as e:
          log.debug("translated_session_id_parse_skip", path=str(path), error=str(e))
          continue
        session_id_value = event.get("session_id")
        if isinstance(session_id_value, str) and session_id_value:
          return session_id_value
    return None

  def _resolve_codex_thread_id(
      self,
      session_id: str,
      persisted_session_id: str | None,
      events: list[dict] | None = None,
  ) -> str | None:
    if persisted_session_id:
      return persisted_session_id
    if events is not None:
      live_session_id = self._extract_translated_session_id(events)
      if live_session_id:
        return live_session_id
    return self._read_translated_session_id(session_id)

  def _find_codex_rollout_path(self, native_thread_id: str) -> Path | None:
    cached_path = self._codex_rollout_path_cache.get(native_thread_id)
    if cached_path is not None and cached_path.exists():
      return cached_path

    if not _CODEX_SESSIONS_DIR.exists():
      return None

    matches = list(_CODEX_SESSIONS_DIR.rglob(f"rollout-*{native_thread_id}.jsonl"))
    if not matches:
      return None

    matches.sort(key=lambda path: path.stat().st_mtime, reverse=True)
    rollout_path = matches[0]
    self._codex_rollout_path_cache[native_thread_id] = rollout_path
    return rollout_path

  def _load_codex_rollout_usage(self, native_thread_id: str) -> dict | None:
    rollout_path = self._find_codex_rollout_path(native_thread_id)
    if rollout_path is None:
      return None

    stat = rollout_path.stat()
    cached_usage = self._codex_rollout_usage_cache.get(native_thread_id)
    if cached_usage is not None:
      cached_mtime_ns, cached_size, usage = cached_usage
      if cached_mtime_ns == stat.st_mtime_ns and cached_size == stat.st_size:
        return usage

    usage = _extract_latest_codex_rollout_usage(rollout_path)
    self._codex_rollout_usage_cache[native_thread_id] = (stat.st_mtime_ns, stat.st_size, usage)
    return usage

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  def _invalidate_cache(self, session_id: str) -> None:
    """Remove a session from the metadata cache."""
    self._metadata_cache.pop(session_id, None)

  async def _update_field(self, session_id: str, field: str, value: Any, log_event: str) -> Optional[SessionMetadata]:
    """Get a session, set one field, save, and log. Returns None if session not found."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    setattr(meta, field, value)
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info(log_event, session_id=session_id)
    return meta

  async def _has_running_tasks(self, session_id: str) -> bool:
    """Check if a session has any threads with status 'running'."""

    def _check():
      threads_dir = self._session_dir(session_id) / "threads"
      if not threads_dir.exists():
        return False
      for thread_dir in threads_dir.iterdir():
        meta_path = thread_dir / "metadata.json"
        if not meta_path.exists():
          continue
        try:
          meta = json.loads(meta_path.read_text(encoding="utf-8"))
          if meta.get("status") == "running":
            return True
        except (json.JSONDecodeError, OSError) as e:
          log.debug('thread_meta_read_failed', thread_dir=thread_dir.name, error=str(e))
          continue
      return False

    return await asyncio.to_thread(_check)

  async def _get_pending_trigger_state(self, session_id: str) -> tuple[int, Optional[datetime]]:
    """Return the number of pending delayed triggers and the earliest fire time."""

    def _check() -> tuple[int, Optional[datetime]]:
      triggers_dir = self._session_dir(session_id) / "triggers"
      if not triggers_dir.exists():
        return 0, None

      pending_count = 0
      next_trigger_at: Optional[datetime] = None
      for trigger_path in triggers_dir.glob("*.json"):
        try:
          trigger = json.loads(trigger_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, ValueError) as e:
          log.debug("trigger_meta_read_failed", trigger_path=str(trigger_path), error=str(e))
          continue
        if trigger.get("status") != "pending":
          continue

        pending_count += 1
        fire_at_raw = trigger.get("fire_at")
        if not fire_at_raw:
          continue
        try:
          fire_at = datetime.fromisoformat(fire_at_raw.replace("Z", "+00:00"))
        except ValueError as e:
          log.debug("trigger_fire_at_parse_failed", trigger_path=str(trigger_path), error=str(e))
          continue
        if fire_at.tzinfo is None:
          fire_at = fire_at.replace(tzinfo=timezone.utc)
        if next_trigger_at is None or fire_at < next_trigger_at:
          next_trigger_at = fire_at

      return pending_count, next_trigger_at

    return await asyncio.to_thread(_check)

  async def _enrich_and_sort(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Populate sidebar state, normalize timezone-naive updated_at to UTC, and sort newest first."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )
    for s in sessions:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> None:
    """Public wrapper for _populate_sidebar_state."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )

  async def _populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> None:
    """Populate derived sidebar-only state on session metadata objects."""
    if not sessions:
      return

    running_future = None
    trigger_future = None
    if include_running_status:
      running_future = asyncio.gather(*(self._has_running_tasks(meta.id) for meta in sessions))
    if include_pending_trigger_status:
      trigger_future = asyncio.gather(*(self._get_pending_trigger_state(meta.id) for meta in sessions))

    if running_future and trigger_future:
      running_flags, trigger_states = await asyncio.gather(running_future, trigger_future)
    elif running_future:
      running_flags = await running_future
      trigger_states = None
    elif trigger_future:
      running_flags = None
      trigger_states = await trigger_future
    else:
      return

    if running_flags is not None:
      for meta, running in zip(sessions, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running

    if trigger_states is not None:
      for meta, (pending_count, next_trigger_at) in zip(sessions, trigger_states):
        meta.has_pending_trigger = pending_count > 0
        meta.pending_trigger_count = pending_count
        meta.next_trigger_at = next_trigger_at

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. using a persistent counter file.

    Reads the next number from sessions_dir/.counter (O(1) instead of listing
    all sessions). Falls back to counting directories if the file is missing.
    """
    counter_path = self._cfg.sessions_dir / ".counter"

    def _read_and_increment() -> int:
      self._cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
      if counter_path.exists():
        try:
          n = int(counter_path.read_text().strip())
        except (ValueError, OSError):
          n = self._count_session_dirs()
      else:
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

  async def _save_metadata(self, meta: SessionMetadata) -> None:
    path = self._metadata_path(meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    serialized = meta.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)
    async with aiofiles.open(path, "w") as f:
      await f.write(serialized)
    self._metadata_cache[meta.id] = (SessionMetadata.model_validate_json(serialized), time.monotonic())

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
