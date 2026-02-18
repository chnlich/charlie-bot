"""Chat API routes — Master Agent interaction with SSE streaming."""

import json
from typing import AsyncGenerator

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import StreamingResponse

from src.agents.master_agent import MasterAgent
from src.api.deps import get_dispatcher, get_master_agent, get_session_manager, get_thread_manager
from src.core.dispatcher import SessionDispatcher
from src.core.models import (
  ConversationHistory,
  Priority,
  SendMessageRequest,
  Task,
  ThreadStatus,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

router = APIRouter()


@router.post("/{session_id}/message")
async def send_message(
  session_id: str,
  req: SendMessageRequest,
  master: MasterAgent = Depends(get_master_agent),
  session_mgr: SessionManager = Depends(get_session_manager),
  thread_mgr: ThreadManager = Depends(get_thread_manager),
  dispatcher: SessionDispatcher = Depends(get_dispatcher),
):
  """Send a message to the Master Agent. Returns SSE stream."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  history = await session_mgr.load_history(session_id)

  async def event_stream() -> AsyncGenerator[str, None]:
    full_response = ""

    # Stream text chunks from Master Agent
    async for chunk in master.chat_streaming(history, req.content):
      full_response += chunk
      event = json.dumps({"type": "chunk", "content": chunk})
      yield f"data: {event}\n\n"

    # Parse the complete response to determine action
    action = master._parse_response(full_response)

    # Persist any new memory facts/preferences the model identified
    memory_note = action.get("memory_update", "").strip()
    if memory_note:
      await master._memory.append_memory(memory_note)

    if action.get("action") == "delegate":
      # Create task and thread inline so the thread is visible immediately
      priority_map = {"P0": Priority.P0, "P1": Priority.P1, "P2": Priority.P2}
      priority = priority_map.get(action.get("priority", "P1"), Priority.P1)
      task = Task(
        priority=priority,
        description=action.get("description", req.content),
        is_plan_mode=action.get("plan_mode", False),
      )

      # Create thread NOW (before SSE completes) so it shows in the UI
      thread = await thread_mgr.create_thread(meta, task)
      task.thread_id = thread.id

      # Hand off to dispatcher to run the worker in the background
      await dispatcher.enqueue(task)

      delegation_event = json.dumps({
        "type": "task_delegated",
        "task_id": task.id,
        "thread_id": thread.id,
        "priority": task.priority.value,
        "description": task.description,
        "plan_mode": task.is_plan_mode,
      })
      yield f"data: {delegation_event}\n\n"

    # Save updated history
    await session_mgr.save_history(history)

    yield f"data: {json.dumps({'type': 'done'})}\n\n"

  return StreamingResponse(
    event_stream(),
    media_type="text/event-stream",
    headers={
      "Cache-Control": "no-cache",
      "X-Accel-Buffering": "no",
      "Connection": "keep-alive",
    },
  )


@router.get("/{session_id}/history", response_model=ConversationHistory)
async def get_history(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return await session_mgr.load_history(session_id)
