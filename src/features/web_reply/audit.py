"""The round-end audit of a web session: the status reminder, the reply nudge and the reply notice.

The status reminder covers every finished turn of an open task, background turns included. A turn that ran
``charliebot web status set`` or ``charliebot web status confirm`` needs no reminder, and neither does a turn that a
status reminder woke. The reply nudge covers a turn that answered a user message without a web reply. When one turn
misses both, one combined reminder covers both and wakes one turn.

Each step leaves a log marker (the status reminder, the nudge, the notice), so each happens at most once per turn or
user message.
"""

import asyncio

from src.features.web_reply import reply
from src.infra import event_types as ET
from src.infra import log_once
from src.runtime import (
    master_trigger,
    session_events,
    session_lifecycle,
    session_store,
    session_successor,
    task_sessions,
)
from src.runtime.api import message_utils

logger = log_once.LazyStructlogLogger()

# The ask every status reminder ends with, so the status reminder and the combined reminder cannot drift: the
# two status commands and the panel's reading.
STATUS_ASK_TEXT = (
    "Run `charliebot web status set` when Goal, Now or Waiting on you differs from the state that turn read back. "
    "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back.")
# The reminder a turn gets when it recorded no status set or confirm.
STATUS_REMINDER_TEXT = (
    f"Session status reminder: your last turn ended without a status set or confirm. {STATUS_ASK_TEXT}")
# The one reminder a turn gets when it missed both its web reply and its status.
COMBINED_REMINDER_TEXT = (
    "Web reply and session status reminder: your last turn woke on a user message and ended without a web reply and "
    f"without a status set or confirm. {reply.REPLY_ASK_TEXT} {STATUS_ASK_TEXT}")
# The sender name a status reminder carries in the chat log.
STATUS_REMINDER_SENDER_NAME = "Session status"


def is_status_reminder(event: dict) -> bool:
  """Whether *event* is a reminder that a turn ended without a status set or confirm."""
  return ET.WEB_STATUS_REMINDER in event


async def audit_turn(
    session_id: str,
    done: dict,
    store: session_store.SessionStore,
    events: session_events.SessionEvents,
    lifecycle: session_lifecycle.SessionLifecycle,
    successor: session_successor.SessionSuccessor,
    tree: task_sessions.TaskTreeManager,
    *,
    archived: bool = False) -> None:
  """Audit one finished turn of a web session for its status and its web reply.

  The status reminder needs a session that is not archived, an open task, no pending close request, a turn without a
  status event and a turn that no status reminder woke. The reply step nudges a turn that answered a user message
  without a web reply, and posts the notice that closes a nudge turn still without one. A turn that needs both gets
  one combined reminder and one wake.
  """
  input_ids = message_utils.master_done_input_event_ids(done)
  loaded = await asyncio.to_thread(events.load_chat_events_sync, session_id)
  status_needed = not archived and _status_reminder_needed(session_id, done, input_ids, loaded, tree)
  action, user_event_id = _reply_action(loaded, input_ids)
  turn_id = done.get("id")
  if status_needed and action == "nudge":
    assert user_event_id is not None, "a reply nudge names the user message it answers"
    await _post_status_reminder(
        session_id,
        COMBINED_REMINDER_TEXT,
        turn_id=turn_id,
        nudge_of=user_event_id,
        events=events,
        store=store,
        successor=successor,
        lifecycle=lifecycle)
  elif status_needed:
    await _post_status_reminder(
        session_id,
        STATUS_REMINDER_TEXT,
        turn_id=turn_id,
        nudge_of=None,
        events=events,
        store=store,
        successor=successor,
        lifecycle=lifecycle)
    if action == "notice":
      assert user_event_id is not None, "a reply notice names the user message it answers"
      await _notice(session_id, user_event_id, events, lifecycle)
  elif action == "nudge":
    assert user_event_id is not None, "a reply nudge names the user message it answers"
    await _nudge(session_id, user_event_id, events, store, successor, lifecycle)
  elif action == "notice":
    assert user_event_id is not None, "a reply notice names the user message it answers"
    await _notice(session_id, user_event_id, events, lifecycle)


