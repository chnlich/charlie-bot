"""Slack Socket Mode listener — both halves of the Slack entrypoint.

Summon path: an allowed ``app_mention`` resolves/creates a per-thread session,
persists the thread permalink as an agent message, and starts a master round
via ``trigger_master`` (without awaiting it).

Delivery path: ``deliver_done`` hangs off the round's terminal ``master_done``
event (called from ``SessionManager.persist_and_broadcast``), not off a waiting
coroutine — that is what makes delivery survive a server restart, since a
re-attached round still emits its done through the same funnel. The reply is
the marked reply block of the round's final composed assistant message: the
text after a lone ``SLACK REPLY:`` marker line, posted in full — one round posts
one message for any reply within Slack's per-message limit, and only a reply
past that hard limit is split into ordered paragraph-bounded replies.
``backfill_lost_summons`` closes the remaining hole at boot: a summon that was
still queued when the process died is answered by nothing, so it gets a notice
rather than vanishing.
The eyes ack reaction tracks the round in flight: lit at the summon, cleared
once the round's delivery attempt (normal or backfill) ends, whatever the post
outcome.
"""

import asyncio
import json
import re
import uuid
from datetime import datetime
from pathlib import Path
from typing import Any, Optional
from zoneinfo import ZoneInfo

import httpx
import structlog
import websockets

from src.api.message_utils import build_agent_message_event
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.http import get_http_client
from src.core.master_trigger import trigger_master
from src.core.message_aggregator import extract_text_from_message
from src.core.models import CreateSessionRequest, SessionStatus, SlackOrigin
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task
from src.core.verify_trailer import _normalize_line

logger = structlog.get_logger()

# Fixed namespace UUID for Slack summon session ids. Arbitrary but stable
# across process restarts; changing it would orphan every existing Slack-backed
# session.
SLACK_NS = uuid.UUID("1b4e28ba-2fa1-4d7a-9f0c-8d5e7a3b6c11")

# Fixed citation boundary appended to every Slack-sourced prompt so the master
# scopes its citations to the channel/thread and public content only.
CITATION_BOUNDARY = (
    "引用边界：只引用这条频道／线程本身、公开仓库、公开频道；现场只读命令取得的运行状态可引用并附取数命令；已成文的私有内容不引用。"
)

_ACCEPTANCE_REACTION = "eyes"

_LOCAL_TZ = ZoneInfo("America/Los_Angeles")

# Slack's hard per-message text limit. 40000 is the ceiling, so a long single
# message is left for the client to collapse; splitting is the above-limit
# fallback only.
_MAX_POST_CHARS = 40000

# The reply budget the format contract states (prompts/slack_reply_format.md):
# replies should stay under this many chars, with the depth going to a page.
# This is the single measurement point for that budget.
_REPLY_BUDGET_CHARS = 500

# The single authority for the marker line that starts a round's reply: the
# reply-format contract (prompts/slack_reply_format.md) states the same string,
# and the parser below treats a line as the marker only when its normalized form
# equals this exactly.
SLACK_REPLY_MARKER = "SLACK REPLY:"

# Waits between the retries of one Slack call; the answer stays readable in the
# session log either way, so exhausting them logs an error rather than raising.
_RETRY_DELAYS = (1.0, 4.0)

_LOST_SUMMON_NOTICE = "上一次召唤在服务重启时丢失了，没有被处理。需要的话请重新 @ 我一次。"

_LOST_SUMMON_CONTENT = (
    "这条 Slack 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。")


def summon_session_id(team_id: str, channel_id: str, thread_ts: str) -> str:
  """Return the deterministic session id for a Slack thread."""
  return str(uuid.uuid5(SLACK_NS, f"slack:{team_id}:{channel_id}:{thread_ts}"))


