"""Thread management API routes."""

import asyncio
import os
import shlex
from dataclasses import dataclass
from datetime import datetime, timezone

from src.core.process import kill_process_group

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.agents.backends.pty_common import _TMUX_SOCKET, tmux_session_exists, tmux_session_name
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.message_utils import extract_text_from_message, extract_tool_result_text
from src.core.config import CharlieBotConfig, get_config
from src.core.models import (
    ThreadMetadata,
    ThreadStatus,
    WorkerEvent,
)
from src.core.ndjson import parse_ndjson_file
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


def _tmux_attach_command(session_id: str) -> str:
  return shlex.join(["tmux", "-L", _TMUX_SOCKET, "attach", "-t", tmux_session_name(session_id)])


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
      return _tmux_attach_command(tmux_id)
    if not thread.worktree_path or not thread.claude_session_id:
      return None
    return f"cd {shlex.quote(thread.worktree_path)} && claude --resume {shlex.quote(thread.claude_session_id)}"
  if dispatch.type == "cc-kimi":
    return None
  if dispatch.type == "cc-deepseek-sglang":
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
  if dispatch.type == "cc-deepseek-sglang":
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


@router.get("/{session_id}/threads/{thread_id}/events", response_model=list[WorkerEvent])
async def get_thread_events(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Return historical Worker events from the on-disk events.jsonl log."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  raw_events = await asyncio.to_thread(parse_ndjson_file, events_path)
  events: list[WorkerEvent] = []
  tool_id_to_name: dict[str, str] = {}
  for data in raw_events:
    event_timestamp = data.get("timestamp") or datetime.now(timezone.utc)
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
    kill_process_group(thread.pid)

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.CANCELLED)
  return {"ok": True}
