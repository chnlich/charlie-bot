"""The round-end audit of a web session: a nudge for an unanswered user message, then a notice if it stays unanswered.

Each step leaves a log marker (the nudge, the notice), so each happens at most once per user message.
"""

import asyncio

from src.features.web_reply import reply
from src.infra import event_types as ET
from src.infra import log_once
from src.runtime import master_trigger, session_events, session_lifecycle, session_store, session_successor
from src.runtime.api import message_utils

logger = log_once.LazyStructlogLogger()


async def audit_turn(
    session_id: str, done: dict, store: session_store.SessionStore, events: session_events.SessionEvents,
    lifecycle: session_lifecycle.SessionLifecycle, successor: session_successor.SessionSuccessor) -> None:
  """Audit one finished turn of a web session.

  The turn's newest user message or nudge decides the step. A user message that the log shows neither answered nor
  nudged gets the nudge, which wakes a new turn. A nudge whose user message the log still shows unanswered and
  unnoticed gets the notice, which marks the session unread. A turn with neither input, a background turn, gets
  nothing.
  """
  input_ids = message_utils.master_done_input_event_ids(done)
  if not input_ids:
    return
  loaded = await asyncio.to_thread(events.load_chat_events_sync, session_id)
  bound = reply.answered_input(loaded, input_ids)
  if bound is None:
    return
  input_event, user_event_id = bound
  if reply.replied(loaded, user_event_id):
    return
  if reply.nudge_of(input_event) is None:
    if not reply.nudged(loaded, user_event_id):
      await _nudge(session_id, user_event_id, events, store, successor, lifecycle)
  elif not reply.noticed(loaded, user_event_id):
    await _notice(session_id, user_event_id, events, lifecycle)


async def _nudge(
    session_id: str, user_event_id: str, events: session_events.SessionEvents, store: session_store.SessionStore,
    successor: session_successor.SessionSuccessor, lifecycle: session_lifecycle.SessionLifecycle) -> None:
  """Persist the nudge for *user_event_id* and wake a new turn with it."""
  nudge = message_utils.build_agent_message_event(
      reply.NUDGE_TEXT, from_session=session_id, from_session_name=reply.NUDGE_SENDER_NAME)
  nudge[ET.WEB_REPLY] = {"nudge_of": user_event_id}
  await events.persist_and_broadcast(session_id, nudge)
  master_trigger.spawn_master_turn(
      session_id,
      reply.NUDGE_TEXT,
      store,
      successor,
      lifecycle,
      input_id=nudge["id"],
      event_type=ET.AGENT_MESSAGE,
      actor="agent",
      from_session=session_id,
      from_session_name=reply.NUDGE_SENDER_NAME,
      task_name=f"web-reply-nudge-{session_id}")
  logger.info("web_reply_nudge", session=session_id, user_event_id=user_event_id, nudge_id=nudge["id"])


async def _notice(
    session_id: str, user_event_id: str, events: session_events.SessionEvents,
    lifecycle: session_lifecycle.SessionLifecycle) -> None:
  """Persist the no-reply notice for *user_event_id* and mark the session unread."""
  await events.persist_and_broadcast(
      session_id, {
          "type": ET.WEB_REPLY_NOTICE,
          "content": reply.NOTICE_TEXT,
          ET.WEB_REPLY_NOTICE: {
              ET.INPUT_EVENT_ID: user_event_id
          },
      })
  await lifecycle.mark_unread(session_id)
  logger.info("web_reply_notice", session=session_id, user_event_id=user_event_id)
