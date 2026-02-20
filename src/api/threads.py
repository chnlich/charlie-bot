"""Thread management API routes."""

import json
import os
import signal

import structlog
from fastapi import APIRouter, Depends, HTTPException

log = structlog.get_logger()

from src.api.deps import get_thread_manager
from src.core.models import (
  ThreadMetadata,
  ThreadStatus,
  WorkerEvent,
)
from src.core.threads import ThreadManager

router = APIRouter()


@router.get("/{session_id}/threads/{thread_id}", response_model=ThreadMetadata)
async def get_thread(
  session_id: str,
  thread_id: str,
  thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  meta = await thread_mgr.get_thread(session_id, thread_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Thread not found")
  return meta


@router.get("/{session_id}/threads/{thread_id}/events", response_model=list[WorkerEvent])
async def get_thread_events(
  session_id: str,
  thread_id: str,
  thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Return historical Worker events from the on-disk events.jsonl log."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  if not events_path.exists():
    return []

  events: list[WorkerEvent] = []
  with open(events_path, "r", encoding="utf-8") as f:
    for line in f:
      line = line.strip()
      if not line:
        continue
      try:
        data = json.loads(line)
        events.append(WorkerEvent(**{k: v for k, v in data.items() if k in WorkerEvent.model_fields}))
      except Exception as e:
        log.debug("event_parse_failed", error=str(e), line=line[:200])
        events.append(WorkerEvent(type="raw", content=line))
  return events


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
    try:
      os.kill(thread.pid, signal.SIGTERM)
    except ProcessLookupError:
      log.debug("cancel_pid_gone", pid=thread.pid, thread=thread_id)

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.CANCELLED)
  return {"ok": True}
