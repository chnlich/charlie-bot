"""Thread management API routes."""

import asyncio
import json
import os
import shlex
import threading
from collections import OrderedDict
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import JSONResponse

from src.agents.backends.pty_common import (
  _TMUX_SOCKET,
  tmux_session_exists,
  tmux_session_name,
)
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.message_utils import extract_text_from_message, extract_tool_result_text
from src.core.config import CharlieBotConfig, get_config
from src.core.models import (
  ThreadMetadata,
  ThreadStatus,
  WorkerEvent,
)
from src.core.process import kill_process_group
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

log = structlog.get_logger()

router = APIRouter()


class ThreadMetadataResponse(ThreadMetadata):
  attach_command: str | None = None
  attach_available: bool = False


@dataclass(frozen=True)
class _BackendDispatch:
  type: str
  cli_binary: str | None = None


def _backend_dispatch(thread: ThreadMetadata, cfg: CharlieBotConfig | None) -> _BackendDispatch | None:
  if not thread.backend:
    return None
  if cfg is not None:
    option = cfg.get_backend_option(thread.backend)
    if option is not None:
      return _BackendDispatch(type=option.type, cli_binary=option.cli_binary)
  return _BackendDispatch(type=thread.backend)


def _tmux_attach_command(session_id: str, *, read_only: bool = False) -> str:
  command = ["tmux", "-L", _TMUX_SOCKET, "attach"]
  if read_only:
    command.append("-r")
  command.extend(["-t", tmux_session_name(session_id)])
  return shlex.join(command)


def _tmux_attach_id(thread: ThreadMetadata, dispatch: _BackendDispatch) -> str | None:
  if dispatch.type == "tui-cli":
    return thread.session_id
  if dispatch.type == "cc-claude" and dispatch.cli_binary == "claude-sub":
    return thread.claude_session_id
  return None


def build_attach_command(thread: ThreadMetadata, cfg: CharlieBotConfig | None = None) -> str | None:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return None

  if dispatch.type == "cc-claude":
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return _tmux_attach_command(tmux_id, read_only=dispatch.cli_binary == "claude-sub")
    if not thread.worktree_path or not thread.claude_session_id:
      return None
    return f"cd {shlex.quote(thread.worktree_path)} && claude --resume {shlex.quote(thread.claude_session_id)}"
  if dispatch.type == "cc-kimi":
    return None
  if dispatch.type == "cc-openai-compatible":
    return None
  if dispatch.type == "codex":
    return None
  if dispatch.type == "charlie-code":
    return None
  if dispatch.type == "gemini":
    return None
  if dispatch.type == "opencode":
    return None
  if dispatch.type == "antigravity":
    return None
  if dispatch.type == "tui-cli":
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id is None:
      return None
    return _tmux_attach_command(tmux_id)
  return None


async def _attach_available(thread: ThreadMetadata, cfg: CharlieBotConfig) -> bool:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return False

  if dispatch.type == "cc-claude":
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return await tmux_session_exists(tmux_id)
    return bool(thread.claude_session_id and thread.worktree_path and os.path.isdir(thread.worktree_path))
  if dispatch.type == "cc-kimi":
    return False
  if dispatch.type == "cc-openai-compatible":
    return False
  if dispatch.type == "codex":
    return False
  if dispatch.type == "charlie-code":
    return False
  if dispatch.type == "gemini":
    return False
  if dispatch.type == "opencode":
    return False
  if dispatch.type == "antigravity":
    return False
  if dispatch.type == "tui-cli":
    tmux_id = _tmux_attach_id(thread, dispatch)
    return bool(tmux_id and await tmux_session_exists(tmux_id))
  return False


@router.get("/{session_id}/list")
async def list_threads(
    session_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
) -> list[dict]:
  """Return mixed list of thread and trigger summaries, sorted by created_at descending."""
  threads = await thread_mgr.list_threads(session_id)
  thread_items = [
      {
          "type": "thread",
          "id": t.id,
          "description": t.description,
          "status": t.status.value,
          "created_at": t.created_at.isoformat(),
          "completed_at": t.completed_at.isoformat() if t.completed_at else None,
          "backend": t.backend,
      } for t in threads
  ]

  triggers = await trigger_mgr.list_triggers(session_id)
  trigger_items = [
      {
          "type": "trigger",
          "id": tr.id,
          "message": tr.message,
          "status": tr.status.value,
          "fire_at": tr.fire_at.isoformat(),
          "created_at": tr.created_at.isoformat(),
      } for tr in triggers
  ]

  combined = thread_items + trigger_items
  combined.sort(key=lambda x: x["created_at"], reverse=True)
  return combined


