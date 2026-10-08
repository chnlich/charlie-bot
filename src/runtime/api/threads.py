"""Compatibility routes for task Runs addressed through their old thread ids."""

import asyncio
import hashlib
import threading
from collections.abc import Iterable
from datetime import UTC, datetime
from pathlib import Path

from fastapi import APIRouter, Depends, HTTPException, Query, Request, Response

from src.infra import event_types as ET
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import BoundedMemo
from src.infra.models import RunRecord, WorkerEvent
from src.infra.ndjson import PARSE_SKIP_LOG_EVENT, iter_ndjson_events
from src.infra.responses import FastJsonResponse, fast_json_bytes, gzip_body_response
from src.runtime.api.deps import get_run_store, get_task_manager, require_caller
from src.runtime.hooks import backend_types
from src.runtime.message_aggregator import (
    TOOL_PREVIEW_CHARS,
    extract_text_from_message,
    extract_tool_result_text,
    tool_preview,
)
from src.runtime.run_token import CallerIdentity
from src.runtime.runs import (
    RUN_EVENTS_NAME,
    RunIdentityConflictError,
    RunNotFoundError,
    read_host_boot_time,
    stop_requested_in_events,
    terminal_outcome_in_events,
)

log = LazyStructlogLogger()
router = APIRouter()
_THREAD_NOT_FOUND_DETAIL = "Thread not found"
_LIST_DESCRIPTION_CAP = 100
_RUN_LIST_GZIP_MEMO: BoundedMemo[bytes, bytes] = BoundedMemo(8)
_RUN_DETAIL_GZIP_MEMO: BoundedMemo[bytes, bytes] = BoundedMemo(8)
_RUN_EVENTS_GZIP_MEMO: BoundedMemo[bytes, bytes] = BoundedMemo(8)


def _run_status(run: RunRecord, events: list[dict], host_boot: datetime) -> str:
  """The status string the retained thread-id client expects for a task Run."""
  from src.runtime.runs import is_run_alive

  outcome = terminal_outcome_in_events(events, run.id)
  if outcome == "success":
    return "completed"
  if outcome is not None:
    return "failed"
  if run.pid is None:
    return "cancelled" if stop_requested_in_events(events, run.id) else "idle"
  return "running" if is_run_alive(run.pid, run.pid_start, run.started_at, host_boot) else "failed"


def _run_list_item(
    run: RunRecord,
    events: list[dict],
    host_boot: datetime,
    *,
    created_at: datetime,
    description: str,
) -> dict:
  """Render one ephemeral compatibility row from the Run record and facts."""
  item = {
      "type": "thread",
      "id": run.id,
      "description": description[:_LIST_DESCRIPTION_CAP],
      "status": _run_status(run, events, host_boot),
      "created_at": int(created_at.timestamp() * 1000),
      "completed_at": int(run.ended_at.timestamp() * 1000) if run.ended_at else None,
      "backend": run.backend,
      "session_id": run.session_id,
  }
  if len(description) > _LIST_DESCRIPTION_CAP:
    item["description_full_len"] = len(description)
  if run.pid is not None:
    item["pid"] = run.pid
  if run.branch_name:
    item["branch_name"] = run.branch_name
  if run.worktree_path:
    item["worktree_path"] = run.worktree_path
  return item


@router.get("/{session_id}/list")
async def list_threads(
    request: Request,
    session_id: str,
    etag: str | None = Query(default=None),
) -> Response:
  """List task Runs in the compatibility row shape, newest first."""
  tree = await get_task_manager()
  meta = await tree.load_meta(session_id)
  description = (meta.task.goal if meta is not None and meta.task is not None else "") or ""
  events = tree.runs.load_events_sync(session_id)
  host_boot = await asyncio.to_thread(read_host_boot_time)
  rows = []
  for run in await asyncio.to_thread(tree.runs.list_run_records_sync, session_id):
    created_at = run.started_at or run.ended_at or datetime.now(UTC)
    rows.append(_run_list_item(run, events, host_boot, created_at=created_at, description=description))
  rows.sort(key=lambda row: row["created_at"], reverse=True)
  body = fast_json_bytes(rows)
  etag_value = '"' + hashlib.sha1(body).hexdigest() + '"'
  if etag == etag_value:
    return Response(status_code=204, headers={"ETag": etag_value, "Cache-Control": "no-store"})
  return await gzip_body_response(
      request, body, {"ETag": etag_value, "Cache-Control": "no-store"}, _RUN_LIST_GZIP_MEMO)