class SlackClient:
  """Thin Slack Web API wrapper: open_connection / post_message / get_permalink / add_reaction /
  remove_reaction / get_channel_name."""

  def __init__(self, http: httpx.AsyncClient, *, bot_token: str, app_token: str) -> None:
    self._http = http
    self._bot_headers = {"Authorization": f"Bearer {bot_token}"}
    self._app_headers = {"Authorization": f"Bearer {app_token}"}
    # In-process channel-name cache, keyed by channel id; failures cache as None.
    self._channel_name_cache: dict[str, str | None] = {}

  async def open_connection(self) -> str:
    """POST apps.connections.open and return the wss: socket url."""
    resp = await self._http.post(
        "https://slack.com/api/apps.connections.open", headers=self._app_headers)
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"apps.connections.open failed: {payload}")
    return payload["url"]

  async def post_message(self, channel: str, text: str, thread_ts: Optional[str] = None) -> dict:
    """POST chat.postMessage; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "text": text}
    if thread_ts is not None:
      body["thread_ts"] = thread_ts
    resp = await self._http.post(
        "https://slack.com/api/chat.postMessage", headers=self._bot_headers, json=body)
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"chat.postMessage failed: {payload}")
    return payload

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Add one emoji reaction to a message; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "name": name, "timestamp": ts}
    resp = await self._http.post(
        "https://slack.com/api/reactions.add", headers=self._bot_headers, json=body)
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"reactions.add failed: {payload}")
    return payload

  async def remove_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Remove one emoji reaction from a message; returns the API payload.

    A ``no_reaction`` error already is the end state, so the clear is
    idempotent; any other ok=false payload raises.
    """
    body: dict[str, Any] = {"channel": channel, "name": name, "timestamp": ts}
    resp = await self._http.post(
        "https://slack.com/api/reactions.remove", headers=self._bot_headers, json=body)
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok") and payload.get("error") != "no_reaction":
      raise RuntimeError(f"reactions.remove failed: {payload}")
    return payload

  async def get_permalink(self, channel: str, ts: str) -> str:
    """GET chat.getPermalink for one message; return its permalink url."""
    resp = await self._http.get(
        "https://slack.com/api/chat.getPermalink",
        headers=self._bot_headers,
        params={"channel": channel, "message_ts": ts},
    )
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"chat.getPermalink failed: {payload}")
    return payload["permalink"]

  async def get_channel_name(self, channel_id: str) -> str | None:
    """Resolve a channel id to its display name via conversations.info.

    Cached in-process per channel id: cache hits return immediately and
    failures cache as None for the process lifetime. On any failure —
    missing_scope, channel_not_found, HTTP error, exception — logs one
    warning and returns None; never raises.
    """
    if channel_id in self._channel_name_cache:
      return self._channel_name_cache[channel_id]
    name: str | None = None
    try:
      resp = await self._http.get(
          "https://slack.com/api/conversations.info",
          headers=self._bot_headers,
          params={"channel": channel_id},
      )
      resp.raise_for_status()
      payload = resp.json()
      if payload.get("ok"):
        name = payload["channel"]["name"]
      else:
        logger.warning("slack_channel_name_unresolved", channel=channel_id,
                       error=payload.get("error"))
    except Exception as e:
      logger.warning("slack_channel_name_resolve_failed", channel=channel_id, error=str(e))
    self._channel_name_cache[channel_id] = name
    return name


def _load_prompt_doc(repo_root: Path, name: str, *, likely_cause: str) -> str:
  """Read one prompts doc fresh from disk, raising a ValueError naming the path when missing.

  No caching, so an edit takes effect on the next summon. A missing or
  unreadable doc raises a ValueError naming the path and its most likely cause
  (mirrors the worker-prompt loader in src/core/spawner.py); a prompt without
  the doc is never built.
  """
  path = repo_root / "prompts" / name
  try:
    return path.read_text(encoding="utf-8").strip()
  except OSError as e:
    raise ValueError(f"{name} prompt not found at {path} — {likely_cause}") from e


