"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio
import re

import structlog
from fastapi import APIRouter, Depends, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.agents.master_cc import cancel_master, run_message
from src.api.deps import get_session_manager
from src.core.autonamer import maybe_auto_name
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SendMessageRequest
from src.core.sessions import SessionManager

log = structlog.get_logger()

router = APIRouter()

_DEFAULT_NAME_RE = re.compile(r"^Session \d+$")


class SwitchBackendRequest(BaseModel):
  backend: str


@router.post("/{session_id}/upload")
async def upload_file(
  session_id: str,
  file: UploadFile = File(...),
  session_mgr: SessionManager = Depends(get_session_manager),
  cfg: CharlieBotConfig = Depends(get_config),
):
  """Upload a file to the session's uploads directory. Returns {filename, path, size}."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  uploads_dir = cfg.sessions_dir / session_id / "uploads"
  uploads_dir.mkdir(parents=True, exist_ok=True)

  dest = uploads_dir / (file.filename or "upload")
  size = 0
  try:
    with dest.open("wb") as out:
      while True:
        chunk = await file.read(1024 * 1024)  # 1 MB chunks
        if not chunk:
          break
        out.write(chunk)
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
  session_mgr: SessionManager = Depends(get_session_manager),
  cfg: CharlieBotConfig = Depends(get_config),
):
  """Send a message to the master CC agent. Returns 202; response streams via WebSocket."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  # Build content, appending any uploaded file paths.
  content = req.content
  if req.uploaded_files:
    content += "\n\n[Attached files]\n" + "\n".join(f"- {p}" for p in req.uploaded_files)

  # Fire-and-forget: spawn master CC in a background task
  asyncio.create_task(run_and_finalize(cfg, meta, content, session_mgr))

  return JSONResponse(status_code=202, content={"status": "accepted"})


@router.post("/{session_id}/cancel")
async def cancel_master_agent(
  session_id: str,
  session_mgr: SessionManager = Depends(get_session_manager),
):
  """Send SIGTERM to the running master CC agent for this session."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  found = await cancel_master(session_id)
  if not found:
    raise HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}


@router.patch("/{session_id}/backend")
async def switch_backend(
  session_id: str,
  req: SwitchBackendRequest,
  session_mgr: SessionManager = Depends(get_session_manager),
  cfg: CharlieBotConfig = Depends(get_config),
):
  """Switch the active backend for a session."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  option = next((o for o in cfg.backend_options if o.id == req.backend), None)
  if option is None:
    raise HTTPException(status_code=400, detail=f"Unknown backend id: {req.backend!r}")

  if option.type == "cc-kimi" and not cfg.moonshot_api_key:
    raise HTTPException(status_code=400, detail="moonshot_api_key not set in config")

  # Resuming across backends is invalid — clear the CC session ID.
  meta.cc_session_id = None
  meta.backend = req.backend
  await session_mgr.save_metadata(meta)

  log.info("backend_switched", session=session_id, backend=req.backend)
  return {"ok": True, "backend": req.backend}


async def run_and_finalize(
  cfg: CharlieBotConfig,
  meta,
  content: str,
  session_mgr: SessionManager,
  *,
  is_voice: bool = False,
) -> None:
  """Run master CC, persist cc_session_id, and auto-name the session."""
  backend_id = meta.backend
  backend_option = next((o for o in cfg.backend_options if o.id == backend_id), None)
  try:
    cc_session_id = await run_message(
      cfg, meta, content, session_mgr.save_chat_event,
      session_mgr.save_metadata, mark_unread=session_mgr.mark_unread,
      backend_option=backend_option,
      is_voice=is_voice,
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
    log.error("master_cc_run_failed", session=meta.id, error=str(e))


async def _auto_name(
  cfg: CharlieBotConfig,
  session_meta,
  user_message: str,
  session_mgr: SessionManager,
) -> None:
  """Extract assistant response from saved events and auto-name the session."""
  events = session_mgr.load_chat_events_sync(session_meta.id)
  assistant_text = ""
  for ev in events:
    if ev.get("type") == "assistant":
      for block in (ev.get("message") or {}).get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
          assistant_text += block.get("text", "")

  if not assistant_text:
    return

  await maybe_auto_name(cfg, session_meta, user_message, assistant_text, session_mgr)
