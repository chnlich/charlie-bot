"""Slack Socket Mode listener — both halves of the Slack entrypoint.

Summon path: an allowed ``app_mention`` resolves/creates a per-thread session,
persists the thread permalink as an agent message, and starts a master round
via ``trigger_master`` (without awaiting it).

Reply path: the master posts to its session's thread itself, through
``charliebot slack reply`` -> ``POST /api/internal/slack/reply`` -> ``post_reply``
here, and reads the outcome back in the same call. The posted text is persisted
as a ``slack_reply`` event whose ``answers`` names the summon the running round
was answering (None for a round no summon started); a reply that answers a
summon clears the summon's eyes ack.

Round-end audit: ``deliver_done`` hangs off the round's terminal ``master_done``
event (called from ``SessionManager.persist_and_broadcast``), not off a waiting
coroutine, so it survives a server restart. A summon round that ended without
a reply wakes the master once through a nudge event (the summon's ``slack``
block plus ``nudge_of``); a nudge round that still posted nothing gets a
one-line notice in the thread. Every predicate reads the event log, so a
replayed done is a no-op. ``backfill_lost_summons`` runs the same audit over
every finished round at boot, after reporting the summons that were still
queued when the process died.
The eyes ack reaction tracks the open question: lit at the summon, cleared when
a reply answering it lands, or when the notice or the lost-summon report closes it.

Thread follow: after the first summon, eligible thread messages (human, allowed,
newer than the session's ``slack_watermark_ts``) arriving over the same Socket
Mode connection — or found by the reconnect backfill — arm one persisted
per-session trigger whose wake label names the chain's floor ts and the thread
link. The reply path is gated on freshness: ``assert_thread_fresh`` refuses with
412 until every eligible message is acked (``ack_messages`` advances the
watermark); silence stays a legal round outcome because trigger wakes enter the
log as scheduled-trigger events with no slack block, outside the audit.
"""

import asyncio
import json
import re
import uuid
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo

import httpx
import structlog
import websockets
from websockets.asyncio.client import ClientConnection

from src.api.message_utils import build_agent_message_event
from src.core import event_types as ET
from src.core.config import HOUSE_TIMEZONE, CharlieBotConfig
from src.core.http import get_http_client
from src.core.master_trigger import trigger_master
from src.core.models import (
  CreateSessionRequest,
  PendingTrigger,
  SessionMetadata,
  SessionStatus,
  SlackOrigin,
  TriggerStatus,
  utc_now,
)
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task
from src.core.triggers import TriggerManager

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

_LOCAL_TZ = ZoneInfo(HOUSE_TIMEZONE)

# Slack's hard per-message text limit. 40000 is the ceiling, so a long single
# message is left for the client to collapse; splitting is the above-limit
# fallback only.
_MAX_POST_CHARS = 40000

# The reply budget the format contract states (prompts/slack_reply_format.md):
# replies should stay under this many chars, with the depth going to a page.
# This is the single measurement point for that budget.
_REPLY_BUDGET_CHARS = 500

# The command the reply-format contract (prompts/slack_reply_format.md) names
# for posting a reply. A summon prompt embeds that contract, so a summon whose
# content names the command was issued under it; the round-end audit enforces
# only that contract and leaves rounds issued under the earlier one alone.
_REPLY_COMMAND = "charliebot slack reply"

# Waits between the retries of one Slack call; the answer stays readable in the
# session log either way, so exhausting them logs an error rather than raising.
_RETRY_DELAYS = (1.0, 4.0)

_LOST_SUMMON_NOTICE = "上一次召唤在服务重启时丢失了，没有被处理。需要的话请重新 @ 我一次。"

_LOST_SUMMON_CONTENT = (
    "这条 Slack 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。")

# Nudge event content: the summon round ended without a reply, so the master is
# asked once whether the thread should hear something.
_NUDGE_TEMPLATE = (
    "Slack thread {link}: the round answering this mention ended without posting a reply\n"
    "(no `charliebot slack reply` call). Decide now: when the thread should hear something, post it with\n"
    "`charliebot slack reply --file <path>`; when there is nothing to say, end this round and the thread\n"
    "gets a one-line notice pointing to this session.")

# Thread-visible end state after a summon round and its nudge round both posted nothing.
_NO_REPLY_NOTICE = ("No reply was posted for this mention; the details are in the session log. "
                    "Mention me again for a thread answer.")

