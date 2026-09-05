"""Thread management API routes."""

import asyncio
import hashlib
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
from fastapi import APIRouter, Depends, HTTPException, Query, Response

from src.agents.backends.pty_common import (
    _TMUX_SOCKET,
    tmux_session_exists,
    tmux_session_name,
)
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.message_utils import extract_text_from_message, extract_tool_result_text
from src.api.responses import FastJsonResponse
from src.core.config import CharlieBotConfig, get_config
from src.core.models import (
    BackendType,
    ThreadMetadata,
    ThreadStatus,
    WorkerEvent,
)
from src.core.ndjson import iter_ndjson_events
from src.core.process import kill_process_group
from src.core.threads import ThreadManager, iter_thread_meta_stats
from src.core.triggers import TriggerManager

log = structlog.get_logger()

router = APIRouter()

# Cap on the description prefix shipped in the workers-panel list rows;
# generously past the one truncated line the card paints at any sidebar width.
_LIST_DESCRIPTION_CAP = 240

# Parsed-meta memo behind the thread-detail endpoint. The endpoint reads the
# meta without mutating it, so the memoized instance is shared read-only
# (mutating callers go through ThreadManager.get_thread, which re-reads). Every
# writer publishes metadata.json through the atomic tmp rename, so a content
# change always moves (mtime_ns, size); the signature is taken before the read,
# so an entry recorded mid-rewrite keys the older signature and can never be
# served for the newer bytes.
_DETAIL_META_MEMO_LIMIT = 32
_detail_meta_memo: OrderedDict[str, tuple[tuple[int, int], ThreadMetadata]] = OrderedDict()


async def _detail_thread_meta(thread_mgr: ThreadManager, session_id: str, thread_id: str) -> ThreadMetadata | None:
  path = thread_mgr.thread_dir(session_id, thread_id) / "metadata.json"
  key = str(path)
  try:
    st = os.stat(path)
  except OSError:
    _detail_meta_memo.pop(key, None)
    return None
  sig = (st.st_mtime_ns, st.st_size)
  hit = _detail_meta_memo.get(key)
  if hit is not None and hit[0] == sig:
    _detail_meta_memo.move_to_end(key)
    return hit[1]
  meta = await thread_mgr.get_thread(session_id, thread_id)
  if meta is None:
    return None
  _detail_meta_memo[key] = (sig, meta)
  while len(_detail_meta_memo) > _DETAIL_META_MEMO_LIMIT:
    _detail_meta_memo.popitem(last=False)
  return meta


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
  if dispatch.type == BackendType.TUI_CLI:
    return thread.session_id
  if dispatch.type == BackendType.CC_CLAUDE and dispatch.cli_binary == "claude-sub":
    return thread.claude_session_id
  return None


def build_attach_command(thread: ThreadMetadata, cfg: CharlieBotConfig | None = None) -> str | None:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return None

  if dispatch.type == BackendType.CC_CLAUDE:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return _tmux_attach_command(tmux_id, read_only=dispatch.cli_binary == "claude-sub")
    if not thread.worktree_path or not thread.claude_session_id:
      return None
    return f"cd {shlex.quote(thread.worktree_path)} && claude --resume {shlex.quote(thread.claude_session_id)}"
  if dispatch.type == BackendType.TUI_CLI:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id is None:
      return None
    return _tmux_attach_command(tmux_id)
  return None


async def _attach_available(thread: ThreadMetadata, cfg: CharlieBotConfig) -> bool:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return False

  if dispatch.type == BackendType.CC_CLAUDE:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return await tmux_session_exists(tmux_id)
    return bool(thread.claude_session_id and thread.worktree_path and os.path.isdir(thread.worktree_path))
  if dispatch.type == BackendType.TUI_CLI:
    tmux_id = _tmux_attach_id(thread, dispatch)
    return bool(tmux_id and await tmux_session_exists(tmux_id))
  return False


def _thread_list_item(t: ThreadMetadata) -> dict:
  """One thread row of the workers-panel list payload.

  The description ships as a prefix: the card it backs paints one CSS-truncated
  line and the full-text modal fetches the thread row on click, so the 3 s
  poll never ships task-spec-length descriptions (~KB each). A truncated row
  carries ``description_full_len`` so the client knows to fetch.
  """
  description = t.description or ""
  item = {
      "type": "thread",
      "id": t.id,
      "description": description[:_LIST_DESCRIPTION_CAP],
      "status": t.status.value,
      "created_at": t.created_at.isoformat(),
      "completed_at": t.completed_at.isoformat() if t.completed_at else None,
      "backend": t.backend,
  }
  if len(description) > _LIST_DESCRIPTION_CAP:
    item["description_full_len"] = len(description)
  return item


# Whole-body memo for the 3 s workers-panel list poll: body bytes per session
# keyed on the union file signature. Every row field derives from thread
# metadata.json and trigger *.json files, and every writer rewrites those
# files atomically (a rename always moves mtime_ns), so an unchanged signature
# proves the built body is still current. Single slot per session with an LRU
# cap: one slot holds the ~140 KB worst body, and deeper caps buy nothing
# because a session's poll reuses its one slot.
_LIST_BODY_MEMO_LIMIT = 8
_list_body_memo: OrderedDict[str, tuple[tuple[tuple[str, int, int], ...], bytes, str]] = OrderedDict()


