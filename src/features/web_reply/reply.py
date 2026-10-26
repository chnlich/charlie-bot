"""The web reply record, the web-session predicate, the answers binding, and the For-you layer.

A web session is a manager session that no Slack or Discord thread owns. Its agent posts the reply its user reads
through ``charliebot web reply``; each reply records the user message it answers. The For-you layer is the user's
messages, the replies and the notices, in log order.
"""

import re

from src.infra import constants, log_once, models
from src.infra import event_types as ET
from src.runtime import (
    control_events,
    file_urls,
    master_cc_state,
    midturn_delivery,
    session_events,
    session_lifecycle,
    session_store,
    task_execution,
    task_sessions,
)
from src.runtime.api import deps, message_utils

logger = log_once.LazyStructlogLogger()

# The language check holds a reply when the user's last message has at least this many Han characters and the
# reply's Han share is below the maximum. The Han share is defined in language_check_holds.
LANGUAGE_CHECK_MIN_USER_HAN = 5
LANGUAGE_CHECK_MAX_REPLY_HAN_SHARE = 0.05
# The 409 detail of a held reply. The agent reads it: it rewrites the reply in Chinese, or passes --keep-language.
LANGUAGE_HOLD_DETAIL = (
    "Language check: the user's last message is in Chinese.\n"
    "This reply has almost no Chinese.\n"
    "The command posted nothing.\n"
    "Rewrite the reply in Chinese.\n"
    "Then post the reply again.\n"
    "To post the reply as written, add --keep-language \"<reason>\" to the command.")
# The spans the Han share does not count: fenced and inline code, URLs and paths.
_CODE_FENCE = re.compile(r"```.*?```", re.DOTALL)
_INLINE_CODE = re.compile(r"`[^`]*`")
_URL = re.compile(r"\(?https?://\S+\)?")
_PATH = re.compile(r"(?<![\w])[~/][\w.\-/]+")
# An inline markdown link: the visible count keeps its label and drops its URL.
_MARKDOWN_LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")
# CJK Unified Ideographs (U+4E00-U+9FFF) and Extension A (U+3400-U+4DBF); Latin letters are the ASCII letters.
_HAN = re.compile(r"[㐀-䶿一-鿿]")
_LATIN = re.compile(r"[A-Za-z]")

# The rule file under prompts/ that a web session's manager turn carries.
RULES_FILE = "web_reply.md"

# The ask every reply reminder carries, so the nudge and the combined reminder cannot drift: post the reply
# now, and keep an answer past ``REPLY_BUDGET_CHARS`` on a page first.
REPLY_ASK_TEXT = (
    f"Post your answer as the web reply now. For an answer over {constants.REPLY_BUDGET_CHARS} characters, "
    "write the whole answer on a page first. Then post the conclusion and the page link as the web reply.")
# The audit's nudge: the agent's input when a user message woke a turn that ended without a reply.
NUDGE_TEXT = (
    "Web reply nudge: your last turn woke on a user message and ended without a web reply. The For-you view "
    f"hides your turn output. {REPLY_ASK_TEXT}")
# The notice the user reads when the nudge turn also ended without a reply.
NOTICE_TEXT = "No reply was posted for this message. The Everything view shows this turn's output."
# The sender name the nudge carries in the chat log.
NUDGE_SENDER_NAME = "Web reply"

# The For-you layer's roles: the chat renderer's role names for the three kinds the layer shows.
_FOR_YOU_ROLES = (ET.USER, ET.WEB_REPLY, ET.WEB_REPLY_NOTICE)


class WebReplyError(Exception):
  """A refused web reply: the HTTP status and the detail the route answers with."""

  def __init__(self, status: int, detail: str) -> None:
    super().__init__(detail)
    self.status = status
    self.detail = detail


def visible_reply_chars(text: str) -> int:
  """Count *text* characters after replacing each inline markdown link with its label."""
  return len(_MARKDOWN_LINK.sub(r"\1", text))


def is_web_session(meta: models.SessionMetadata) -> bool:
  """True for a manager session that no Slack or Discord thread owns."""
  return midturn_delivery.is_web_session(meta)


async def require_web_session(store: session_store.SessionStore, session_id: str) -> models.SessionMetadata:
  """The web session named by *session_id*; raises WebReplyError with 404 when unknown, 409 when not a web session."""
  meta = await store.get_session(session_id)
  if meta is None:
    raise WebReplyError(404, deps.SESSION_NOT_FOUND_DETAIL)
  if not is_web_session(meta):
    raise WebReplyError(409, "Session is not a web session: it is a thread session or a worker")
  return meta


def nudge_of(event: dict) -> str | None:
  """The user message an audit nudge was posted for, or None for any other event."""
  if event.get("type") != ET.AGENT_MESSAGE:
    return None
  return (event.get(ET.WEB_REPLY) or {}).get("nudge_of")


def answered_input(events: list[dict], input_event_ids: list[str]) -> tuple[dict, str] | None:
  """The newest user message or nudge among *input_event_ids*, with the user message it answers; None when neither.

  *events* is in log order, so the last match is the newest. A user message answers itself; a nudge answers the
  user message it was posted for.
  """
  wanted = set(input_event_ids)
  bound = None
  for ev in events:
    if ev.get("id") not in wanted:
      continue
    if ev.get("type") == ET.USER:
      bound = (ev, ev["id"])
    elif nudge_of(ev) is not None:
      bound = (ev, nudge_of(ev))
  return bound


def newest_user_text(events: list[dict]) -> str | None:
  """The text of the newest user message in *events*, which is in log order; None when the log holds none."""
  for ev in reversed(events):
    if ev.get("type") == ET.USER and ev.get("actor") == "user" and isinstance(ev.get("content"), str):
      return ev["content"]
  return None


