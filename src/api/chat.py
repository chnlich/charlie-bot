"""Chat API routes — Master Agent interaction with SSE streaming."""

import asyncio
import json
import traceback
from typing import AsyncGenerator

import structlog
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import StreamingResponse

from src.agents.master_agent import MasterAgent
from src.api.deps import get_dispatcher, get_master_agent, get_session_manager, get_thread_manager
from src.core.dispatcher import SessionDispatcher
from src.core.models import (
  ChatMessage,
  ConversationHistory,
  MessageRole,
  Priority,
  SendMessageRequest,
  Task,
  ThreadStatus,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()

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
    try:
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

      if action.get("action") == "execute":
        # Master handles small tasks directly by running a shell command
        command = action.get("command", "")
        cwd = meta.repo_path or "."
        try:
          proc = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
            cwd=cwd,
          )
          stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)
          output = stdout.decode("utf-8", errors="replace") if stdout else ""
          # Cap output to avoid huge payloads
          if len(output) > 50_000:
            output = output[:50_000] + "\n... (truncated)"
          result_text = f"```\n$ {command}\n{output}```"
        except asyncio.TimeoutError:
          result_text = f"```\n$ {command}\n(command timed out after 30s)\n```"
        except Exception as e:
          log.warning("execute_command_failed", command=command, error=str(e))
          result_text = f"```\n$ {command}\nError: {e}\n```"

        # Stream the command output as a chunk
        exec_event = json.dumps({"type": "chunk", "content": "\n\n" + result_text})
        yield f"data: {exec_event}\n\n"

        # Append the command output to conversation history
        exec_msg = ChatMessage(role=MessageRole.ASSISTANT, content=result_text)
        history.messages.append(exec_msg)

      elif action.get("action") == "delegate":
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

    except Exception as e:
      log.error("sse_stream_error", session=session_id, error=str(e), tb=traceback.format_exc())
      error_event = json.dumps({"type": "chunk", "content": f"\n\n**Error:** {e}"})
      yield f"data: {error_event}\n\n"

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