def _build_summon_prompt(permalink: str, cfg: CharlieBotConfig) -> str:
  """The persisted summon: the thread link plus a self-fetch hint, ending at the fixed notices.

  Only the permalink is stored because the master reads the thread itself via
  its slack skill when the round runs — a snapshot persisted here would go
  stale as the thread keeps changing after the mention.

  Both the PII red line and the reply-format contract are read fresh from
  prompts/ on every call — no caching, so an edit takes effect on the next
  summon. A missing or unreadable doc raises a ValueError naming the path; a
  prompt without both docs is never built.
  """
  red_line = _load_prompt_doc(
      cfg.charlie_bot_repo, "slack_reply_redline.md",
      likely_cause="the repo checkout most likely predates the slack-reply-redline extraction commit")
  reply_format = _load_prompt_doc(
      cfg.charlie_bot_repo, "slack_reply_format.md",
      likely_cause="the repo checkout most likely predates the slack-reply-format extraction commit")
  return (f"Slack 线程召唤：{permalink}\n\n"
          "用 slack 技能按链接读线程（conversations.replies，channel 与 thread_ts 从链接解析）。\n\n"
          f"{CITATION_BOUNDARY}\n{red_line}\n{reply_format}")


def _local_time() -> str:
  """Local wall-clock stamp for a session display name."""
  return datetime.now(_LOCAL_TZ).strftime("%Y-%m-%d %H:%M")


async def ensure_slack_group(session_mgr: SessionManager, sid: str, label: str) -> None:
  """Group a Slack-summon session under its label, unless it already has a group.

  The label is resolved once by ``handle_app_mention`` (the single resolution
  point) and passed through — this function resolves nothing. It only writes
  when the session has a slack_origin and an empty group (None or '') — an
  existing group is never overwritten. Best-effort: any failure logs a warning
  and is swallowed, so summon, round, and reply behavior are unaffected.
  """
  try:
    meta = await session_mgr.get_session(sid)
    if meta is None or meta.slack_origin is None or meta.group:
      return
    await session_mgr.set_group(sid, label)
  except Exception as e:
    logger.warning("slack_group_assignment_failed", session=sid, label=label, error=str(e))


async def handle_app_mention(event: dict, cfg, session_mgr, client: SlackClient) -> Optional[str]:
  """Accept or drop one app_mention. Returns the session id when accepted, else None.

  The summon's channel label is resolved exactly once here, before
  create_session, and reused for both the session name and the group: a single
  ``Slack #<channel_name>`` label keeps the two display fields in lockstep and
  the resolution count to one lookup per accepted mention. A name that cannot
  be resolved falls back to the channel id.
  """
  channel_id = event.get("channel")
  thread_ts = event.get("thread_ts") or event.get("ts")
  slack_user = event.get("user")

  if event.get("type") != "app_mention" or slack_user not in cfg.slack_allowed_user_ids:
    logger.debug("slack_mention_dropped", channel=channel_id, thread_ts=thread_ts, slack_user=slack_user)
    return None

  team_id = event.get("team") or event.get("team_id")
  sid = summon_session_id(team_id, channel_id, thread_ts)

  name = await client.get_channel_name(channel_id)
  label = f"Slack #{name or channel_id}"

  session_meta = await session_mgr.get_session(sid)
  if session_meta is None:
    session_name = f"{label} {_local_time()}"
    await session_mgr.create_session(
        CreateSessionRequest(
            session_id=sid,
            name=session_name,
            slack_origin=SlackOrigin(team_id=team_id, channel_id=channel_id, thread_ts=thread_ts)))
    logger.info("slack_mention_session_created", channel=channel_id, thread_ts=thread_ts,
                slack_user=slack_user, session=sid)
  elif session_meta.status == SessionStatus.ARCHIVED:
    await session_mgr.unarchive_session(sid)
    logger.info("slack_mention_session_unarchived", channel=channel_id, thread_ts=thread_ts,
                slack_user=slack_user, session=sid)
  else:
    logger.info("slack_mention_session_existing", channel=channel_id, thread_ts=thread_ts,
                slack_user=slack_user, session=sid)

  await ensure_slack_group(session_mgr, sid, label)

  permalink = await client.get_permalink(channel_id, event["ts"])
  content = _build_summon_prompt(permalink, cfg)

  evt = build_agent_message_event(content, from_session=sid, from_session_name="Slack")
  evt["slack"] = {
      "channel_id": channel_id,
      "thread_ts": thread_ts,
      "mention_ts": event.get("ts"),
  }
  await session_mgr.persist_and_broadcast(sid, evt)
  round_event_id = evt.get("id")
  logger.info("slack_mention_round_started", channel=channel_id, thread_ts=thread_ts,
              slack_user=slack_user, session=sid, user_event_id=round_event_id)

  create_logged_task(
      trigger_master(sid, content, cfg, session_mgr, user_event_id=round_event_id),
      name=f"slack-round-{sid}")
  create_logged_task(
      client.add_reaction(channel_id, _ACCEPTANCE_REACTION, event["ts"]),
      name=f"slack-ack-{sid}")
  return sid


