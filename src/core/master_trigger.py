"""Master-agent triggering subsystem — wake the master CC to process results."""

import traceback
from datetime import UTC, datetime, timedelta
from zoneinfo import ZoneInfo

import structlog

from src.agents.master_cc import run_message
from src.core import event_types as ET
from src.core.config import HOUSE_TIMEZONE, CharlieBotConfig
from src.core.models import SessionMetadata, SessionStatus
from src.core.sessions import SessionManager

log = structlog.get_logger()


async def run_message_with_resume_recovery(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    summary: str,
    session_mgr: SessionManager,
    expect_fresh_session: bool = False,
    user_event_id: str | None = None,
) -> str | None:
  """Call run_message, retrying once with cc_session_id cleared on stale-resume errors.

  ``expect_fresh_session`` and ``user_event_id`` are forwarded to the first
  ``run_message`` call only. The stale-resume retry deliberately clears the
  anchor but must NOT forward either: an anchor-missing alarm there is correct
  (the resume failed and context is being dropped as recovery).
  """
  backend_id = session_meta.backend
  backend_option = cfg.get_backend_option(backend_id)
  try:
    return await run_message(
        cfg,
        session_meta,
        summary,
        session_mgr.callbacks(),
        skip_user_event=True,
        auto_trigger=True,
        backend_option=backend_option,
        expect_fresh_session=expect_fresh_session,
        user_event_id=user_event_id,
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
    # expect_fresh_session intentionally not forwarded: an alarm on the retry
    # path is correct (context is being dropped as stale-resume recovery).
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
    user_event_id: str | None = None,
    # Default True = pull back, so any wake path added later carries content and
    # unarchives its target without being listed here. Only the two timed wakes
    # (the trigger fire in src/core/triggers.py and the cron scheduler in
    # src/core/scheduler.py) pass pull_back=False and keep the skip.
    pull_back: bool = True,
) -> None:
  """Best-effort trigger of the master agent to process a worker result.

  A target archived without a successor is pulled back to active first when
  ``pull_back`` is set (the default); timed wakes opt out and skip instead.
  """
  target_session_id = session_id
  try:
    # Resolve through the succession chain: an elone may have landed since this
    # wake was scheduled, so the run targets the chain end rather than the
    # originally-requested session.
    resolved = await session_mgr.resolve_successor_chain(session_id)
    if resolved is None:
      log.error("trigger_master_session_not_found", session=session_id)
      return

    # The resolved target is what the wake falls back to for the error write.
    target_session_id = resolved.id

    if resolved.id != session_id:
      log.info(
          "trigger_master_redirected_to_successor",
          session=session_id,
          resolved_session=resolved.id,
      )

    # An archived session with no successor is the user's explicit "no more
    # wakes" signal. Timed wakes (pull_back=False) skip entirely: no run, no
    # event. Content wakes pull the session back to active first and continue.
    # An archived session WITH a successor has already been eloned and
    # redirects above instead.
    if resolved.status == SessionStatus.ARCHIVED and resolved.successor_session_id is None:
      if not pull_back:
        log.info(
            "wake_skipped_archived",
            session=session_id,
            resolved_session=resolved.id,
        )
        return
      await session_mgr.unarchive_session(resolved.id)
      log.info(
          "wake_pulled_back_archived_session",
          session=session_id,
          resolved_session=resolved.id,
      )

    session_meta = await session_mgr.get_session(resolved.id)
    if not session_meta:
      log.error("trigger_master_session_not_found", session=resolved.id)
      return

    # expect_fresh_session is True only on the weekly-recycle path that
    # deliberately clears the anchor; it suppresses the resume-anchor-missing
    # pre-flight alarm. The stale-resume retry clears the anchor too but must
    # NOT set this flag (an alarm there is correct).
    expect_fresh_session = False
    if (session_meta.scheduled_task and session_meta.cc_session_id and session_meta.cc_session_started_at):
      pt = ZoneInfo(HOUSE_TIMEZONE)
      now_pt = datetime.now(pt)
      # Most recent Saturday 1:00 AM PT
      days_since_sat = (now_pt.weekday() - 5) % 7
      last_sat_1am_pt = now_pt.replace(hour=1, minute=0, second=0, microsecond=0) - timedelta(days=days_since_sat)
      last_sat_1am_utc = last_sat_1am_pt.astimezone(UTC)
      if session_meta.cc_session_started_at < last_sat_1am_utc < datetime.now(UTC):
        log.info(
            'scheduled_cc_session_expired', session=resolved.id, started_at=str(session_meta.cc_session_started_at))
        session_meta.cc_session_id = None
        session_meta.cc_session_started_at = None
        await session_mgr.save_metadata(session_meta)
        # The weekly recycle deliberately clears the anchor; the next run is an
        # intentional fresh start, so suppress the resume-anchor-missing alarm.
        expect_fresh_session = True
        try:
          result = await session_mgr.recycle_scheduled_session(resolved.id, last_sat_1am_utc)
          log.info('scheduled_session_recycled', session=resolved.id, **result)
        except Exception:
          log.exception('scheduled_session_recycle_failed', session=resolved.id)

    master_summary = summary
    if not session_meta.cc_session_id and session_meta.scheduled_task:
      master_summary = (
          f"[Auto-triggered scheduled task result for '{session_meta.scheduled_task}']\n"
          "Review the worker/reviewer results below. Check: was the branch merged? "
          "Are there errors? Summarize the outcome.\n\n"
          f"{summary}")

    await run_message_with_resume_recovery(
        cfg,
        session_meta,
        master_summary,
        session_mgr,
        expect_fresh_session=expect_fresh_session,
        user_event_id=user_event_id)
  except Exception as e:
    log.error("trigger_master_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    try:
      error_payload = {
          'type': ET.ERROR,
          'message': f'Failed to notify master agent: {e}',
          'source': 'trigger_master',
      }
      await session_mgr.persist_and_broadcast(target_session_id, error_payload)
    except Exception:
      pass  # Last resort — nothing more we can do


def is_resume_not_found_error(error: Exception) -> bool:
  """Return True only for stale resume errors where session/conversation is missing."""
  message = str(error).lower()
  has_no_rollout_found = "no rollout found" in message and ("thread" in message or "resume failed" in message)
  if has_no_rollout_found:
    return True
  if "resume" not in message:
    return False

  has_conversation_not_found = "conversation" in message and "not found" in message
  has_session_not_found = "session" in message and "not found" in message
  return has_conversation_not_found or has_session_not_found
