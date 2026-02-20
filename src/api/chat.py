"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio
import re

import structlog
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import JSONResponse

from src.agents.gemini_provider import GeminiProvider
from src.agents.master_cc import run_message
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SendMessageRequest
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager

log = structlog.get_logger()

router = APIRouter()

_DEFAULT_NAME_RE = re.compile(r"^Session \d+$")


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

  is_first_run = meta.cc_session_id is None

  # Fire-and-forget: spawn master CC in a background task
  async def _run():
    try:
      cc_session_id = await run_message(cfg, meta, req.content, session_mgr.save_chat_event, session_mgr.save_metadata)
      # Persist CC session ID if newly assigned
      if cc_session_id and cc_session_id != meta.cc_session_id:
        meta.cc_session_id = cc_session_id
        await session_mgr.save_metadata(meta)
    except Exception as e:
      log.error("master_cc_run_failed", session=session_id, error=str(e))

    # Auto-rename session after first run if it still has a default name
    if is_first_run and _DEFAULT_NAME_RE.match(meta.name):
      await _auto_rename_session(cfg, session_mgr, meta, req.content)

  asyncio.create_task(_run())

  return JSONResponse(status_code=202, content={"status": "accepted"})


async def _auto_rename_session(
  cfg: CharlieBotConfig,
  session_mgr: SessionManager,
  meta,
  user_message: str,
) -> None:
  """Generate a descriptive session name from the first user message."""
  try:
    provider = GeminiProvider(cfg.gemini_api_key, cfg.gemini_model)
    new_name = await provider.generate_session_name(user_message)
    if not new_name:
      return
    await session_mgr.rename_session(meta.id, new_name)
    channel = f"session:{meta.id}"
    await streaming_manager.broadcast(channel, {"type": "session_renamed", "name": new_name})
    log.info("session_auto_renamed", session_id=meta.id, new_name=new_name)
  except Exception as e:
    log.warning("session_auto_rename_failed", session_id=meta.id, error=str(e))