# ---------------------------------------------------------------------------
# Reply delivery
# ---------------------------------------------------------------------------


def _bot_client(cfg: CharlieBotConfig) -> SlackClient:
  """The client every outbound path (delivery, backfill) posts through."""
  return SlackClient(get_http_client(), bot_token=cfg.slack_bot_token, app_token=cfg.slack_app_token)


async def _post_with_retry(client: SlackClient, channel: str, thread_ts: str, text: str, *,
                           session_id: str) -> bool:
  """Post one thread reply, retrying on failure; True when Slack accepted it.

  Exhausting the retries logs an error and returns False instead of raising:
  the answer is readable in the session log either way, and delivery must never
  break the caller (the event funnel, or the rest of a backfill pass).
  """
  attempts = len(_RETRY_DELAYS) + 1
  for attempt in range(attempts):
    try:
      await client.post_message(channel, text, thread_ts=thread_ts)
      return True
    except Exception as e:
      if attempt == attempts - 1:
        logger.error("slack_post_gave_up", session=session_id, channel=channel, thread_ts=thread_ts,
                     attempts=attempts, error=str(e))
        return False
      logger.warning("slack_post_retry", session=session_id, channel=channel, thread_ts=thread_ts,
                     attempt=attempt + 1, error=str(e))
      await asyncio.sleep(_RETRY_DELAYS[attempt])


def _ack_clear(client: SlackClient, slack_block: dict, session_id: str) -> None:
  """Clear the summon eye once its round's delivery attempt has ended.

  Fires the remove as its own logged task — a failure there only leaves one
  stale eye plus a background_task_failed log and never touches the delivery
  result. Skipped when the persisted slack block carries no mention_ts.
  """
  mention_ts = slack_block.get("mention_ts")
  if mention_ts is None:
    return
  create_logged_task(
      client.remove_reaction(slack_block["channel_id"], _ACCEPTANCE_REACTION, mention_ts),
      name=f"slack-ack-clear-{session_id}")


def _slack_target(events: list[dict], input_event_id: str) -> Optional[dict]:
  """The ``slack`` block of the event this round answers, or None when it has none.

  None covers both "no such event" and "a message the user typed into this same
  session from the browser": neither is a Slack injection, so neither delivers.
  """
  for ev in events:
    if ev.get("id") == input_event_id:
      return ev.get("slack")
  return None


def _already_answered(events: list[dict], done_idx: int, input_event_id: str) -> bool:
  """True when an earlier master_done in the log already answered *input_event_id*.

  The window between the done persist and the master_run clear
  (src/agents/master_cc.py) can replay a round, and a resumed failed round
  carries ``exit_code = -1``, so exit code cannot be the discriminator.
  """
  return any(
      ev.get("type") == ET.MASTER_DONE and ev.get("input_event_id") == input_event_id
      for ev in events[:done_idx])


