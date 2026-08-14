"""Slack Socket Mode listener — both halves of the Slack entrypoint.

Summon path: an allowed ``app_mention`` resolves/creates a per-thread session,
persists the thread as an agent message, and starts a master round via
``trigger_master`` (without awaiting it).

Delivery path: ``deliver_done`` hangs off the round's terminal ``master_done``
event (called from ``SessionManager.persist_and_broadcast``), not off a waiting
coroutine — that is what makes delivery survive a server restart, since a
re-attached round still emits its done through the same funnel.
``backfill_lost_summons`` closes the remaining hole at boot: a summon that was
still queued when the process died is answered by nothing, so it gets a notice
rather than vanishing.
"""

import asyncio
import html
import json
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

_ACCEPTANCE_REPLY = "已收到，正在处理…"

_LOCAL_TZ = ZoneInfo("America/Los_Angeles")

# Answers longer than this go to an artifact file and are posted as a link:
# a Slack message that long is unreadable and gets truncated by the client.
_MAX_POST_CHARS = 3500

# How much of an over-length answer precedes its artifact link.
_LINK_HEAD_CHARS = 400

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
  """Thin Slack Web API wrapper: open_connection / post_message / thread_text."""

  def __init__(self, http: httpx.AsyncClient, *, bot_token: str, app_token: str) -> None:
    self._http = http
    self._bot_headers = {"Authorization": f"Bearer {bot_token}"}
    self._app_headers = {"Authorization": f"Bearer {app_token}"}

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

  async def thread_text(self, channel: str, ts: str) -> list[dict]:
    """Return the raw message dicts for a channel thread via conversations.replies."""
    resp = await self._http.get(
        "https://slack.com/api/conversations.replies",
        headers=self._bot_headers,
        params={"channel": channel, "ts": ts},
    )
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"conversations.replies failed: {payload}")
    return payload.get("messages", [])


def _build_prompt(messages: list[dict]) -> str:
  """Render a thread's messages into the prompt body, ending at the citation boundary."""
  lines: list[str] = []
  for msg in messages:
    text = msg.get("text")
    if text is None:
      continue
    user = msg.get("user")
    sender = f"<@{user}>" if user else "unknown"
    lines.append(f"{sender}: {text}")
  body = "\n".join(lines).strip()
  return f"{body}\n\n{CITATION_BOUNDARY}" if body else CITATION_BOUNDARY


def _local_time() -> str:
  """Local wall-clock stamp for a session display name."""
  return datetime.now(_LOCAL_TZ).strftime("%Y-%m-%d %H:%M")


async def handle_app_mention(event: dict, cfg, session_mgr, client: SlackClient) -> Optional[str]:
  """Accept or drop one app_mention. Returns the session id when accepted, else None."""
  channel_id = event.get("channel")
  thread_ts = event.get("thread_ts") or event.get("ts")
  slack_user = event.get("user")

  if event.get("type") != "app_mention" or slack_user not in cfg.slack_allowed_user_ids:
    logger.debug("slack_mention_dropped", channel=channel_id, thread_ts=thread_ts, slack_user=slack_user)
    return None

  team_id = event.get("team") or event.get("team_id")
  sid = summon_session_id(team_id, channel_id, thread_ts)

  session_meta = await session_mgr.get_session(sid)
  if session_meta is None:
    session_name = f"Slack #{channel_id} {_local_time()}"
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

  await client.post_message(channel_id, _ACCEPTANCE_REPLY, thread_ts=thread_ts)

  messages = await client.thread_text(channel_id, thread_ts)
  content = _build_prompt(messages)

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
  """Assistant text between the previous master_done (exclusive) and the one at *done_idx*.

  Slicing from the summon instead would sweep in a previous round's answer when
  the summon queued behind another round.
  """
  start = 0
  for idx in range(done_idx - 1, -1, -1):
    if events[idx].get("type") == ET.MASTER_DONE:
      start = idx + 1
      break
  return "".join(
      extract_text_from_message(ev.get("message")) for ev in events[start:done_idx]
      if ev.get("type") == ET.ASSISTANT)


def _artifact_html(text: str, session_id: str) -> str:
  """A minimal standalone page carrying one over-length answer."""
  return ("<!DOCTYPE html>\n"
          "<html>\n"
          "<head><meta charset=\"utf-8\">"
          f"<title>CharlieBot reply · {html.escape(session_id)}</title></head>\n"
          "<body style=\"font-family: system-ui, sans-serif; margin: 2em; max-width: 48em;\">\n"
          f"<pre style=\"white-space: pre-wrap; word-wrap: break-word;\">{html.escape(text)}</pre>\n"
          "</body>\n"
          "</html>\n")


def _write_artifact(cfg: CharlieBotConfig, session_id: str, thread_ts: str, text: str) -> Path:
  """Write the over-length answer as a standalone page and return its absolute path."""
  path = cfg.sessions_dir / session_id / "artifacts" / f"slack_reply_{thread_ts}.html"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(_artifact_html(text, session_id), encoding="utf-8")
  return path.resolve()


async def _link_post(cfg: CharlieBotConfig, session_id: str, thread_ts: str, text: str) -> str:
  """The post body for an over-length answer: a short head plus the artifact link.

  Without ``public_base_url`` no link can be built for a Slack reader
  (``server_base_url`` is localhost), so the path stops with a notice naming the
  missing setting rather than silently degrading — and writes no artifact whose
  link nobody could follow.
  """
  if not cfg.public_base_url:
    logger.error("slack_delivery_link_unbuildable", session=session_id, thread_ts=thread_ts,
                 chars=len(text), missing="public_base_url")
    return (f"回复过长（{len(text)} 字符），无法直接发到 Slack；"
            "本机未配置 public_base_url，链接生成不了，请到会话里查看完整回复。")
  path = await asyncio.to_thread(_write_artifact, cfg, session_id, thread_ts, text)
  url = f"{cfg.public_base_url.rstrip('/')}/absolute_filepath/{str(path).lstrip('/')}"
  return f"{text[:_LINK_HEAD_CHARS].rstrip()}…\n\n完整回复：{url}"


async def deliver_done(session_id: str, done: dict, cfg: CharlieBotConfig,
                       session_mgr: SessionManager) -> bool:
  """Post one finished round's answer back to the Slack thread it was summoned from.

  Called as a fire-and-forget task from ``persist_and_broadcast`` for every
  ``master_done``; returns False without posting unless the round belongs to a
  Slack injection that no earlier done already answered. Returns True when a
  message reached Slack.
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
    body = f"这一轮没有产生任何回复内容（exit_code={done.get('exit_code')}），请到会话里查看详情。"
  elif len(text) > _MAX_POST_CHARS:
    body = await _link_post(cfg, session_id, thread_ts, text)
  else:
    body = text

  posted = await _post_with_retry(
      _bot_client(cfg), target["channel_id"], thread_ts, body, session_id=session_id)
  logger.info("slack_delivery_done", session=session_id, channel=target["channel_id"],
              thread_ts=thread_ts, input_event_id=input_event_id, chars=len(text), posted=posted)
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