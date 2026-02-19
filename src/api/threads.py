"""Thread management API routes including plan approval."""

import json
import os
import signal

from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_thread_manager, get_queue_manager
from src.core.models import (
  PlanApprovalRequest,
  Priority,
  Task,
  ThreadMetadata,
  ThreadStatus,
  WorkerEvent,
)
from src.core.queue import QueueManager
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
      except Exception:
        events.append(WorkerEvent(type="raw", content=line))
  return events


@router.post("/{session_id}/threads/{thread_id}/approve-plan")
async def approve_plan(
  session_id: str,
  thread_id: str,
  req: PlanApprovalRequest,
  thread_mgr: ThreadManager = Depends(get_thread_manager),
  queue_mgr: QueueManager = Depends(get_queue_manager),
):
  """User approved a plan. Push approved steps as P0/P1 tasks into the queue."""
  thread = await thread_mgr.get_thread(session_id, thread_id)
  if not thread:
    raise HTTPException(status_code=404, detail="Thread not found")
  if thread.status != ThreadStatus.AWAITING_APPROVAL:
    raise HTTPException(status_code=400, detail="Thread is not awaiting plan approval")

  steps = req.edited_steps or req.approved_steps
  queued_tasks = []
  for i, step in enumerate(steps):
    task = Task(
      priority=Priority.P0 if i == 0 else Priority.P1,
      description=step,
      context={"plan_thread_id": thread_id, "step_index": i},
    )
    await queue_mgr.push(task)
    queued_tasks.append(task.id)

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.COMPLETED)
  return {"ok": True, "queued_tasks": queued_tasks}


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
      pass  # Process already finished

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.CANCELLED)
  return {"ok": True}