def _round_text(events: list[dict], done_idx: int) -> str:
  """The window's last assistant message carrying content: the round's composed reply.

  Middle assistant messages are work narration ("let me look at…"); the reply
  to the summoner is composed in the last one, per the reply-format notice in
  the summon prompt, so only that one is posted. Empty or whitespace-only
  messages don't qualify. The window still starts after the previous
  master_done: slicing from the summon instead would sweep in a previous
  round's answer when the summon queued behind another round.
  """
  start = 0
  for idx in range(done_idx - 1, -1, -1):
    if events[idx].get("type") == ET.MASTER_DONE:
      start = idx + 1
      break
  for idx in range(done_idx - 1, start - 1, -1):
    if events[idx].get("type") != ET.ASSISTANT:
      continue
    text = extract_text_from_message(events[idx].get("message"))
    if text.strip():
      return text
  return ""


def _extract_marker_reply(text: str) -> tuple[str, int | None]:
  """The text after the marker line, or a failure count when it is not unique.

  Returns ``(reply, None)`` for exactly one marker line, ``("", n)`` for n !=
  1 marker lines. A marker line is a whole normalized line equal to
  ``SLACK_REPLY_MARKER``; an occurrence inside a sentence is not one. The reply
  keeps the text after the marker verbatim, minus leading blank lines. Zero and
  many marker lines are reported apart so the caller can log them differently.
  """
  count = 0
  marker_end = 0
  offset = 0
  for raw_line in text.splitlines(keepends=True):
    if _normalize_line(raw_line) == SLACK_REPLY_MARKER:
      count += 1
      marker_end = offset + len(raw_line)
    offset += len(raw_line)
  if count != 1:
    return "", count
  reply = text[marker_end:]
  leading = 0
  for line in reply.splitlines(keepends=True):
    if line.strip():
      break
    leading += len(line)
  return reply[leading:], None


def _chunk_text(text: str, limit: int = _MAX_POST_CHARS) -> list[str]:
  """Split *text* into chunks of at most *limit* chars for sequential posting.

  Greedy packing over paragraph units (a paragraph plus its trailing
  blank-line separator); a unit longer than *limit* falls to newline units,
  and a single line longer than *limit* is hard-cut. Chunks keep the input
  order and concatenate back to the input byte-for-byte — a boundary never
  eats content.
  """
  if len(text) <= limit:
    return [text]
  pieces: list[str] = []
  paragraphs = re.split(r"(\n{2,})", text)  # alternates paragraph / separator
  for i in range(0, len(paragraphs), 2):
    unit = paragraphs[i] + (paragraphs[i + 1] if i + 1 < len(paragraphs) else "")
    if len(unit) <= limit:
      pieces.append(unit)
      continue
    for line in re.split(r"(?<=\n)", unit):  # a line plus its trailing newline
      if len(line) <= limit:
        pieces.append(line)
      else:
        pieces.extend(line[j:j + limit] for j in range(0, len(line), limit))
  chunks: list[str] = []
  current = ""
  for piece in pieces:
    if piece and len(current) + len(piece) > limit:
      chunks.append(current)
      current = ""
    current += piece
  if current:
    chunks.append(current)
  # A whitespace-only chunk (possible only from leading input whitespace) is
  # dropped: Slack rejects text-less posts, and no content is lost by it.
  return [c for c in chunks if c.strip()] or chunks