# Session-log content of the notice marker; its ``slack_notice`` payload names the summon it closes.
_NO_REPLY_CONTENT = "This Slack mention got no reply from its round or the nudge round; the thread was told so."

# Thread-follow windows: a batch sleeps out this quiet delay from the newest
# message, and a chain never runs past this cap from its first message, so a
# steady trickle still flushes.
_FOLLOW_QUIET_SECONDS = 45
_FOLLOW_CHAIN_CAP_SECONDS = 300

# Trigger-label prefix identifying a session's armed thread-follow record.
_FOLLOW_TRIGGER_PREFIX = "slack-thread-follow"

# The chain floor carried on every follow label; re-arms parse it back.
_FOLLOW_FLOOR_RE = re.compile(r"floor=([0-9.]+)")

# Wire type of the persisted ack audit record. It mirrors the ET constants but
# stays local: the ack record is a pure audit trail, consumed by nothing else.
_ACK_EVENT_TYPE = "slack_ack"

# How much of an unread message's text the 412 refusal and the gate list carry.
_TEXT_PREVIEW_CHARS = 200


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

  @staticmethod
  def _checked_payload(resp: httpx.Response, method: str) -> dict[str, Any]:
    """Slack Web API envelope rule for the raise-on-failure methods: HTTP errors
    raise through httpx; an ok=false payload raises RuntimeError naming the
    Slack method. get_channel_name folds failures into its None cache instead,
    and remove_reaction adds its no_reaction exemption on top.
    """
    resp.raise_for_status()
    payload = resp.json()
    if not payload.get("ok"):
      raise RuntimeError(f"{method} failed: {payload}")
    return payload

  async def open_connection(self) -> str:
    """POST apps.connections.open and return the wss: socket url."""
    resp = await self._http.post(
        "https://slack.com/api/apps.connections.open", headers=self._app_headers)
    return self._checked_payload(resp, "apps.connections.open")["url"]

  async def post_message(self, channel: str, text: str, thread_ts: str | None = None) -> dict:
    """POST chat.postMessage; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "text": text}
    if thread_ts is not None:
      body["thread_ts"] = thread_ts
    resp = await self._http.post(
        "https://slack.com/api/chat.postMessage", headers=self._bot_headers, json=body)
    return self._checked_payload(resp, "chat.postMessage")

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Add one emoji reaction to a message; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "name": name, "timestamp": ts}
    resp = await self._http.post(
        "https://slack.com/api/reactions.add", headers=self._bot_headers, json=body)
    return self._checked_payload(resp, "reactions.add")

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
    return self._checked_payload(resp, "chat.getPermalink")["permalink"]

  async def get_thread_replies(self, channel: str, thread_ts: str) -> list[dict]:
    """GET conversations.replies for one thread; return its messages in order."""
    resp = await self._http.get(
        "https://slack.com/api/conversations.replies",
        headers=self._bot_headers,
        params={"channel": channel, "ts": thread_ts},
    )
    return self._checked_payload(resp, "conversations.replies")["messages"]

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


async def handle_app_mention(
    event: dict,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: SlackClient,
    trigger_mgr: TriggerManager | None = None,
) -> str | None:
  """Accept or drop one app_mention. Returns the session id when accepted, else None.

  The summon's channel label is resolved exactly once here, before
  create_session, and reused for both the session name and the group: a single
  ``Slack #<channel_name>`` label keeps the two display fields in lockstep and
  the resolution count to one lookup per accepted mention. A name that cannot
  be resolved falls back to the channel id.

  A new-round watermark step runs on every accepted mention: the watermark
  advances to the mention ts (the mention round consumes it), and any armed
  thread-follow trigger is cancelled — both delivery orders of an @ dedup
  (app_mention + message events for the same mention) end clean. When the
  caller passes no trigger manager, an in-process one is constructed, so
  existing four-argument call sites exercise the identical path.
  """
  trigger_mgr = trigger_mgr or TriggerManager(cfg, session_mgr)
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

  await _consume_mention(session_mgr, trigger_mgr, sid, event["ts"])

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
# Thread follow: later thread messages feed the same session
# ---------------------------------------------------------------------------


def _eligible_thread_message(message: dict, allowed_user_ids: list[str]) -> bool:
  """The shared thread-eligibility rule for guards, the gate, and ack.

  A message is eligible when it is a plain (subtype-absent) human-authored
  message from an allowed user. Gate eligibility equals guard eligibility, so
  nothing is demanded of an ack that the session would never consume.
  """
  return (message.get("subtype") is None and message.get("bot_id") is None
          and message.get("user") in allowed_user_ids)


def _unread_eligible(messages: list[dict], allowed_user_ids: list[str], watermark_ts: str | None) -> list[dict]:
  """The eligible messages strictly above *watermark_ts*; a None watermark passes everything."""
  return [
      m for m in messages
      if _eligible_thread_message(m, allowed_user_ids) and (watermark_ts is None or m["ts"] > watermark_ts)
  ]


async def _fetch_unread_eligible(
    client: SlackClient, origin: SlackOrigin, cfg: CharlieBotConfig, watermark_ts: str | None) -> list[dict]:
  """Read the session's thread once and return its unread eligible messages."""
  messages = await client.get_thread_replies(origin.channel_id, origin.thread_ts)
  return _unread_eligible(messages, cfg.slack_allowed_user_ids, watermark_ts)


