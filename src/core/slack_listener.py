"""Slack Socket Mode listener — turns an app_mention from an allowed user into a running master round.

This is the summon path of the Slack entrypoint. Delivery of replies to Slack is
a separate concern; this module ends at "the round is running": it accepts an
allowed mention, resolves/creates a per-thread session, persists the thread as
an agent message, and starts the master round via ``trigger_master`` (without
awaiting it).
"""

import asyncio
import json
import uuid
from datetime import datetime
from typing import Any, Optional
from zoneinfo import ZoneInfo

import httpx
import structlog
import websockets

from src.api.message_utils import build_agent_message_event
from src.core.config import CharlieBotConfig
from src.core.http import get_http_client
from src.core.master_trigger import trigger_master
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