def _list_body_signature(threads_dir: str, triggers_dir: str) -> tuple[tuple[str, int, int], ...]:
  """(path, mtime_ns, size) of every row-source file of the list payload.

  The thread scan is ``iter_thread_meta_stats``; a directory that cannot be
  scanned contributes nothing, and the signature keys exactly the files the
  thread rows are built from.
  """
  sig: list[tuple[str, int, int]] = []
  try:
    for meta_path, st in iter_thread_meta_stats(threads_dir):
      sig.append((meta_path, st.st_mtime_ns, st.st_size))
  except OSError:
    pass
  try:
    for entry in os.scandir(triggers_dir):
      if not entry.is_file() or not entry.name.endswith(".json"):
        continue
      try:
        st = entry.stat()
      except OSError:
        continue
      sig.append((entry.path, st.st_mtime_ns, st.st_size))
  except OSError:
    pass
  sig.sort()
  return tuple(sig)


@router.get("/{session_id}/list")
async def list_threads(
    session_id: str,
    etag: str | None = Query(default=None),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
    cfg: CharlieBotConfig = Depends(get_config),
) -> Response:
  """Return mixed list of thread and trigger summaries, sorted by created_at descending.

  The body carries a strong ETag (sha1 of the body bytes). A poll repeating the
  ETag it rendered via ``?etag=`` is answered 204 with no body. The conditional
  rides a query param rather than If-None-Match because the browser's HTTP
  cache fulfils a revalidation itself and fetch never surfaces the 304; the
  no-store on every answer keeps each poll a real request.
  """
  session_dir = cfg.sessions_dir / session_id
  sig = await asyncio.to_thread(_list_body_signature, str(session_dir / "threads"), str(session_dir / "triggers"))
  hit = _list_body_memo.get(session_id)
  if hit is not None and hit[0] == sig:
    _list_body_memo.move_to_end(session_id)
    if etag == hit[2]:
      return Response(status_code=204, headers={"ETag": hit[2], "Cache-Control": "no-store"})
    return Response(
        content=hit[1],
        media_type="application/json",
        headers={
            "ETag": hit[2],
            "Cache-Control": "no-store"
        },
    )

  threads = await thread_mgr.list_threads(session_id)
  thread_items = [_thread_list_item(t) for t in threads]

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
  # These dumps flags and the mapped-return serialization this replaces ship
  # byte-identical bodies (verified on the 277-row worst-session corpus), so a
  # memo hit and a fresh build are indistinguishable on the wire.
  body = json.dumps(combined, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")
  etag_value = '"' + hashlib.sha1(body).hexdigest() + '"'
  _list_body_memo[session_id] = (sig, body, etag_value)
  _list_body_memo.move_to_end(session_id)
  while len(_list_body_memo) > _LIST_BODY_MEMO_LIMIT:
    _list_body_memo.popitem(last=False)
  if etag == etag_value:
    return Response(status_code=204, headers={"ETag": etag_value, "Cache-Control": "no-store"})
  return Response(
      content=body,
      media_type="application/json",
      headers={
          "ETag": etag_value,
          "Cache-Control": "no-store"
      },
  )


@router.get("/{session_id}/threads/{thread_id}")
async def get_thread(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
    attach: bool = Query(default=False),
):
  """Return a thread's metadata plus the derived attach pair.

  With ``attach`` the response is only ``{"attach_command", "attach_available"}``
  — the 5 s poll's shape (the poll reads nothing else of the row). Without it
  the response is the full row minus ``context`` (the task-spec body; no HTTP
  consumer reads it off this endpoint, and the modal fetches ``description``
  once per click).
  """
  meta = await _detail_thread_meta(thread_mgr, session_id, thread_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Thread not found")
  attach_command = build_attach_command(meta, cfg)
  attach_available = await _attach_available(meta, cfg)
  if attach:
    return FastJsonResponse({
        "attach_command": attach_command,
        "attach_available": attach_available,
    })
  payload = meta.model_dump(mode="json")
  del payload["context"]
  payload["attach_command"] = attach_command
  payload["attach_available"] = attach_available
  return FastJsonResponse(payload)


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
        raw_events = list(
            iter_ndjson_events(window[:complete_end].split(b"\n"), log_event="ndjson_parse_skip", log_fields={}))
        _append_worker_events(raw_events, entry.events, entry.tool_id_to_name)
        entry.offset += complete_end
    _thread_events_cache[key] = entry
    while len(_thread_events_cache) > _THREAD_EVENTS_CACHE_CAP:
      _thread_events_cache.popitem(last=False)
    return list(entry.events)


def _append_worker_events(
    raw_events: Iterable[dict], events: list[WorkerEvent], tool_id_to_name: dict[str, str]) -> None:
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
  return FastJsonResponse(
      {
          "events": [e.model_dump(mode="json") for e in events[start:]],
          "total": len(events),
          "reset": reset
      })


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