async def _armed_follow_triggers(trigger_mgr: TriggerManager, session_id: str) -> list[PendingTrigger]:
  """The session's pending thread-follow trigger records (at most one by construction)."""
  return [
      t for t in await trigger_mgr.list_triggers(session_id)
      if t.status == TriggerStatus.PENDING and t.message.startswith(_FOLLOW_TRIGGER_PREFIX)
  ]


def _build_follow_wake_message(floor_ts: str, permalink: str) -> str:
  """The armed follow trigger's label: the chain floor ts, the thread link, and the wake contract.

  ``floor=<ts>`` on the first line is machine-readable: a re-arm parses it back
  so the wake always reads from the chain's oldest unacked message, independent
  of watermark state.
  """
  return (f"{_FOLLOW_TRIGGER_PREFIX} floor={floor_ts}\n"
          f"Slack 线程跟帖唤醒：{permalink}\n"
          "用 slack 技能从上面 floor 标注的消息读起（conversations.replies，channel 与 thread_ts 从链接解析）；"
          "回复之前从仓库重读 prompts/slack_reply_redline.md 与 prompts/slack_reply_format.md；"
          "读到的消息用 `charliebot slack ack --message-id <ts> [...]` 确认，本轮沉默也要 ack；"
          "只在值得时回复。")


async def _arm_follow_trigger(
    trigger_mgr: TriggerManager,
    session_id: str,
    channel_id: str,
    thread_ts: str,
    permalink: str,
    floor_ts: str,
) -> PendingTrigger:
  """Cancel-then-create the session's one persisted follow trigger; return the fresh record.

  The replaced record's ``created_at`` and floor ts are read BEFORE the cancel:
  the new record is stamped with that same ``created_at`` — the chain's start —
  so a steady trickle still flushes at ``chain_start + _FOLLOW_CHAIN_CAP_SECONDS``
  no matter how many re-arms land, and the label keeps the chain's oldest
  unacked ts as the floor.
  """
  chain_start: datetime | None = None
  for old in await _armed_follow_triggers(trigger_mgr, session_id):
    if chain_start is None:  # exactly one armed record exists by construction
      chain_start = old.created_at
      match = _FOLLOW_FLOOR_RE.search(old.message)
      if match is not None:
        floor_ts = match.group(1)
    await trigger_mgr.cancel_trigger(session_id, old.id)
  now = utc_now()
  start = chain_start or now
  fire_at = min(now + timedelta(seconds=_FOLLOW_QUIET_SECONDS),
                start + timedelta(seconds=_FOLLOW_CHAIN_CAP_SECONDS))
  delay = max(0, int((fire_at - now).total_seconds()))
  trigger = await trigger_mgr.create_trigger(
      session_id, delay, _build_follow_wake_message(floor_ts, permalink), created_at=start)
  logger.info("slack_follow_trigger_armed", session=session_id, channel=channel_id, thread_ts=thread_ts,
              floor_ts=floor_ts, fire_at=trigger.fire_at.isoformat(), chain_start=start.isoformat())
  return trigger


async def _cancel_armed_follow_triggers(trigger_mgr: TriggerManager, session_id: str) -> int:
  """Cancel every armed thread-follow trigger of the session; return how many."""
  armed = await _armed_follow_triggers(trigger_mgr, session_id)
  for trigger in armed:
    await trigger_mgr.cancel_trigger(session_id, trigger.id)
  return len(armed)


