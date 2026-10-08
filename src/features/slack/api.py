"""Internal API endpoints behind ``charliebot slack reply`` and ``charliebot slack ack``."""

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, ConfigDict

from src.infra.config import CharlieBotConfig
from src.runtime.api.deps import get_config_on_loop, get_session_manager
from src.runtime.sessions import SessionManager

router = APIRouter()


class SlackReplyRequest(BaseModel):
  """Request body for the internal slack/reply endpoint: the calling session posts *text* to its own thread."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  text: str


class SlackAckRequest(BaseModel):
  """Request body for the internal slack/ack endpoint: the calling session marks *message_ids* (Slack ts) as read."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  message_ids: list[str]


@router.post("/slack/reply")
async def slack_reply(
    req: SlackReplyRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Post the calling session's reply to its own Slack thread and return the readback.

  The in-process boundary behind ``charliebot slack reply``: the session's
  platform origin names the thread, the running round's input names the summon
  the reply answers, and the readback (chars, chunks, over_budget, answers) is
  what the CLI prints. Refusals map SlackReplyError's status (404 unknown
  session, 409 no Slack thread, 422 blank text or a file-server link, 502 Slack
  rejected the post after retries); nothing is persisted on a refusal. Freshness is gated first:
  eligible thread messages above the session's watermark refuse with a 412
  ``stale_thread`` payload naming each unseen message, before any chunk posts.
  """
  from src.features.slack.slack_listener import SlackReplyError, assert_thread_fresh, post_reply
  try:
    await assert_thread_fresh(req.session_id, cfg, session_mgr)
    return await post_reply(req.session_id, req.text, cfg, session_mgr)
  except SlackReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/slack/ack")
async def slack_ack(
    req: SlackAckRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Mark the calling session's read thread messages as consumed and return the readback.

  The boundary behind ``charliebot slack ack``: ``message_ids`` are Slack ts
  values, every one must be eligible, and every eligible id at or below the
  newest must be included — a skipped id (or an unknown/ineligible one) refuses
  with 422 naming it and persists nothing. Success advances the session's
  read watermark, persists a small ack event for the audit trail, and
  returns ``acked`` plus the new watermark; re-acking ids at or below the
  watermark is an idempotent no-op counted as acked. Refusals map
  SlackReplyError's status: 404 unknown session, 409 no Slack thread.
  """
  from src.features.slack.slack_listener import SlackReplyError, ack_messages
  try:
    return await ack_messages(req.session_id, req.message_ids, cfg, session_mgr)
  except SlackReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc
