"""Internal API endpoints behind ``charliebot slack reply`` and ``charliebot slack ack``."""

import fastapi
import pydantic

from src.features.chat_threads import thread_entry
from src.features.slack import slack_listener
from src.infra import config
from src.runtime import run_token, session_events, session_store, task_sessions
from src.runtime.api import deps

router = fastapi.APIRouter()


class SlackReplyRequest(pydantic.BaseModel):
  """Request body for the internal slack/reply endpoint: the calling session posts *text* to its own thread."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  text: str


class SlackAckRequest(pydantic.BaseModel):
  """Request body for the internal slack/ack endpoint: the calling session marks *message_ids* (Slack ts) as read."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  message_ids: list[str]


@router.post("/slack/reply")
async def slack_reply(
    req: SlackReplyRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    caller: run_token.CallerIdentity = fastapi.Depends(deps.require_caller),
) -> dict:
  """Post the calling session's reply to its own Slack thread and return the readback.

  The in-process boundary behind ``charliebot slack reply``: the session's
  platform origin names the thread, the running round's input names the summon
  the reply answers, and the readback (chars, chunks, over_budget, answers) is
  what the CLI prints. Refusals map SlackReplyError's status (404 unknown
  session, 409 no Slack thread, 422 blank text or a file-server link, 502 Slack
  rejected the post after retries); nothing is persisted on a refusal. New
  replies pass a freshness gate before chunk posts. A replay returns its stored
  readback before the freshness check. Eligible thread messages above the
  session's watermark refuse with a 412 ``stale_thread`` payload naming each
  unseen message.
  """
  try:
    request_id, run_started_at = await thread_entry.caller_reply_identity(
        caller, task_mgr, verb="slack reply", session_id=req.session_id, text=req.text)
    return await slack_listener.post_reply(
        req.session_id,
        req.text,
        cfg,
        store,
        events,
        request_id=request_id,
        run_started_at=run_started_at,
        tree=task_mgr)
  except slack_listener.SlackReplyError as exc:
    raise deps.http_refusal(exc) from exc


@router.post("/slack/ack")
async def slack_ack(
    req: SlackAckRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
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
  try:
    return await slack_listener.ack_messages(req.session_id, req.message_ids, cfg, store, events)
  except slack_listener.SlackReplyError as exc:
    raise deps.http_refusal(exc) from exc