async def _resolve_run(owner_session_id: str, thread_id: str) -> tuple[str, str] | None:
  """Resolve a retained thread address to its owning task Run."""
  tree = await get_task_manager()
  target = tree.aliases.resolve_thread(owner_session_id, thread_id)
  if not target:
    return None
  session_id, run_id = target["session_id"], target["run_id"]
  run = await asyncio.to_thread(tree.runs.read_run_sync, session_id, run_id)
  return (session_id, run_id) if run is not None else None


@router.get("/{session_id}/threads/{thread_id}")
async def get_thread(session_id: str, thread_id: str, request: Request) -> Response:
  """Return the Run record behind a retained thread-id alias."""
  target = await _resolve_run(session_id, thread_id)
  if target is None:
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
  tree = await get_task_manager()
  run = await asyncio.to_thread(tree.runs.read_run_sync, target[0], target[1])
  if run is None:
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
  meta = await tree.load_meta(target[0])
  description = (meta.task.goal if meta is not None and meta.task is not None else "") or ""
  events = tree.runs.load_events_sync(target[0])
  row = _run_list_item(
      run,
      events,
      await asyncio.to_thread(read_host_boot_time),
      created_at=run.started_at or run.ended_at or datetime.now(UTC),
      description=description,
  )
  row["session_id"] = target[0]
  row["description_full"] = description
  return await gzip_body_response(request, fast_json_bytes(row), {}, _RUN_DETAIL_GZIP_MEMO)


# Incremental event projection for a Run's transport log. The endpoint's path
# keeps its old thread-id spelling so clients can address a delegated Run.
_THREAD_EVENTS_CACHE_CAP = 32


class _ThreadEventsCacheEntry:
  __slots__ = ("events", "full_body", "offset", "tool_id_to_name")

  def __init__(self) -> None:
    self.events: list[WorkerEvent] = []
    self.offset = 0
    self.tool_id_to_name: dict[str, str] = {}
    self.full_body: bytes | None = None


_thread_events_cache: BoundedMemo[str, _ThreadEventsCacheEntry] = BoundedMemo(_THREAD_EVENTS_CACHE_CAP)
_thread_events_lock = threading.Lock()


def read_thread_worker_events(events_path: Path) -> list[WorkerEvent]:
  """Project a Run's worker events log, parsing only newly appended complete lines."""
  key = str(events_path)
  with _thread_events_lock:
    if not events_path.exists():
      _thread_events_cache.drop(key)
      return []
    entry = _thread_events_cache.get(key)
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
            iter_ndjson_events(window[:complete_end].split(b"\n"), log_event=PARSE_SKIP_LOG_EVENT, log_fields={}))
        _append_worker_events(raw_events, entry.events, entry.tool_id_to_name)
        entry.offset += complete_end
        entry.full_body = None
    _thread_events_cache.store(key, entry)
    return list(entry.events)


def _thread_events_snapshot(events_path: Path) -> tuple[list[WorkerEvent], _ThreadEventsCacheEntry, int] | None:
  """Return an unchanged-log projection plus its cache token, or None."""
  key = str(events_path)
  if not _thread_events_lock.acquire(blocking=False):
    return None
  try:
    entry = _thread_events_cache.get(key)
    if entry is None:
      return None
    try:
      size = events_path.stat().st_size
    except OSError:
      return None
    if size != entry.offset:
      return None
    return list(entry.events), entry, entry.offset
  finally:
    _thread_events_lock.release()


def read_thread_worker_events_memo_hit(events_path: Path) -> list[WorkerEvent] | None:
  snap = _thread_events_snapshot(events_path)
  return None if snap is None else snap[0]


def stored_thread_events_full_body(events_path: Path) -> bytes | None:
  key = str(events_path)
  if not _thread_events_lock.acquire(blocking=False):
    return None
  try:
    entry = _thread_events_cache.get(key)
    if entry is None or entry.full_body is None:
      return None
    try:
      size = events_path.stat().st_size
    except OSError:
      return None
    if size != entry.offset:
      return None
    return entry.full_body
  finally:
    _thread_events_lock.release()


def store_thread_events_full_body(
    events_path: Path, body: bytes, entry_token: _ThreadEventsCacheEntry, offset_token: int) -> None:
  key = str(events_path)
  with _thread_events_lock:
    entry = _thread_events_cache.get(key)
    if entry is not entry_token or entry.offset != offset_token:
      return
    try:
      size = events_path.stat().st_size
    except OSError:
      return
    if size == entry.offset:
      entry.full_body = body


def _append_worker_events(
    raw_events: Iterable[dict], events: list[WorkerEvent], tool_id_to_name: dict[str, str]) -> None:
  for data in raw_events:
    event_timestamp = data.get("timestamp") or datetime.now(UTC)
    event_type = data.get("type", "")
    if event_type == ET.SESSION_ATTACHED:
      continue
    if event_type == ET.ASSISTANT and isinstance(data.get("message"), dict):
      text = extract_text_from_message(data["message"])
      if text:
        events.append(WorkerEvent(type=ET.ASSISTANT, content=text, timestamp=event_timestamp))
      for block in data["message"].get("content", []):
        if isinstance(block, dict) and block.get("type") == "tool_use":
          tool_id_to_name[block["id"]] = block["name"]
          events.append(
              WorkerEvent(
                  type=ET.TOOL_USE,
                  tool_name=block["name"],
                  input=tool_preview({"name": block["name"], "input": block.get("input", {})})["input"],
                  timestamp=event_timestamp,
              ))
    elif event_type == ET.USER and isinstance(data.get("message"), dict):
      for block in data["message"].get("content", []):
        if block.get("type") == "tool_result":
          tool_use_id = block.get("tool_use_id", "")
          name = tool_id_to_name.get(tool_use_id, "")
          result_text = extract_tool_result_text(block)
          truncated = len(result_text) > TOOL_PREVIEW_CHARS
          events.append(
              WorkerEvent(
                  type=ET.TOOL_RESULT,
                  tool_name=name,
                  content=result_text[:TOOL_PREVIEW_CHARS] if truncated else result_text,
                  output_truncated=True if truncated else None,
                  timestamp=event_timestamp,
              ))
    else:
      try:
        row = WorkerEvent(**{k: v for k, v in data.items() if k in WorkerEvent.model_fields})
      except Exception as e:
        log.debug("event_parse_failed", error=str(e))
        row = WorkerEvent(type="raw", content=str(data))
      if row.type == ET.TOOL_RESULT and row.content is not None and len(row.content) > TOOL_PREVIEW_CHARS:
        row.content = row.content[:TOOL_PREVIEW_CHARS]
        row.output_truncated = True
      events.append(row)


@router.get("/{session_id}/threads/{thread_id}/events", response_model=list[WorkerEvent])
async def get_thread_events(
    request: Request,
    session_id: str,
    thread_id: str,
    after: int | None = Query(default=None, ge=0),
) -> Response:
  """Return events for a Run addressed through its retained thread-id alias."""
  target = await _resolve_run(session_id, thread_id)
  if target is None:
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
  run_store = await get_run_store()
  events_path = run_store.run_dir(target[0], target[1]) / RUN_EVENTS_NAME
  if after is None:
    body = stored_thread_events_full_body(events_path)
    if body is None:
      snap = _thread_events_snapshot(events_path)
      if snap is not None:
        events, entry_token, offset_token = snap
        body = fast_json_bytes([e.model_dump(mode="json") for e in events])
        store_thread_events_full_body(events_path, body, entry_token, offset_token)
      else:
        events = await asyncio.to_thread(read_thread_worker_events, events_path)
        body = fast_json_bytes([e.model_dump(mode="json") for e in events])
    return await gzip_body_response(request, body, {}, _RUN_EVENTS_GZIP_MEMO)
  events = read_thread_worker_events_memo_hit(events_path)
  if events is None:
    events = await asyncio.to_thread(read_thread_worker_events, events_path)
  reset = after > len(events)
  start = 0 if reset else after
  return FastJsonResponse({
      "events": [e.model_dump(mode="json") for e in events[start:]],
      "total": len(events),
      "reset": reset,
  })


@router.post("/{session_id}/threads/{thread_id}/cancel")
async def cancel_thread(
    session_id: str,
    thread_id: str,
    run_store=Depends(get_run_store),
    task_mgr=Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> dict:
  """Stop the task Run addressed through its retained thread-id alias."""
  alias = task_mgr.aliases.resolve_thread(session_id, thread_id)
  if alias is None:
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
  target_session, target_run = alias["session_id"], alias["run_id"]
  from src.runtime.api.sessions import _require_own_run_scope
  _require_own_run_scope(caller, target_session, target_run)
  try:
    result = await run_store.request_stop(target_session, target_run, f"thread-cancel:{thread_id}")
  except (RunNotFoundError, RunIdentityConflictError) as e:
    from src.runtime.api.sessions import _task_http_error
    raise _task_http_error(e) from e
  return {"run_id": result.run_id, "stop_requested": result.stop_requested, "outcome": result.outcome}