async def deliver_done(session_id: str, done: dict, cfg: CharlieBotConfig,
                       session_mgr: SessionManager) -> bool:
  """Post one finished round's answer back to the Slack thread it was summoned from.

  Called as a fire-and-forget task from ``persist_and_broadcast`` for every
  ``master_done``; returns False without posting unless the round belongs to a
  Slack injection that no earlier done already answered. When the round's last
  message carries exactly one marker line, only the marked reply block is
  posted, split into ordered thread replies when it exceeds one post's budget;
  a missing or ambiguous marker posts nothing and keeps the ack reaction.
  Returns True when every reply of the round reached Slack.
  """
  meta = await session_mgr.get_session(session_id)
  if meta is None or meta.slack_origin is None:
    return False

  input_event_id = done.get("input_event_id")
  if input_event_id is None:
    return False  # guard-path dones (src/api/chat.py) answer no recorded event

  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  target = _slack_target(events, input_event_id)
  if target is None:
    return False

  done_id = done["id"]
  done_idx = next((i for i in range(len(events) - 1, -1, -1) if events[i].get("id") == done_id), None)
  if done_idx is None:
    raise RuntimeError(f"master_done {done_id} missing from session {session_id} log")
  if _already_answered(events, done_idx, input_event_id):
    logger.info("slack_delivery_duplicate_done", session=session_id, input_event_id=input_event_id,
                exit_code=done.get("exit_code"))
    return False

  thread_ts = target["thread_ts"]
  text = _round_text(events, done_idx)
  if not text:
    bodies = [f"这一轮没有产生任何回复内容（exit_code={done.get('exit_code')}），请到会话里查看详情。"]
    measured = text  # the empty-round notice reports the empty round text itself
  else:
    reply, marker_count = _extract_marker_reply(text)
    if marker_count is None:
      if not reply.strip():
        logger.error("slack_reply_marker_empty", session=session_id,
                     input_event_id=input_event_id)
        return False
      bodies = _chunk_text(reply)
      measured = reply
    elif marker_count == 0:
      logger.error("slack_reply_marker_missing", session=session_id,
                   input_event_id=input_event_id)
      return False
    else:
      logger.error("slack_reply_marker_ambiguous", session=session_id,
                   input_event_id=input_event_id, marker_count=marker_count)
      return False

  # A chunk that exhausts its retries is already logged by _post_with_retry;
  # keep posting the rest: the full text lives in the session log regardless,
  # and partial delivery beats none.
  posted = True
  client = _bot_client(cfg)
  for body in bodies:
    ok = await _post_with_retry(client, target["channel_id"], thread_ts, body, session_id=session_id)
    posted = posted and ok
  logger.info("slack_delivery_done", session=session_id, channel=target["channel_id"],
              thread_ts=thread_ts, input_event_id=input_event_id, chars=len(measured),
              chunks=len(bodies), posted=posted, over_budget=len(measured) > _REPLY_BUDGET_CHARS,
              budget=_REPLY_BUDGET_CHARS)
  # The delivery attempt ended: the eye goes out whether or not it posted.
  _ack_clear(client, target, session_id)
  return posted


# ---------------------------------------------------------------------------
# Boot backfill
# ---------------------------------------------------------------------------


def _lost_summons(events: list[dict], *, owned: set[str], running: Optional[str]) -> list[dict]:
  """The Slack injections in one session's log that nothing will ever answer.

  A summon is lost when no master_done names it, this process does not already
  own it (queued or running), the session's master_run record does not name it
  (alive but not followable), and no earlier backfill marked it. The marker is
  the ``slack_backfill`` payload, never a synthetic master_done: that event is
  the cut point replay uses to decide which user messages are still unanswered.
  """
  answered = {ev.get("input_event_id") for ev in events if ev.get("type") == ET.MASTER_DONE}
  marked = {ev["slack_backfill"].get("input_event_id") for ev in events if "slack_backfill" in ev}
  return [
      ev for ev in events
      if ev.get("type") == ET.AGENT_MESSAGE and "slack" in ev and ev["id"] not in answered
      and ev["id"] not in marked and ev["id"] not in owned and ev["id"] != running
  ]