async def _consume_mention(
    session_mgr: SessionManager, trigger_mgr: TriggerManager, session_id: str, mention_ts: str) -> None:
  """The mention round consumes its own ts: advance the watermark to it and cancel armed follows."""
  meta = await session_mgr.get_session(session_id)
  if meta is None:  # unreachable from the summon path; the session was just resolved there
    return
  if meta.slack_watermark_ts is None or meta.slack_watermark_ts < mention_ts:
    meta.slack_watermark_ts = mention_ts
    meta.updated_at = utc_now()
    await session_mgr.save_metadata(meta)
  cancelled = await _cancel_armed_follow_triggers(trigger_mgr, session_id)
  if cancelled:
    logger.info("slack_follow_trigger_cancelled_for_mention", session=session_id, cancelled=cancelled)


async def handle_thread_message(
    event: dict,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: SlackClient,
    trigger_mgr: TriggerManager,
) -> str | None:
  """Accept or drop one thread message event; returns the session id when it armed a follow.

  Guard chain, in order — the event is dropped when any check fails:
  (1) subtype absent — edits, deletes, and every other subtype drop;
  (2) the event targets a thread this session follows;
  (3) the sender is human (bot_id drops) and in the allowed list;
  (4) the session exists, is ACTIVE, and its slack_origin matches the channel;
  (5) the event ts is strictly above the session's watermark — None passes.
  A passed event arms (or re-arms) the session's one persisted follow trigger.
  """
  channel_id = event.get("channel")
  thread_ts = event.get("thread_ts")
  ts = event.get("ts")
  slack_user = event.get("user")
  if event.get("subtype") is not None:
    return None
  if thread_ts is None:
    return None  # channel-top-level messages are no thread's follow traffic
  if event.get("bot_id") is not None or slack_user not in cfg.slack_allowed_user_ids:
    return None
  team_id = event.get("team") or event.get("team_id")
  sid = summon_session_id(team_id, channel_id, thread_ts)
  meta = await session_mgr.get_session(sid)
  if (meta is None or meta.status != SessionStatus.ACTIVE or meta.slack_origin is None
      or meta.slack_origin.channel_id != channel_id):
    return None
  if meta.slack_watermark_ts is not None and not (ts > meta.slack_watermark_ts):
    return None
  permalink = await client.get_permalink(channel_id, thread_ts)
  await _arm_follow_trigger(trigger_mgr, sid, channel_id, thread_ts, permalink, ts)
  return sid


async def _backfill_followed_threads(
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: SlackClient,
    trigger_mgr: TriggerManager,
) -> int:
  """Arm the follow trigger of every ACTIVE Slack session holding unread messages; return the count.

  Runs once per successful Socket Mode (re)connection: one conversations.replies
  read per followed thread closes the socket-down window, which persisted
  triggers cannot cover (no events arrive while the socket is down). A session
  whose thread shows no unread eligible message arms nothing, and each armed
  session arms exactly once, independent of its unread count.
  """
  armed = 0
  for meta in await session_mgr.list_sessions():
    if meta.status != SessionStatus.ACTIVE or meta.slack_origin is None:
      continue
    origin = meta.slack_origin
    try:
      unread = await _fetch_unread_eligible(client, origin, cfg, meta.slack_watermark_ts)
      if not unread:
        continue
      permalink = await client.get_permalink(origin.channel_id, origin.thread_ts)
      await _arm_follow_trigger(trigger_mgr, meta.id, origin.channel_id, origin.thread_ts, permalink,
                                unread[0]["ts"])
      armed += 1
    except Exception as e:
      logger.warning("slack_follow_backfill_thread_failed", session=meta.id, channel=origin.channel_id,
                     thread_ts=origin.thread_ts, error=str(e))
  if armed:
    logger.info("slack_follow_backfill_armed", sessions=armed)
  return armed


# ---------------------------------------------------------------------------
# Shared Slack plumbing
# ---------------------------------------------------------------------------


def _bot_client(cfg: CharlieBotConfig) -> SlackClient:
  """The client every outbound path (reply, notice, backfill) posts through."""
  return SlackClient(get_http_client(), bot_token=cfg.slack_bot_token, app_token=cfg.slack_app_token)


