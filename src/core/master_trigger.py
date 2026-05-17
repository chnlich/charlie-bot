"""Master-agent triggering subsystem — wake the master CC to process results."""

import traceback
from datetime import datetime, timedelta, timezone
from typing import Optional
from zoneinfo import ZoneInfo

import structlog

from src.agents.master_cc import run_message
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager

log = structlog.get_logger()


async def run_message_with_resume_recovery(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    summary: str,
    session_mgr: SessionManager,
) -> Optional[str]:
  """Call run_message, retrying once with cc_session_id cleared on stale-resume errors."""
  backend_id = session_meta.backend
  backend_option = cfg.get_backend_option(backend_id)
  if backend_option is None and backend_id.startswith("codex"):
    backend_option = next((o for o in cfg.backend_options if o.type == "codex"), None)
  try:
    return await run_message(
        cfg,
        session_meta,
        summary,
        session_mgr.callbacks(),
        skip_user_event=True,
        auto_trigger=True,
        backend_option=backend_option,
    )
  except Exception as e:
    if not is_resume_not_found_error(e):
      raise

    stale_cc_session_id = session_meta.cc_session_id
    log.warning(
        "trigger_master_invalid_resume_detected",
        session=session_meta.id,
        cc_session_id=stale_cc_session_id,
        error=str(e),
    )

    retry_session_meta = session_meta.model_copy(deep=True)
    retry_session_meta.cc_session_id = None
    log.info(
        "trigger_master_retry_without_resume",
        session=session_meta.id,
        stale_cc_session_id=stale_cc_session_id,
    )
    new_cc_session_id = await run_message(
        cfg,
        retry_session_meta,
        summary,
        session_mgr.callbacks(),
        skip_user_event=True,
        auto_trigger=True,
        backend_option=backend_option,
    )
    log.info(
        "trigger_master_resume_recovery_succeeded",
        session=session_meta.id,
        stale_cc_session_id=stale_cc_session_id,
        recovered_cc_session_id=new_cc_session_id,
    )
    return new_cc_session_id


async def trigger_master(
    session_id: str,
    summary: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> None:
  """Best-effort trigger of the master agent to process a worker result."""
  try:
    session_meta = await session_mgr.get_session(session_id)
    if not session_meta:
      log.error("trigger_master_session_not_found", session=session_id)
      return

    if (session_meta.scheduled_task and session_meta.cc_session_id and session_meta.cc_session_started_at):
      pt = ZoneInfo('America/Los_Angeles')
      now_pt = datetime.now(pt)
      # Most recent Saturday 1:00 AM PT
      days_since_sat = (now_pt.weekday() - 5) % 7
      last_sat_1am_pt = now_pt.replace(hour=1, minute=0, second=0, microsecond=0) - timedelta(days=days_since_sat)
      last_sat_1am_utc = last_sat_1am_pt.astimezone(timezone.utc)
      if session_meta.cc_session_started_at < last_sat_1am_utc < datetime.now(timezone.utc):
        log.info('scheduled_cc_session_expired', session=session_id, started_at=str(session_meta.cc_session_started_at))
        session_meta.cc_session_id = None
        session_meta.cc_session_started_at = None
        await session_mgr.save_metadata(session_meta)
        try:
          result = await session_mgr.recycle_scheduled_session(session_id, last_sat_1am_utc)
          log.info('scheduled_session_recycled', session=session_id, **result)
        except Exception:
          log.exception('scheduled_session_recycle_failed', session=session_id)

    master_summary = summary
    if not session_meta.cc_session_id and session_meta.scheduled_task:
      master_summary = (
          f"[Auto-triggered scheduled task result for '{session_meta.scheduled_task}']\n"
          "Review the worker/reviewer results below. Check: was the branch merged? "
          "Are there errors? Summarize the outcome.\n\n"
          f"{summary}")

    new_cc_session_id = await run_message_with_resume_recovery(cfg, session_meta, master_summary, session_mgr)

    if new_cc_session_id and new_cc_session_id != session_meta.cc_session_id:
      await session_mgr.persist_cc_session_id(session_id, new_cc_session_id)
      # persist_cc_session_id only saves cc_session_id; also persist cc_session_started_at.
      fresh = await session_mgr.get_session(session_id)
      if fresh:
        fresh.cc_session_started_at = datetime.now(timezone.utc)
        await session_mgr.save_metadata(fresh)
  except Exception as e:
    log.error("trigger_master_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    try:
      error_payload = {
          'type': ET.ERROR,
          'message': f'Failed to notify master agent: {e}',
          'source': 'trigger_master',
      }
      await session_mgr.persist_and_broadcast(session_id, error_payload)
    except Exception:
      pass  # Last resort — nothing more we can do


def is_resume_not_found_error(error: Exception) -> bool:
  """Return True only for stale resume errors where session/conversation is missing."""
  message = str(error).lower()
  if "resume" not in message:
    return False

  has_conversation_not_found = "conversation" in message and "not found" in message
  has_session_not_found = "session" in message and "not found" in message
  return has_conversation_not_found or has_session_not_found