def _reply_action(loaded: list[dict], input_ids: list[str]) -> tuple[str, str | None]:
  """The reply step this turn needs: ``none``, ``nudge`` or ``notice``, and the user message it answers."""
  bound = reply.answered_input(loaded, input_ids)
  if bound is None:
    return "none", None
  input_event, user_event_id = bound
  if reply.replied(loaded, user_event_id):
    return "none", None
  if reply.nudge_of(input_event) is None:
    if reply.nudged(loaded, user_event_id):
      return "none", None
    return "nudge", user_event_id
  if reply.noticed(loaded, user_event_id):
    return "none", None
  return "notice", user_event_id


def _turn_events(loaded: list[dict], done: dict) -> list[dict]:
  """The events this turn recorded, from the previous round end to *done*.

  The done event is already in the log when the hook runs. A caller that passes an unpersisted done has only this
  log to judge, so the whole log stands for the turn.
  """
  done_id = done.get("id")
  if done_id is None:
    return loaded
  for index, event in enumerate(loaded):
    if event.get("id") != done_id:
      continue
    start = 0
    for earlier in range(index - 1, -1, -1):
      if loaded[earlier].get("type") == ET.MASTER_DONE:
        start = earlier + 1
        break
    return loaded[start:index]
  return loaded


def _status_recorded(turn_events: list[dict]) -> bool:
  """Whether this turn recorded a status set or a status confirm."""
  return any(event.get("type") in (ET.WEB_STATUS, ET.WEB_STATUS_CONFIRMED) for event in turn_events)


def _reminder_already_posted(loaded: list[dict], turn_id: str | None) -> bool:
  """Whether a status reminder for the turn *turn_id* is already in the log."""
  if turn_id is None:
    return False
  return any(event.get(ET.WEB_STATUS_REMINDER) == turn_id for event in loaded)


def _woken_by_status_reminder(loaded: list[dict], input_ids: list[str]) -> bool:
  """Whether one of this turn's inputs is a status reminder."""
  wanted = set(input_ids)
  return any(event.get("id") in wanted and is_status_reminder(event) for event in loaded)


def _has_pending_close_request(tree: task_sessions.TaskTreeManager, session_id: str) -> bool:
  """Whether the task holds a close request that no close event has answered."""
  facts = tree.facts_of(session_id)
  closed = {event.get("request_id") for event in facts.close_events}
  return any(request.get("request_id") not in closed for request in facts.close_requests)


def _status_reminder_needed(
    session_id: str, done: dict, input_ids: list[str], loaded: list[dict], tree: task_sessions.TaskTreeManager) -> bool:
  """Whether this turn needs one status reminder."""
  if tree.task_state(session_id) != "open":
    return False
  if _has_pending_close_request(tree, session_id):
    return False
  if _status_recorded(_turn_events(loaded, done)):
    return False
  if _woken_by_status_reminder(loaded, input_ids):
    return False
  return not _reminder_already_posted(loaded, done.get("id"))


async def _post_status_reminder(
    session_id: str, text: str, *, turn_id: str | None, nudge_of: str | None, events: session_events.SessionEvents,
    store: session_store.SessionStore, successor: session_successor.SessionSuccessor,
    lifecycle: session_lifecycle.SessionLifecycle) -> None:
  """Persist one status reminder for *turn_id* and wake a new turn with it.

  *nudge_of* is the user message a combined reminder also answers without a web reply.
  """
  reminder = message_utils.build_agent_message_event(
      text, from_session=session_id, from_session_name=STATUS_REMINDER_SENDER_NAME)
  reminder[ET.WEB_STATUS_REMINDER] = turn_id if turn_id is not None else True
  if nudge_of is not None:
    reminder[ET.WEB_REPLY] = {"nudge_of": nudge_of}
  await events.persist_and_broadcast(session_id, reminder)
  master_trigger.spawn_master_turn(
      session_id,
      text,
      store,
      successor,
      lifecycle,
      input_id=reminder["id"],
      event_type=ET.AGENT_MESSAGE,
      actor="agent",
      from_session=session_id,
      from_session_name=STATUS_REMINDER_SENDER_NAME,
      task_name=f"web-status-reminder-{session_id}")
  logger.info("web_status_reminder", session=session_id, turn_id=turn_id, combined=nudge_of is not None)


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
