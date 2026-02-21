"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import asyncio
import re

import structlog
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import JSONResponse

from src.agents.master_cc import run_message
from src.api.deps import get_session_manager
from src.core.autonamer import maybe_auto_name
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SendMessageRequest
from src.core.sessions import SessionManager

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

  # Fire-and-forget: spawn master CC in a background task
  async def _run():
    try:
      cc_session_id = await run_message(
        cfg, meta, req.content, session_mgr.save_chat_event,
        session_mgr.save_metadata, mark_unread=session_mgr.mark_unread,
      )
      # Persist CC session ID if newly assigned
      if cc_session_id and cc_session_id != meta.cc_session_id:
        meta.cc_session_id = cc_session_id
        await session_mgr.save_metadata(meta)

      # Auto-name session after first turn if still using default name
      if _DEFAULT_NAME_RE.match(meta.name):
        asyncio.create_task(_auto_name(cfg, meta, req.content, session_mgr))
    except Exception as e:
      log.error("master_cc_run_failed", session=session_id, error=str(e))

  asyncio.create_task(_run())

  return JSONResponse(status_code=202, content={"status": "accepted"})


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