def _han_and_latin(text: str) -> tuple[int, int]:
  """The Han and Latin-letter counts of *text*, leaving out code, URLs and paths."""
  body = text
  for pattern in (_CODE_FENCE, _INLINE_CODE, _URL, _PATH):
    body = pattern.sub(" ", body)
  return len(_HAN.findall(body)), len(_LATIN.findall(body))


def language_check_holds(user_text: str | None, reply_text: str) -> bool:
  """Whether the language check holds *reply_text* after the user message *user_text*; None means no user message.

  The check holds when the user message has at least LANGUAGE_CHECK_MIN_USER_HAN Han characters and the reply's Han
  share, Han / (Han + Latin letters), is below LANGUAGE_CHECK_MAX_REPLY_HAN_SHARE. A reply with no Han characters
  and no Latin letters is not held.
  """
  if user_text is None or _han_and_latin(user_text)[0] < LANGUAGE_CHECK_MIN_USER_HAN:
    return False
  reply_han, reply_latin = _han_and_latin(reply_text)
  if reply_han + reply_latin == 0:
    return False
  return reply_han / (reply_han + reply_latin) < LANGUAGE_CHECK_MAX_REPLY_HAN_SHARE


def replied(events: list[dict], user_event_id: str) -> bool:
  """Whether the log holds a web reply that answers *user_event_id*."""
  return any(
      ev.get("type") == ET.WEB_REPLY and (ev.get(ET.WEB_REPLY) or {}).get("answers") == user_event_id for ev in events)


def nudged(events: list[dict], user_event_id: str) -> bool:
  """Whether the log holds the nudge posted for *user_event_id*."""
  return any(nudge_of(ev) == user_event_id for ev in events)


def noticed(events: list[dict], user_event_id: str) -> bool:
  """Whether the log holds the no-reply notice for *user_event_id*."""
  return any(
      ev.get("type") == ET.WEB_REPLY_NOTICE and
      (ev.get(ET.WEB_REPLY_NOTICE) or {}).get(ET.INPUT_EVENT_ID) == user_event_id for ev in events)


async def post_web_reply(
    session_id: str,
    text: str,
    store: session_store.SessionStore,
    events: session_events.SessionEvents,
    lifecycle: session_lifecycle.SessionLifecycle,
    keep_language: str | None = None,
    *,
    request_id: str | None = None,
    tree: task_sessions.TaskTreeManager | None = None,
    scan_context: file_urls.ScanContext) -> dict:
  """Record *text* as the session's web reply and return the readback the CLI prints.

  The text is recorded exactly as written. It answers the newest user message or nudge of the running turn, and
  nothing when the running turn has neither. Blank text and dead file links raise WebReplyError before anything is
  recorded. The language check holds the text unless *keep_language*, the agent's reason for posting it as written,
  is given; the reason is then recorded on the reply.
  """
  await require_web_session(store, session_id)
  if not text.strip():
    raise WebReplyError(422, "Reply text is empty")
  dead_links = [finding for finding in file_urls.scan_file_links(text, scan_context) if finding.dead]
  if dead_links:
    raise WebReplyError(422, file_urls.dead_links_detail(dead_links))
  if keep_language is not None and not keep_language.strip():
    raise WebReplyError(422, "Keep-language reason is empty")
  chars = visible_reply_chars(text)
  task_tree = tree or task_execution.task_manager()
  epoch = await events.prime_aggregator(session_id)
  async with task_tree.control_lock:
    loaded = await task_tree.read_fact_history(session_id)
    bound = answered_input(loaded, master_cc_state.running_user_event_ids(session_id))
    answers = bound[1] if bound is not None else None
    web_reply = {"answers": answers, "chars": chars}
    if keep_language is not None:
      web_reply["keep_language"] = keep_language
    event_id = control_events.stable_request_event_id(session_id, request_id) if request_id is not None else None
    if request_id is not None:
      existing = control_events.replay_request_event(
          loaded,
          event_id=event_id,
          request_id=request_id,
          event_type=ET.WEB_REPLY,
          content=text,
      )
      if existing is not None:
        stored_reply = existing[ET.WEB_REPLY]
        over_budget = chars > constants.REPLY_BUDGET_CHARS
        return {
            "posted": True,
            "chars": chars,
            "over_budget": over_budget,
            "answers": stored_reply["answers"],
        }
    if keep_language is None and language_check_holds(newest_user_text(loaded), text):
      logger.info("web_reply_held", session=session_id, chars=chars)
      raise WebReplyError(409, LANGUAGE_HOLD_DETAIL)
    event = {
        "type": ET.WEB_REPLY,
        "content": text,
        ET.WEB_REPLY: web_reply,
    }
    if event_id is not None:
      event["id"] = event_id
      event["request_id"] = request_id
    await task_tree.session_events.save_chat_event(session_id, event)
  await events.announce_appended_event(session_id, event, epoch=epoch)
  await lifecycle.mark_unread(session_id)
  over_budget = chars > constants.REPLY_BUDGET_CHARS
  logger.info("web_reply_posted", session=session_id, chars=chars, over_budget=over_budget, answers=answers)
  return {"posted": True, "chars": chars, "over_budget": over_budget, "answers": answers}


def for_you_entries(events: list[dict]) -> list[dict]:
  """The For-you layer: the user's messages, the replies and the notices, in log order, as ``{role, content}`` rows."""
  return [
      {
          "role": message["role"],
          "content": message["content"]
      } for message in message_utils.events_to_messages(events) if message["role"] in _FOR_YOU_ROLES
  ]