@router.get("/{session_id}/threads/{thread_id}", response_model=ThreadMetadataResponse)
async def get_thread(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  meta = await thread_mgr.get_thread(session_id, thread_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Thread not found")
  return ThreadMetadataResponse(
      **meta.model_dump(),
      attach_command=build_attach_command(meta, cfg),
      attach_available=await _attach_available(meta, cfg),
  )


# Reads from read_thread_worker_events, per events-log path. The workers-panel
# poll re-reads the same append-only log every 5 s per expanded running worker,
# so parsed results are retained and a call parses only the bytes appended since
# the last one. A file that shrank (truncate/rewrite) restarts its entry.
_THREAD_EVENTS_CACHE_CAP = 32


class _ThreadEventsCacheEntry:
  __slots__ = ("events", "offset", "tool_id_to_name")

  def __init__(self) -> None:
    self.events: list[WorkerEvent] = []
    self.offset = 0
    self.tool_id_to_name: dict[str, str] = {}


_thread_events_cache: OrderedDict[str, _ThreadEventsCacheEntry] = OrderedDict()
_thread_events_lock = threading.Lock()


def read_thread_worker_events(events_path: Path) -> list[WorkerEvent]:
  """Project a worker's events.jsonl into the events endpoint's full WorkerEvent list.

  The caller gets the complete projected history on every call; an unchanged
  log costs one stat, and between polls only newly appended complete lines are
  parsed. A trailing partial line (writer mid-append) is left for the next
  call, so nothing the writer has not committed reaches the projection.
  """
  key = str(events_path)
  with _thread_events_lock:
    if not events_path.exists():
      _thread_events_cache.pop(key, None)
      return []
    entry = _thread_events_cache.get(key)
    if entry is not None:
      _thread_events_cache.move_to_end(key)
    size = events_path.stat().st_size
    if entry is None or size < entry.offset:
      entry = _ThreadEventsCacheEntry()
    if size > entry.offset:
      with open(events_path, "rb") as f:
        f.seek(entry.offset)
        window = f.read(size - entry.offset)
      complete_end = window.rfind(b"\n") + 1
      if complete_end:
        raw_events: list[dict] = []
        for raw_line in window[:complete_end].split(b"\n"):
          line = raw_line.strip()
          if not line:
            continue
          try:
            raw_events.append(json.loads(line))
          except json.JSONDecodeError as e:
            log.debug("ndjson_parse_skip", error=str(e))
        _append_worker_events(raw_events, entry.events, entry.tool_id_to_name)
        entry.offset += complete_end
    _thread_events_cache[key] = entry
    while len(_thread_events_cache) > _THREAD_EVENTS_CACHE_CAP:
      _thread_events_cache.popitem(last=False)
    return list(entry.events)


def _append_worker_events(
    raw_events: Iterable[dict], events: list[WorkerEvent], tool_id_to_name: dict[str, str]
) -> None:
  for data in raw_events:
    event_timestamp = data.get("timestamp") or datetime.now(UTC)
    event_type = data.get('type', '')
    if event_type == 'assistant' and isinstance(data.get('message'), dict):
      text = extract_text_from_message(data['message'])
      if text:
        events.append(WorkerEvent(type='assistant', content=text, timestamp=event_timestamp))
      for block in data['message'].get('content', []):
        if isinstance(block, dict) and block.get('type') == 'tool_use':
          tool_id_to_name[block['id']] = block['name']
          events.append(
              WorkerEvent(
                  type='tool_use',
                  tool_name=block['name'],
                  input=block.get('input', {}),
                  timestamp=event_timestamp,
              ))
    elif event_type == 'user' and isinstance(data.get('message'), dict):
      for block in data['message'].get('content', []):
        if block.get('type') == 'tool_result':
          tool_use_id = block.get('tool_use_id', '')
          name = tool_id_to_name.get(tool_use_id, '')
          result_text = extract_tool_result_text(block)
          events.append(
              WorkerEvent(
                  type='tool_result',
                  tool_name=name,
                  content=result_text,
                  timestamp=event_timestamp,
              ))
    else:
      try:
        events.append(WorkerEvent(**{k: v for k, v in data.items() if k in WorkerEvent.model_fields}))
      except Exception as e:
        log.debug('event_parse_failed', error=str(e))
        events.append(WorkerEvent(type='raw', content=str(data)))


@router.get("/{session_id}/threads/{thread_id}/events", response_model=list[WorkerEvent])
async def get_thread_events(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    after: int | None = Query(default=None, ge=0),
):
  """Return historical Worker events from the on-disk events.jsonl log.

  Without ``after`` the response is the full projected list. With ``after``
  (the client's rendered raw count) it is an envelope ``{"events", "total",
  "reset"}`` carrying only the events past that count; ``reset`` marks the
  count as ahead of the projection (log replaced), so the client re-renders
  the envelope's full payload. The projection is append-only
  (_append_worker_events never rewrites an emitted row), so a count inside
  it is a sound prefix cut.
  """
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(read_thread_worker_events, events_path)
  if after is None:
    return events
  reset = after > len(events)
  start = 0 if reset else after
  # A returned Response skips response_model validation; model_dump(mode="json") is
  # ~6x faster than the jsonable_encoder pass FastAPI runs on mapped returns.
  return JSONResponse(
      {"events": [e.model_dump(mode="json") for e in events[start:]], "total": len(events), "reset": reset})


@router.post("/{session_id}/threads/{thread_id}/cancel")
async def cancel_thread(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Cancel a running thread (sends SIGTERM to the subprocess via streaming manager)."""
  thread = await thread_mgr.get_thread(session_id, thread_id)
  if not thread:
    raise HTTPException(status_code=404, detail="Thread not found")

  if thread.pid:
    kill_process_group(thread.pid)

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.CANCELLED)
  return {"ok": True}