async def _post_with_retry(client: SlackClient, channel: str, thread_ts: str, text: str, *,
                           session_id: str) -> bool:
  """Post one thread reply, retrying on failure; True when Slack accepted it.

  Exhausting the retries logs an error and returns False instead of raising:
  the caller decides what a failed post means (a 502 to the CLI, a notice
  left for the boot audit), and the event funnel is never broken by one.
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
  raise AssertionError("unreachable: the last loop iteration returns (attempt == attempts - 1)")


def _ack_clear(client: SlackClient, slack_block: dict, session_id: str) -> None:
  """Clear the summon eye once its question is closed (reply landed, notice posted, or lost).

  Fires the remove as its own logged task — a failure there only leaves one
  stale eye plus a background_task_failed log and never touches the caller's
  result. Skipped when the persisted slack block carries no mention_ts.
  """
  mention_ts = slack_block.get("mention_ts")
  if mention_ts is None:
    return
  create_logged_task(
      client.remove_reaction(slack_block["channel_id"], _ACCEPTANCE_REACTION, mention_ts),
      name=f"slack-ack-clear-{session_id}")


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


def _event_by_id(events: list[dict], event_id: str) -> dict | None:
  for ev in events:
    if ev.get("id") == event_id:
      return ev
  return None


def _slack_target(events: list[dict], input_event_id: str) -> dict | None:
  """The ``slack`` block of the event a round answers, or None when it has none.

  None covers both "no such event" and "a message the user typed into this same
  session from the browser": neither is a Slack injection, so neither is audited.
  """
  ev = _event_by_id(events, input_event_id)
  return ev.get("slack") if ev is not None else None


def _summon_of(slack_block: dict, event_id: str) -> str:
  """The summon a round with this slack block answers: the block's ``nudge_of`` for a nudge, else the event itself."""
  return slack_block.get("nudge_of") or event_id


# ---------------------------------------------------------------------------
# Reply: the master posts to its own thread
# ---------------------------------------------------------------------------


class SlackReplyError(Exception):
  """A reply the endpoint refuses or Slack did not accept; ``status`` is the HTTP status it maps to.

  ``detail`` becomes the HTTPException detail; the 412 refusal carries the
  structured ``stale_thread`` payload instead of a plain string.
  """

  def __init__(self, status: int, detail: str | dict) -> None:
    super().__init__(str(detail))
    self.status = status
    self.detail = detail


def _bound_summon(events: list[dict], user_event_id: str | None) -> tuple[str, dict] | None:
  """``(summon_id, slack block)`` of the summon the running round answers, or None.

  Only an input event that carries a slack block (a summon or its nudge) binds
  the reply to a summon. A browser-typed message, a trigger wake, or no round
  identity binds nothing.
  """
  if user_event_id is None:
    return None
  ev = _event_by_id(events, user_event_id)
  block = ev.get("slack") if ev is not None else None
  if block is None:
    return None
  return _summon_of(block, ev["id"]), block


async def _require_slack_thread_session(session_id: str, session_mgr: SessionManager) -> SessionMetadata:
  """The session named by *session_id* when it exists and carries a Slack thread.

  The reply-path preamble shared by ``assert_thread_fresh``, ``ack_messages``,
  and ``post_reply``. Refusals raise ``SlackReplyError``: 404 unknown session,
  409 no Slack thread.
  """
  meta = await session_mgr.get_session(session_id)
  if meta is None:
    raise SlackReplyError(404, "Session not found")
  if meta.slack_origin is None:
    raise SlackReplyError(409, "Session has no Slack thread")
  return meta


async def assert_thread_fresh(session_id: str, cfg: CharlieBotConfig,
                              session_mgr: SessionManager) -> None:
  """Refuse the reply when eligible thread messages sit above the session's watermark.

  The reply-path gate, run by the slack/reply endpoint before ``post_reply``:
  a reply to a thread the running round has not acked through would answer a
  stale state, so nothing posts until the ack advances the watermark. Refusals
  raise ``SlackReplyError``: 404 unknown session, 409 no Slack thread, then 412
  with the structured ``stale_thread`` payload naming each unread message's ts,
  user, and a text preview; with a None watermark the whole thread tail counts.
  """
  meta = await _require_slack_thread_session(session_id, session_mgr)
  unread = await _fetch_unread_eligible(_bot_client(cfg), meta.slack_origin, cfg, meta.slack_watermark_ts)
  if not unread:
    return
  raise SlackReplyError(412, {
      "error": "stale_thread",
      "new_messages": [{
          "ts": m["ts"],
          "user": m.get("user"),
          "text_preview": (m.get("text") or "")[:_TEXT_PREVIEW_CHARS],
      } for m in unread],
      "watermark_ts": meta.slack_watermark_ts,
  })


