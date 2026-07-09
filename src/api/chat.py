"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio
from pathlib import Path

import aiofiles
import structlog
from fastapi import APIRouter, Depends, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse
from src.agents.master_cc import cancel_master, run_message
from src.api.deps import get_session_manager, require_session
from src.api.message_utils import (
    build_agent_input_content,
    build_user_event,
    extract_text_from_message,
    serialize_uploaded_files,
)
from src.core import event_types as ET
from src.core.autonamer import is_default_session_name, maybe_auto_name
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SendMessageRequest, SessionMetadata
from src.core.sessions import SessionManager
from src.core.slash_commands import dispatch_slash_command
from src.core.tasks import create_logged_task

log = structlog.get_logger()

router = APIRouter()


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

  dest = uploads_dir / Path(file.filename or "upload").name
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
  backend_option = cfg.get_backend_option(meta.backend) if meta.backend else None
  if backend_option is not None and backend_option.type == "tui-cli":
    raise HTTPException(status_code=400, detail="Chat input is not supported for tui-cli sessions; use the terminal.")

  uploaded_files = serialize_uploaded_files(req.uploaded_files)
  content = build_agent_input_content(req.content, uploaded_files)

  is_slash = req.content.startswith('/')
  log.info(
      "send_message",
      session=session_id,
      content_chars=len(content),
      uploaded_files_count=len(uploaded_files),
      is_slash=is_slash,
  )

  # Slash command interception
  if is_slash:
    space_idx = req.content.find(' ')
    name = req.content[1:space_idx] if space_idx != -1 else req.content[1:]
    args = req.content[space_idx + 1:].strip() if space_idx != -1 else ''

    dispatch = await dispatch_slash_command(name, args, session_dir=str(cfg.sessions_dir / session_id))

    if dispatch.kind != 'not_found':
      await session_mgr.persist_and_broadcast(session_id, build_user_event(req.content, uploaded_files))

      if dispatch.kind == 'prompt':
        create_logged_task(
            run_and_finalize(
                cfg,
                meta,
                build_agent_input_content(dispatch.substituted_prompt, uploaded_files),
                session_mgr,
                extra_claude_flags=dispatch.claude_code_flags,
                skip_user_event=True,
                display_content=req.content,
                uploaded_files=uploaded_files))
        return JSONResponse(status_code=202, content={"status": "accepted"})

      elif dispatch.kind == 'error':
        error_text = dispatch.error or f'Failed to dispatch /{name}'
        asst_event = {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": error_text}]}}
        await session_mgr.persist_and_broadcast(session_id, asst_event)
        done_event = {"type": ET.MASTER_DONE, "exit_code": 1, "still_thinking": False}
        await session_mgr.persist_and_broadcast(session_id, done_event)
        return JSONResponse(status_code=202, content={"status": "accepted"})

      elif dispatch.kind == 'shell_result':
        result = dispatch.shell_result
        out = result['stderr'] if result['exit_code'] != 0 and result['stderr'] else (
            result['stdout'] or result['stderr'] or '(no output)')
        md_out = '```\n' + out + '\n```'
        asst_event = {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": md_out}]}}
        await session_mgr.persist_and_broadcast(session_id, asst_event)
        done_event = {"type": ET.MASTER_DONE, "exit_code": 0, "still_thinking": False}
        await session_mgr.persist_and_broadcast(session_id, done_event)
        return JSONResponse(status_code=202, content={"status": "accepted"})

    # Unknown /xxx — fall through to normal run_and_finalize (e.g. /compact)

  # Fire-and-forget: spawn master CC in a background task
  create_logged_task(
      run_and_finalize(cfg, meta, content, session_mgr, display_content=req.content, uploaded_files=uploaded_files))

  return JSONResponse(status_code=202, content={"status": "accepted"})


@router.post("/{session_id}/cancel")
async def cancel_master_agent(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Send SIGTERM to the running master CC agent for this session."""
  found = await cancel_master(session_id)
  if not found:
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.ASSISTANT_ERROR,
            "content": "No active master agent to cancel.",
        })
    raise HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}


async def run_and_finalize(
    cfg: CharlieBotConfig,
    meta,
    content: str,
    session_mgr: SessionManager,
    *,
    extra_claude_flags: list[str] | None = None,
    skip_user_event: bool = False,
    display_content: str | None = None,
    uploaded_files: list[dict] | None = None,
) -> None:
  """Run master CC, persist cc_session_id, and auto-name the session."""
  log.info("run_and_finalize_start", session=meta.id, backend=meta.backend)
  backend_id = meta.backend
  backend_option = cfg.get_backend_option(backend_id)
  if backend_option is None and backend_id.startswith("codex"):
    backend_option = next((o for o in cfg.backend_options if o.type == "codex"), None)
  try:
    cc_session_id = await run_message(
        cfg,
        meta,
        content,
        session_mgr.callbacks(),
        skip_user_event=skip_user_event,
        backend_option=backend_option,
        extra_claude_flags=extra_claude_flags,
        display_content=display_content,
        uploaded_files=uploaded_files,
    )
    # Persist CC session ID if newly assigned.
    # Re-read fresh metadata from disk to avoid overwriting has_unread
    # (or other fields) that mark_unread() set during run_message().
    if cc_session_id and cc_session_id != meta.cc_session_id:
      await session_mgr.persist_cc_session_id(meta.id, cc_session_id)
      meta.cc_session_id = cc_session_id

    # Auto-name session after first turn if still using default name
    if is_default_session_name(meta.name):
      create_logged_task(_auto_name(cfg, meta, display_content or content, session_mgr))
  except Exception as e:
    log.exception("master_cc_run_failed", session=meta.id)
    # run_message() should handle and emit failures, but keep this as a
    # last-resort guard so the UI never gets stuck in "Thinking...".
    error_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {e}"}
    done_event = {"type": ET.MASTER_DONE, "exit_code": 1, "still_thinking": False}
    await session_mgr.persist_and_broadcast(meta.id, error_event)
    await session_mgr.persist_and_broadcast(meta.id, done_event)


async def _auto_name(
    cfg: CharlieBotConfig,
    session_meta,
    user_message: str,
    session_mgr: SessionManager,
) -> None:
  """Extract assistant response from saved events and auto-name/group the session."""
  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_meta.id)
  assistant_text = ""
  for ev in events:
    if ev.get("type") == ET.ASSISTANT:
      assistant_text += extract_text_from_message(ev.get("message"))

  if not assistant_text:
    return

  # Collect existing group names so the LLM can reuse them
  all_sessions = await session_mgr.list_sessions()
  existing_groups = sorted({s.group for s in all_sessions if s.group})

  await maybe_auto_name(cfg, session_meta, user_message, assistant_text, session_mgr, existing_groups)