async def backfill_lost_summons(cfg: CharlieBotConfig, session_mgr: SessionManager) -> int:
  """Report every Slack summon lost while queued; returns how many were reported.

  The startup replay covers ``ET.USER`` only (src/core/init.py), so a Slack
  injection sitting in the queue when the process died is picked up by nothing.
  Runs once per boot, after re-attach and replay have had their chance.
  """
  from src.agents import master_cc  # lazy: mirrors the spawner import's cycle guard

  sessions = await session_mgr.list_sessions()  # archived included: a thread can be summoned again
  client = _bot_client(cfg)
  reported = 0
  for meta in sessions:
    if meta.slack_origin is None:
      continue
    events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)
    lost = _lost_summons(
        events,
        owned=master_cc.queued_user_event_ids(meta.id),
        running=meta.master_run.user_event_id if meta.master_run else None)
    for ev in lost:
      # Persist the marker before posting: a crash in between costs one notice,
      # while posting first would re-post it on every boot until the marker landed.
      await session_mgr.persist_and_broadcast(meta.id, {
          "type": ET.ASSISTANT_ERROR,
          "content": _LOST_SUMMON_CONTENT,
          "slack_backfill": {"input_event_id": ev["id"]},
      })
      slack = ev["slack"]
      await _post_with_retry(
          client, slack["channel_id"], slack["thread_ts"], _LOST_SUMMON_NOTICE, session_id=meta.id)
      _ack_clear(client, slack, meta.id)
      reported += 1
      logger.info("slack_backfill_lost_summon", session=meta.id, channel=slack["channel_id"],
                  thread_ts=slack["thread_ts"], input_event_id=ev["id"])
  return reported


async def _expect_hello(ws) -> None:
  """Consume the Socket Mode connection's ``hello`` frame."""
  raw = await ws.recv()
  envelope = json.loads(raw)
  if envelope.get("type") != "hello":
    logger.warning("slack_listener_expected_hello", received=envelope.get("type"))


async def run_listener(cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
  """Socket Mode connect/receive/reconnect loop; never returns."""
  http = get_http_client()
  client = SlackClient(http, bot_token=cfg.slack_bot_token, app_token=cfg.slack_app_token)
  backoff = 1.0

  while True:
    try:
      url = await client.open_connection()
    except Exception as e:
      logger.warning("slack_listener_connect_failed", error=str(e))
      await asyncio.sleep(backoff)
      backoff = min(backoff * 2, 30.0)
      continue
    backoff = 1.0

    try:
      async with websockets.connect(url, max_size=None) as ws:
        await _expect_hello(ws)
        logger.info("slack_listener_connected")
        async for raw in ws:
          envelope = json.loads(raw)
          envelope_id = envelope.get("envelope_id")
          if envelope.get("type") == "disconnect":
            logger.info("slack_listener_disconnect")
            break
          logger.debug("slack_listener_envelope", envelope_id=envelope_id)
          if envelope_id is not None:
            await ws.send(json.dumps({"envelope_id": envelope_id}))
          inner = None
          if envelope.get("type") == "events_api":
            payload = envelope.get("payload") or {}
            inner = payload.get("event")
          if inner and inner.get("type") == "app_mention":
            channel = inner.get("channel")
            thread_ts = inner.get("thread_ts") or inner.get("ts")
            slack_user = inner.get("user")
            try:
              sid = await handle_app_mention(inner, cfg, session_mgr, client)
              logger.info("slack_listener_app_mention_handled", channel=channel, thread_ts=thread_ts,
                          slack_user=slack_user, session=sid)
            except Exception as e:
              logger.exception("slack_listener_app_mention_handle_failed", channel=channel,
                               thread_ts=thread_ts, slack_user=slack_user, error=str(e))
    except Exception as e:
      logger.warning("slack_listener_connection_dropped", error=str(e))

    await asyncio.sleep(backoff)
    backoff = min(backoff * 2, 30.0)