async def ack_messages(session_id: str, message_ids: list[str], cfg: CharlieBotConfig,
                       session_mgr: SessionManager) -> dict:
  """Advance the session's read watermark over *message_ids*; return the readback the CLI prints.

  The follow round's proof-of-read. Refusals raise ``SlackReplyError``: 404
  unknown session, 409 no Slack thread, 422 an empty set, an unknown or
  ineligible id, or an eligible unread id at or below ``max(message_ids)``
  missing from the set (named) — nothing unread may be jumped over and
  nothing is persisted on a refusal; the natural batch is the gate refusal's
  own list. On success the watermark advances to ``max(message_ids)``, a
  small ack event lands in the session log for the audit trail, and re-acking
  ids at or below the watermark is an idempotent no-op counted as acked.
  """
  meta = await _require_slack_thread_session(session_id, session_mgr)
  ids = sorted(set(message_ids))
  if not ids:
    raise SlackReplyError(422, "message_ids is empty")
  origin = meta.slack_origin
  messages = await _bot_client(cfg).get_thread_replies(origin.channel_id, origin.thread_ts)
  eligible = {m["ts"] for m in messages if _eligible_thread_message(m, cfg.slack_allowed_user_ids)}
  unknown = [ts for ts in ids if ts not in eligible]
  if unknown:
    raise SlackReplyError(422, f"Unknown or ineligible message id: {unknown[0]}")
  watermark = meta.slack_watermark_ts
  ceiling = ids[-1]
  skipped = [
      ts for ts in sorted(eligible) if (watermark is None or ts > watermark) and ts <= ceiling and ts not in ids
  ]
  if skipped:
    raise SlackReplyError(422, f"Skipped eligible message id at or below {ceiling}: {skipped[0]}")
  watermark = meta.slack_watermark_ts
  if watermark is None or ceiling > watermark:
    watermark = ceiling
    meta.slack_watermark_ts = watermark
    meta.updated_at = utc_now()
    await session_mgr.save_metadata(meta)
  await session_mgr.persist_and_broadcast(session_id, {
      "type": _ACK_EVENT_TYPE,
      "content": f"Slack thread ack: {len(ids)} message(s) read through {ceiling}",
      "slack_ack": {"message_ids": ids, "watermark_ts": watermark},
  })
  logger.info("slack_thread_acked", session=session_id, acked=len(ids), watermark_ts=watermark)
  return {"acked": len(ids), "watermark_ts": watermark}


async def post_reply(session_id: str, text: str, cfg: CharlieBotConfig,
                     session_mgr: SessionManager) -> dict:
  """Post *text* to the session's Slack thread and return the readback the CLI prints.

  Refusals raise ``SlackReplyError``: 404 unknown session, 409 no Slack thread,
  422 blank text, 502 when a chunk exhausted its retries (nothing is persisted
  then, so the caller can retry). The endpoint runs ``assert_thread_fresh``
  first, so a stale thread (412) never reaches this function. On success the
  ``slack_reply`` event records
  the text, the summon it answers, and the chunk count; a reply that answers a
  summon clears that summon's eyes.
  """
  meta = await _require_slack_thread_session(session_id, session_mgr)
  if not text.strip():
    raise SlackReplyError(422, "Reply text is empty")

  # lazy: mirrors the backfill import's agents-package guard
  from src.agents import master_cc_state

  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  # Binding identity: the in-process running round first (authoritative, no
  # metadata-cache race), the disk record as fallback for the restart gap where
  # an orphaned master posts before the re-attach item reaches the consumer.
  user_event_id = master_cc_state.running_user_event_id(session_id)
  if user_event_id is None:
    fresh = await session_mgr.read_metadata_fresh(session_id)
    if fresh is not None and fresh.master_run is not None:
      user_event_id = fresh.master_run.user_event_id
  bound = _bound_summon(events, user_event_id)
  answers = bound[0] if bound is not None else None
  origin = meta.slack_origin
  client = _bot_client(cfg)
  bodies = _chunk_text(text)
  for index, body in enumerate(bodies, start=1):
    ok = await _post_with_retry(client, origin.channel_id, origin.thread_ts, body, session_id=session_id)
    if not ok:
      raise SlackReplyError(
          502, f"Slack did not accept chunk {index} of {len(bodies)} after {len(_RETRY_DELAYS) + 1} "
          "attempts; nothing was persisted")

  await session_mgr.persist_and_broadcast(session_id, {
      "type": ET.SLACK_REPLY,
      "content": text,
      "slack_reply": {"answers": answers, "chars": len(text), "chunks": len(bodies)},
  })
  if bound is not None:
    _ack_clear(client, bound[1], session_id)
  over_budget = len(text) > _REPLY_BUDGET_CHARS
  logger.info("slack_reply_posted", session=session_id, channel=origin.channel_id,
              thread_ts=origin.thread_ts, chars=len(text), chunks=len(bodies), over_budget=over_budget,
              budget=_REPLY_BUDGET_CHARS, answers=answers)
  return {"posted": True, "chars": len(text), "chunks": len(bodies), "over_budget": over_budget,
          "answers": answers}


