"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio
import re
from datetime import datetime, timezone

import aiofiles
import structlog
from fastapi import APIRouter, Depends, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.agents.master_cc import cancel_master, run_message
from src.api.deps import get_session_manager, require_session
from src.core.autonamer import maybe_auto_name
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SendMessageRequest, SessionMetadata
from src.core.sessions import SessionManager
from src.core.slash_commands import execute_shell_command, load_slash_commands
from src.core.streaming import streaming_manager

log = structlog.get_logger()

router = APIRouter()

_DEFAULT_NAME_RE = re.compile(r"^Session \d+$")


@router.post("/{session_id}/upload")
async def upload_file(
    session_id: str,
    file: UploadFile = File(...),
    _meta: SessionMetadata = Depends(require_session),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Upload a file to the session's uploads directory. Returns {filename, path, size}."""
  uploads_dir = cfg.sessions_dir / session_id / "uploads"
  uploads_dir.mkdir(parents=True, exist_ok=True)

  dest = uploads_dir / (file.filename or "upload")
  size = 0
  try:
    async with aiofiles.open(dest, "wb") as out:
      while True:
        chunk = await file.read(1024 * 1024)  # 1 MB chunks
        if not chunk:
          break
        await out.write(chunk)
        size += len(chunk)
  except Exception as e:
    log.warning("file_upload_failed", session=session_id, filename=file.filename, error=str(e))
    raise HTTPException(status_code=500, detail="Failed to save uploaded file") from e

  log.info("file_uploaded", session=session_id, filename=file.filename, size=size)
  return {"filename": file.filename, "path": str(dest.resolve()), "size": size}


@router.post("/{session_id}/message")
async def send_message(
    session_id: str,
    req: SendMessageRequest,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Send a message to the master CC agent. Returns 202; response streams via WebSocket."""
  # Build content, appending any uploaded file paths.
  content = req.content
  if req.uploaded_files:
    content += "\n\n[Attached files]\n" + "\n".join(f"- {p}" for p in req.uploaded_files)

  # Slash command interception
  if content.startswith('/'):
    space_idx = content.find(' ')
    name = content[1:space_idx] if space_idx != -1 else content[1:]
    args = content[space_idx + 1:].strip() if space_idx != -1 else ''

    commands = {c.name: c for c in await asyncio.to_thread(load_slash_commands)}
    cmd = commands.get(name)

    if cmd is not None:
      channel = f"session:{session_id}"
      user_event = {
          "type": "user",
          "content": content,
          "timestamp": datetime.now(timezone.utc).isoformat(),
          "is_voice": False,
      }
      await session_mgr.save_chat_event(session_id, user_event)
      await streaming_manager.broadcast(channel, user_event)

      if cmd.scope == 'prompt':
        substituted = cmd.prompt.replace('{args}', args) if cmd.prompt else args
        asyncio.create_task(
            run_and_finalize(
                cfg,
                meta,
                substituted,
                session_mgr,
                extra_claude_flags=cmd.claude_code_flags or None,
                skip_user_event=True))
        return JSONResponse(status_code=202, content={"status": "accepted"})

      elif cmd.scope == 'shell' and cmd.command:
        session_dir = str(cfg.sessions_dir / session_id)
        result = await execute_shell_command(
            cmd_template=cmd.command, args=args, session_dir=session_dir, timeout=cmd.timeout, cwd=cmd.cwd)
        out = result['stderr'] if result['exit_code'] != 0 and result['stderr'] else (
            result['stdout'] or result['stderr'] or '(no output)')
        md_out = '```\n' + out + '\n```'
        asst_event = {"type": "assistant", "message": {"content": [{"type": "text", "text": md_out}]}}
        await session_mgr.save_chat_event(session_id, asst_event)
        await streaming_manager.broadcast(channel, asst_event)
        done_event = {"type": "master_done", "exit_code": 0, "still_thinking": False}
        await session_mgr.save_chat_event(session_id, done_event)
        await streaming_manager.broadcast(channel, done_event)
        return JSONResponse(status_code=202, content={"status": "accepted"})

    # Unknown /xxx — fall through to normal run_and_finalize (e.g. /compact)

  # Fire-and-forget: spawn master CC in a background task
  asyncio.create_task(run_and_finalize(cfg, meta, content, session_mgr))

  return JSONResponse(status_code=202, content={"status": "accepted"})


@router.post("/{session_id}/cancel")
async def cancel_master_agent(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
):
  """Send SIGTERM to the running master CC agent for this session."""
  found = await cancel_master(session_id)
  if not found:
    raise HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}


async def run_and_finalize(
    cfg: CharlieBotConfig,
    meta,
    content: str,
    session_mgr: SessionManager,
    *,
    is_voice: bool = False,
    extra_claude_flags: list[str] | None = None,
    skip_user_event: bool = False,
) -> None:
  """Run master CC, persist cc_session_id, and auto-name the session."""
  backend_id = meta.backend
  backend_option = next((o for o in cfg.backend_options if o.id == backend_id), None)
  try:
    cc_session_id = await run_message(
        cfg,
        meta,
        content,
        session_mgr.save_chat_event,
        session_mgr.save_metadata,
        mark_unread=session_mgr.mark_unread,
        skip_user_event=skip_user_event,
        backend_option=backend_option,
        is_voice=is_voice,
        extra_claude_flags=extra_claude_flags,
    )
    # Persist CC session ID if newly assigned.
    # Re-read fresh metadata from disk to avoid overwriting has_unread
    # (or other fields) that mark_unread() set during run_message().
    if cc_session_id and cc_session_id != meta.cc_session_id:
      fresh = await session_mgr.get_session(meta.id)
      if fresh:
        fresh.cc_session_id = cc_session_id
        await session_mgr.save_metadata(fresh)
      meta.cc_session_id = cc_session_id

    # Auto-name session after first turn if still using default name
    if _DEFAULT_NAME_RE.match(meta.name):
      asyncio.create_task(_auto_name(cfg, meta, content, session_mgr))
  except Exception as e:
    log.exception("master_cc_run_failed", session=meta.id)


async def _auto_name(
    cfg: CharlieBotConfig,
    session_meta,
    user_message: str,
    session_mgr: SessionManager,
) -> None:
  """Extract assistant response from saved events and auto-name the session."""
  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_meta.id)
  assistant_text = ""
  for ev in events:
    if ev.get("type") == "assistant":
      for block in (ev.get("message") or {}).get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
          assistant_text += block.get("text", "")

  if not assistant_text:
    return

  await maybe_auto_name(cfg, session_meta, user_message, assistant_text, session_mgr)
