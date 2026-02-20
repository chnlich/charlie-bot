"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import JSONResponse

from src.agents.master_cc import run_message
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, get_config
from src.core.models import ConversationHistory, SendMessageRequest
from src.core.sessions import SessionManager

log = structlog.get_logger()

router = APIRouter()


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

  asyncio.create_task(_run())

  return JSONResponse(status_code=202, content={"status": "accepted"})


@router.get("/{session_id}/history", response_model=ConversationHistory)
async def get_history(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return await session_mgr.load_history(session_id)