# ---------------------------------------------------------------------------
# Round-end audit
# ---------------------------------------------------------------------------


def _replied(events: list[dict], summon_id: str) -> bool:
  return any(
      ev.get("type") == ET.SLACK_REPLY and (ev.get("slack_reply") or {}).get("answers") == summon_id
      for ev in events)


def _nudged(events: list[dict], summon_id: str) -> bool:
  return any(
      ev.get("type") == ET.AGENT_MESSAGE and (ev.get("slack") or {}).get("nudge_of") == summon_id
      for ev in events)


def _noticed(events: list[dict], summon_id: str) -> bool:
  return any((ev.get("slack_notice") or {}).get("input_event_id") == summon_id for ev in events)


def _thread_link(summon: dict | None, slack_block: dict) -> str:
  """The thread permalink as the summon prompt states it; channel and thread ids when it has none."""
  match = re.search(r"https?://\S+", (summon or {}).get("content") or "")
  if match is not None:
    return match.group(0)
  return f"(channel {slack_block['channel_id']}, thread {slack_block['thread_ts']})"


async def _audit_round(session_id: str, events: list[dict], target: dict, input_event_id: str,
                       cfg: CharlieBotConfig, session_mgr: SessionManager, client: SlackClient) -> bool:
  """Act on one finished round whose input carried a slack block; True when it acted.

  Reads the log for the round's summon: a reply answering it ends the audit. A
  summon round without one gets a nudge (once: a second done for the same
  summon finds the nudge event); a nudge round without one gets the thread
  notice (once: the ``slack_notice`` marker, persisted only after the post
  succeeded, so a failed post leaves the boot audit a retry). A summon issued
  under the marker contract (its prompt names no reply command) is outside
  this audit.
  """
  summon_id = _summon_of(target, input_event_id)
  summon = _event_by_id(events, summon_id)
  if _REPLY_COMMAND not in ((summon or {}).get("content") or ""):
    return False
  if _replied(events, summon_id):
    return False

  if "nudge_of" not in target:
    if _nudged(events, summon_id):
      return False
    content = _NUDGE_TEMPLATE.format(link=_thread_link(summon, target))
    nudge = build_agent_message_event(content, from_session=session_id, from_session_name="Slack")
    nudge["slack"] = {key: target[key] for key in ("channel_id", "thread_ts", "mention_ts") if key in target}
    nudge["slack"]["nudge_of"] = summon_id
    await session_mgr.persist_and_broadcast(session_id, nudge)
    create_logged_task(
        trigger_master(session_id, content, cfg, session_mgr, user_event_id=nudge["id"]),
        name=f"slack-nudge-{session_id}")
    logger.info("slack_reply_nudge", session=session_id, channel=target["channel_id"],
                thread_ts=target["thread_ts"], summon_id=summon_id, nudge_id=nudge["id"])
    return True

  if _noticed(events, summon_id):
    return False
  ok = await _post_with_retry(
      client, target["channel_id"], target["thread_ts"], _NO_REPLY_NOTICE, session_id=session_id)
  if not ok:
    return False  # slack_post_gave_up is logged; no marker, so the boot audit posts it later
  await session_mgr.persist_and_broadcast(session_id, {
      "type": ET.ASSISTANT_ERROR,
      "content": _NO_REPLY_CONTENT,
      "slack_notice": {"input_event_id": summon_id},
  })
  _ack_clear(client, target, session_id)
  logger.info("slack_reply_notice", session=session_id, channel=target["channel_id"],
              thread_ts=target["thread_ts"], summon_id=summon_id)
  return True


async def deliver_done(session_id: str, done: dict, cfg: CharlieBotConfig,
                       session_mgr: SessionManager) -> bool:
  """Round-end audit for one finished round; True when it nudged or posted the notice.

  Called as a fire-and-forget task from ``persist_and_broadcast`` for every
  ``master_done``; returns False without acting unless the round belongs to a
  Slack session and answered a summon or a nudge. Guard-path dones
  (src/api/chat.py) carry no input_event_id and browser-typed rounds carry no
  slack block, so both leave the thread alone.
  """
  meta = await session_mgr.get_session(session_id)
  if meta is None or meta.slack_origin is None:
    return False
  input_event_id = done.get("input_event_id")
  if input_event_id is None:
    return False
  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  target = _slack_target(events, input_event_id)
  if target is None:
    return False
  return await _audit_round(session_id, events, target, input_event_id, cfg, session_mgr, _bot_client(cfg))


# ---------------------------------------------------------------------------
# Boot backfill
# ---------------------------------------------------------------------------


def _lost_summons(events: list[dict], *, owned: set[str], running: str | None) -> list[dict]:
  """The Slack injections in one session's log that nothing will ever answer.

  A summon (or a nudge: it carries the same slack block) is lost when no
  master_done names it, this process does not already own it (queued or
  running), the session's master_run record does not name it (alive but not
  followable), and no earlier backfill marked it. The marker is the
  ``slack_backfill`` payload, never a synthetic master_done: that event is
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
  """Boot pass over every Slack session; returns how many notices and nudges it produced.

  First the summons lost while queued: the startup replay covers ``ET.USER``
  only (src/core/init.py), so a Slack injection sitting in the queue when the
  process died is picked up by nothing else and gets the lost-summon notice.
  Then the round-end audit over every finished round, which closes the crash
  windows between a done and its nudge, and between a nudge round's done and
  its notice. Every predicate reads the log, so a second pass finds nothing.
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
    if lost:
      events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)

    dones = [ev for ev in events if ev.get("type") == ET.MASTER_DONE and ev.get("input_event_id")]
    for done in dones:
      target = _slack_target(events, done["input_event_id"])
      if target is None:
        continue
      if await _audit_round(meta.id, events, target, done["input_event_id"], cfg, session_mgr, client):
        reported += 1
        # The action appended an event the next done's predicates must see.
        events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)
  return reported


async def _expect_hello(ws: ClientConnection) -> None:
  """Consume the Socket Mode connection's ``hello`` frame."""
  raw = await ws.recv()
  envelope = json.loads(raw)
  if envelope.get("type") != "hello":
    logger.warning("slack_listener_expected_hello", received=envelope.get("type"))


async def run_listener(cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
  """Socket Mode connect/receive/reconnect loop; never returns."""
  http = get_http_client()
  client = SlackClient(http, bot_token=cfg.slack_bot_token, app_token=cfg.slack_app_token)
  trigger_mgr = TriggerManager(cfg, session_mgr)
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
        await _backfill_followed_threads(cfg, session_mgr, client, trigger_mgr)
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
              sid = await handle_app_mention(inner, cfg, session_mgr, client, trigger_mgr)
              logger.info("slack_listener_app_mention_handled", channel=channel, thread_ts=thread_ts,
                          slack_user=slack_user, session=sid)
            except Exception as e:
              logger.exception("slack_listener_app_mention_handle_failed", channel=channel,
                               thread_ts=thread_ts, slack_user=slack_user, error=str(e))
          elif inner and inner.get("type") == "message":
            try:
              await handle_thread_message(inner, cfg, session_mgr, client, trigger_mgr)
            except Exception as e:
              logger.exception("slack_listener_message_handle_failed", channel=inner.get("channel"),
                               thread_ts=inner.get("thread_ts"), slack_user=inner.get("user"), error=str(e))
    except Exception as e:
      logger.warning("slack_listener_connection_dropped", error=str(e))

    await asyncio.sleep(backoff)
    backoff = min(backoff * 2, 30.0)
