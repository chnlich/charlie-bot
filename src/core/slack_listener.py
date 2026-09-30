"""Slack Socket Mode listener — the Slack half of the shared-thread entrypoint.

This module holds the Slack-specific pieces: the event parsing (the
``app_mention`` and thread ``message`` handlers that read one Socket Mode
event, apply Slack's drop rules, and hand the rest to the shared core), the
``SlackClient`` Web API wrapper, the Socket Mode connect/receive/reconnect
loop (``run_listener``), and the ``SlackThreadAdapter`` over the bot client.
Everything platform-neutral lives in ``src.core.thread_entry``: the summon
acceptance (``accept_summon`` — session create/unarchive/reuse, the watermark
step, the group, the summon event, the round and ack tasks), the mention
consumption, the group assignment, the follow triggers, the thread-message
follow, the reconnect backfill, and the round side (``post_reply``,
``assert_thread_fresh``, ``ack_messages``, ``deliver_done``,
``backfill_lost_summons``). This module describes Slack to the core with the
``SLACK`` platform and keeps the public Slack-named wrappers (``post_reply``,
``assert_thread_fresh``, ``ack_messages``, ``deliver_done``,
``backfill_lost_summons``, the summon and thread-message handlers) that the
server endpoint, the session manager, and the tests import. The master posts
to its session's thread itself, through ``charliebot slack reply`` ->
``POST /api/internal/slack/reply`` -> the ``post_reply`` wrapper, and reads the
outcome back in the same call; before any chunk posts, the reply path publishes
every file-server artifact the text links and swaps the URLs to the published
ones. The posted text is persisted as a ``slack_reply`` event whose ``answers``
names the summon the running round was answering (None for a round no summon
started). ``deliver_done`` hangs off the round's terminal ``master_done`` event
(called from ``SessionManager.persist_and_broadcast``), not off a waiting
coroutine, so it survives a server restart. The eyes ack reaction tracks the
open question: lit at the summon (the shared accept path's ack task), cleared
when a reply answering it lands, or when the notice or the lost-summon report
closes it.

Thread follow: after the first summon, eligible thread messages (human, allowed,
newer than the session's ``slack_watermark_ts``) arriving over the same Socket
Mode connection — or found by the reconnect backfill on every (re)connection —
arm one persisted per-session trigger whose wake label names the chain's floor
ts and the thread link. The reply path is gated on freshness:
``assert_thread_fresh`` refuses with 412 until every eligible message is acked
(``ack_messages`` advances the watermark); silence stays a legal round outcome
because trigger wakes enter the log as scheduled-trigger events with no slack
block, outside the audit.
"""

import asyncio
import json
import uuid
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any

from src.core import event_types as ET
from src.core import thread_entry, timeouts
from src.core.config import CharlieBotConfig, get_credentials
from src.core.http import get_http_client
from src.core.log_once import LazyStructlogLogger
from src.core.models import PendingTrigger, SlackOrigin
from src.core.publish import PublishError, publish_artifact
from src.core.sessions import SessionManager

# _NO_REPLY_NOTICE keeps its importable Slack name for the delivery tests.
from src.core.thread_entry import _NO_REPLY_NOTICE as _NO_REPLY_NOTICE

from src.core.thread_entry import (
    ThreadAdapter,
    ThreadMessage,
    ThreadPlatform,
    ThreadReplyError,
    lost_summons,
    summon_prompt_tail,
)
from src.core.triggers import TriggerManager

if TYPE_CHECKING:
  import httpx
  from websockets.asyncio.client import ClientConnection

logger = LazyStructlogLogger()

# Fixed namespace UUID for Slack summon session ids. Arbitrary but stable
# across process restarts; changing it would orphan every existing Slack-backed
# session.
SLACK_NS = uuid.UUID("1b4e28ba-2fa1-4d7a-9f0c-8d5e7a3b6c11")

_ACCEPTANCE_REACTION = "eyes"

# Slack's hard per-message text limit. 40000 is the ceiling, so a long single
# message is left for the client to collapse; splitting is the above-limit
# fallback only.
_MAX_POST_CHARS = 40000

# The command the reply-format contract (prompts/thread_reply_format.md) names
# for posting a reply. A summon prompt embeds that contract, so a summon whose
# content names the command was issued under it; the round-end audit enforces
# only that contract and leaves rounds issued under the earlier one alone.
_REPLY_COMMAND = "charliebot slack reply"

# The summon prompt's platform line. The shared reply-format contract
# (prompts/thread_reply_format.md) defers the platform-specific facts to
# this line: platform name, reply command, per-message limit, and how
# linked pages reach readers. Another platform's entrypoint states its own
# line and reuses the contract unchanged.
_PLATFORM_LINE = (
    f"Platform: Slack. Reply command: `{_REPLY_COMMAND} --file <path>`. "
    f"Per-message limit: {_MAX_POST_CHARS} characters. "
    "Linked pages: the reply path publishes each linked file-server page and swaps in its published URL.")

# Trigger-label prefix identifying a session's armed thread-follow record.
_FOLLOW_TRIGGER_PREFIX = "slack-thread-follow"

# The platform description every shared thread helper takes; each value keeps
# its one home in the constants above.
SLACK = ThreadPlatform(
    name="slack",
    display_name="Slack",
    reply_event_type=ET.SLACK_REPLY,
    reply_command=_REPLY_COMMAND,
    max_post_chars=_MAX_POST_CHARS,
    scope_doc="slack_reply_scope.md",
    follow_trigger_prefix=_FOLLOW_TRIGGER_PREFIX,
    id_key=str,
    origin_field="slack_origin",
    watermark_field="slack_watermark_ts",
    id_label="ts",
    mention_key="mention_ts",
    block_keys=("channel_id", "thread_ts", "mention_ts"),
    thread_fallback="(channel {channel_id}, thread {thread_ts})",
    attaches_files=False,
)


def summon_session_id(team_id: str, channel_id: str, thread_ts: str) -> str:
  """Return the deterministic session id for a Slack thread."""
  return str(uuid.uuid5(SLACK_NS, f"slack:{team_id}:{channel_id}:{thread_ts}"))


class SlackClient:
  """Thin Slack Web API wrapper: open_connection / post_message / get_permalink / get_thread_replies /
  add_reaction / remove_reaction / get_channel_name."""

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
    resp = await self._http.post("https://slack.com/api/apps.connections.open", headers=self._app_headers)
    return self._checked_payload(resp, "apps.connections.open")["url"]

  async def post_message(self, channel: str, text: str, thread_ts: str) -> dict:
    """POST chat.postMessage as a thread reply; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "text": text, "thread_ts": thread_ts}
    resp = await self._http.post("https://slack.com/api/chat.postMessage", headers=self._bot_headers, json=body)
    return self._checked_payload(resp, "chat.postMessage")

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Add one emoji reaction to a message; returns the API payload."""
    body: dict[str, Any] = {"channel": channel, "name": name, "timestamp": ts}
    resp = await self._http.post("https://slack.com/api/reactions.add", headers=self._bot_headers, json=body)
    return self._checked_payload(resp, "reactions.add")

  async def remove_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Remove one emoji reaction from a message; returns the API payload.

    A ``no_reaction`` error already is the end state, so the clear is
    idempotent; any other ok=false payload raises.
    """
    body: dict[str, Any] = {"channel": channel, "name": name, "timestamp": ts}
    resp = await self._http.post("https://slack.com/api/reactions.remove", headers=self._bot_headers, json=body)
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
        params={
            "channel": channel,
            "message_ts": ts
        },
    )
    return self._checked_payload(resp, "chat.getPermalink")["permalink"]

  async def get_thread_replies(self, channel: str, thread_ts: str) -> list[dict]:
    """GET conversations.replies for one thread; return its messages in order."""
    resp = await self._http.get(
        "https://slack.com/api/conversations.replies",
        headers=self._bot_headers,
        params={
            "channel": channel,
            "ts": thread_ts
        },
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
        logger.warning("slack_channel_name_unresolved", channel=channel_id, error=payload.get("error"))
    except Exception as e:
      logger.warning("slack_channel_name_resolve_failed", channel=channel_id, error=str(e))
    self._channel_name_cache[channel_id] = name
    return name


def _build_summon_prompt(permalink: str, cfg: CharlieBotConfig) -> str:
  """The persisted summon: the thread link plus a self-fetch hint, ending at the fixed notices.

  Only the permalink is stored because the master reads the thread itself via
  its slack skill when the round runs — a snapshot persisted here would go
  stale as the thread keeps changing after the mention.

  The tail after the platform line (the platform's scope doc, the PII red
  line, the reply-format contract) is the shared one from thread_entry, read
  fresh from prompts/ on every call — no caching, so an edit takes effect on
  the next summon.
  """
  return (
      f"Slack 线程召唤：{permalink}\n\n"
      "用 slack 技能按链接读线程（conversations.replies，channel 与 thread_ts 从链接解析）。\n\n"
      f"{summon_prompt_tail(SLACK, _PLATFORM_LINE, cfg)}")


async def handle_app_mention(
    event: dict,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: SlackClient,
    trigger_mgr: TriggerManager | None = None,
) -> str | None:
  """Accept or drop one app_mention. Returns the session id when accepted, else None.

  The summon's channel label is resolved exactly once here, before the shared
  accept path, and reused for both the session name and the group: a single
  ``Slack #<channel_name>`` label keeps the two display fields in lockstep and
  the resolution count to one lookup per accepted mention. A name that cannot
  be resolved falls back to the channel id.

  A new-round watermark step runs on every accepted mention: the watermark
  advances to the mention ts (the mention round consumes it), and any armed
  thread-follow trigger is cancelled — both delivery orders of an @ dedup
  (app_mention + message events for the same mention) end clean. When the
  caller passes no trigger manager, an in-process one is constructed, so
  existing four-argument call sites exercise the identical path.

  The permalink and the summon block are resolved here — the session lookup in
  the shared accept path reads neither — and the summon prompt is built from
  the permalink before the shared path persists it. The session resolution
  (create with the ``slack_origin``, unarchive, or reuse), the watermark step,
  the group assignment, the summon persistence, and the round and ack tasks
  are the shared core's (``thread_entry.accept_summon``).
  """
  trigger_mgr = trigger_mgr or TriggerManager(cfg, session_mgr)
  channel_id = event.get("channel")
  thread_ts = event.get("thread_ts") or event.get("ts")
  slack_user = event.get("user")

  if event.get("type") != "app_mention" or slack_user not in cfg.slack.allowed_user_ids:
    logger.debug("slack_mention_dropped", channel=channel_id, thread_ts=thread_ts, slack_user=slack_user)
    return None

  team_id = event.get("team") or event.get("team_id")
  sid = summon_session_id(team_id, channel_id, thread_ts)

  name = await client.get_channel_name(channel_id)
  label = f"Slack #{name or channel_id}"

  permalink = await client.get_permalink(channel_id, event["ts"])
  block = {
      "channel_id": channel_id,
      "thread_ts": thread_ts,
      "mention_ts": event.get("ts"),
  }
  return await thread_entry.accept_summon(
      SlackThreadAdapter(client),
      cfg,
      session_mgr,
      trigger_mgr,
      session_id=sid,
      label=label,
      origin=SlackOrigin(team_id=team_id, channel_id=channel_id, thread_ts=thread_ts),
      block=block,
      content=_build_summon_prompt(permalink, cfg),
      user=slack_user,
  )


# ---------------------------------------------------------------------------
# Thread follow: later thread messages feed the same session
# ---------------------------------------------------------------------------


def _eligible_thread_message(message: dict, allowed_user_ids: list[str]) -> bool:
  """The thread-eligibility rule the adapter's eligible readback applies.

  A message is eligible when it is a plain (subtype-absent) human-authored
  message from an allowed user. Gate eligibility equals guard eligibility, so
  nothing is demanded of an ack that the session would never consume; the
  thread-message handler's guard 3 restates the same rule against the raw
  event.
  """
  return (message.get("subtype") is None and message.get("bot_id") is None and message.get("user") in allowed_user_ids)


def _build_follow_wake_message(floor_ts: str, permalink: str) -> str:
  """The armed follow trigger's label: the chain floor ts, the thread link, and the wake contract.

  ``floor=<ts>`` on the first line is machine-readable: a re-arm parses it back
  so the wake always reads from the chain's oldest unacked message, independent
  of watermark state.
  """
  return (
      f"{_FOLLOW_TRIGGER_PREFIX} floor={floor_ts}\n"
      f"Slack 线程跟帖唤醒：{permalink}\n"
      "用 slack 技能从上面 floor 标注的消息读起（conversations.replies，channel 与 thread_ts 从链接解析）；"
      f"回复之前从仓库重读 prompts/{SLACK.scope_doc}、prompts/thread_reply_redline.md 与 "
      "prompts/thread_reply_format.md；"
      "读到的消息用 `charliebot slack ack --message-id <ts> [...]` 确认，本轮沉默也要 ack；"
      f"只在值得时用 `{_REPLY_COMMAND} --file <path>` 回复。")


async def _arm_follow_trigger(
    trigger_mgr: TriggerManager,
    session_id: str,
    channel_id: str,
    thread_ts: str,
    permalink: str,
    floor_ts: str,
) -> PendingTrigger | None:
  """Cancel-then-create the session's one persisted follow trigger; the shared core on the Slack platform.

  Kept as the importable Slack name (the pending-trigger-limit tests call it
  with the channel/thread arguments); the cancel-then-create mechanics, the
  chain-start stamp, and the floor parse-back live in the shared core
  (``thread_entry.arm_follow_trigger``).
  """
  return await thread_entry.arm_follow_trigger(
      SLACK,
      trigger_mgr,
      session_id,
      floor=floor_ts,
      wake_label=lambda floor: _build_follow_wake_message(floor, permalink),
      log_fields={
          "channel": channel_id,
          "thread_ts": thread_ts
      })


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
  Guards 1 to 3 read the raw event here; guards 4 and 5 and the arming itself
  are the shared core's (``thread_entry.follow_message``).
  """
  channel_id = event.get("channel")
  thread_ts = event.get("thread_ts")
  ts = event.get("ts")
  slack_user = event.get("user")
  if event.get("subtype") is not None:
    return None
  if thread_ts is None:
    return None  # channel-top-level messages are no thread's follow traffic
  if event.get("bot_id") is not None or slack_user not in cfg.slack.allowed_user_ids:
    return None
  team_id = event.get("team") or event.get("team_id")
  sid = summon_session_id(team_id, channel_id, thread_ts)
  return await thread_entry.follow_message(
      SlackThreadAdapter(client),
      session_mgr,
      trigger_mgr,
      sid,
      ts,
      origin_matches=lambda origin: origin.channel_id == channel_id,
  )


async def _backfill_followed_threads(
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: SlackClient,
    trigger_mgr: TriggerManager,
) -> int:
  """Arm the follow trigger of every ACTIVE Slack session holding unread messages; return the count.

  One-line pass-through to the shared backfill
  (``thread_entry.backfill_followed_threads``) on the Slack adapter; the
  per-thread read, the arming, and the failure logs live there. Kept as the
  module global ``run_listener`` calls on every (re)connection.
  """
  return await thread_entry.backfill_followed_threads(SlackThreadAdapter(client), cfg, session_mgr, trigger_mgr)


# ---------------------------------------------------------------------------
# Shared Slack plumbing
# ---------------------------------------------------------------------------


def _bot_client() -> SlackClient:
  """The client every outbound path (reply, notice, backfill) posts through."""
  creds = get_credentials()
  return SlackClient(
      get_http_client(),
      bot_token=str(creds.require("slack", "bot_token")),
      app_token=str(creds.require("slack", "app_token")))


class SlackThreadAdapter(ThreadAdapter):
  """The round side's face onto the Slack Web API client.

  Slack never receives files: its link swap publishes the linked pages and
  swaps the URLs to the published ones, so ``post`` takes no attachment and
  ``link_swap`` appends to nothing.
  """

  platform = SLACK

  def __init__(self, client: SlackClient | None = None) -> None:
    self._given = client

  @property
  def _client(self) -> SlackClient:
    """The client this adapter posts through: the given one, or the bot client
    built on the first platform call and reused after. The round-side wrappers
    pass no client, so a host without Slack credentials never builds one for a
    round the shared core refuses before its first platform call.
    ``_bot_client`` resolves as the module global at that moment, so the
    ``SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET`` patches keep working.
    """
    if self._given is None:
      self._given = _bot_client()
    return self._given

  async def post(self, address: dict, text: str, files: Sequence[Path]) -> None:
    await self._client.post_message(address["channel_id"], text, thread_ts=address["thread_ts"])

  async def add_ack(self, block: dict) -> None:
    await self._client.add_reaction(block["channel_id"], _ACCEPTANCE_REACTION, block[self.platform.mention_key])

  async def remove_ack(self, block: dict) -> None:
    await self._client.remove_reaction(block["channel_id"], _ACCEPTANCE_REACTION, block[self.platform.mention_key])

  async def read_eligible(self, origin: SlackOrigin, cfg: CharlieBotConfig) -> list[ThreadMessage]:
    messages = await self._client.get_thread_replies(origin.channel_id, origin.thread_ts)
    return [
        ThreadMessage(m["ts"], m.get("user"),
                      m.get("text") or "") for m in messages if _eligible_thread_message(m, cfg.slack.allowed_user_ids)
    ]

  def address_of(self, origin: SlackOrigin) -> dict:
    return {"channel_id": origin.channel_id, "thread_ts": origin.thread_ts}

  def link_swap(self, cfg: CharlieBotConfig) -> tuple[Callable[[Path], str], list[Path]]:
    return _publish_swap(cfg), []

  def log_fields(self, address: dict) -> dict:
    return {"channel": address["channel_id"], "thread_ts": address["thread_ts"]}

  async def thread_link(self, origin: SlackOrigin) -> str:
    return await self._client.get_permalink(origin.channel_id, origin.thread_ts)

  def follow_wake_message(self, floor: str, link: str) -> str:
    return _build_follow_wake_message(floor, link)


# ---------------------------------------------------------------------------
# Reply: the master posts to its own thread
# ---------------------------------------------------------------------------

# The shared reply refusal lives in src/core/thread_entry.py; the Slack name
# stays for the adapter's callers (the server endpoint and the tests).
SlackReplyError = ThreadReplyError


async def assert_thread_fresh(session_id: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
  """Refuse the reply when eligible thread messages sit above the session's watermark.

  One-line pass-through to the shared gate (``thread_entry.assert_thread_fresh``)
  on the Slack adapter; the refusal shapes live there.
  """
  return await thread_entry.assert_thread_fresh(SlackThreadAdapter(), session_id, cfg, session_mgr)


async def ack_messages(
    session_id: str, message_ids: list[str], cfg: CharlieBotConfig, session_mgr: SessionManager) -> dict:
  """Advance the session's read watermark over *message_ids*; return the readback the CLI prints.

  One-line pass-through to the shared ack (``thread_entry.ack_messages``) on
  the Slack adapter; the refusal shapes, the ack event, and the readback keys
  live there.
  """
  return await thread_entry.ack_messages(SlackThreadAdapter(), session_id, message_ids, cfg, session_mgr)


def _publish_swap(cfg: CharlieBotConfig) -> Callable[[Path], str]:
  """The Slack swap for the shared link rewrite: publish the file, return its published URL.

  An unconfigured publish lane refuses the whole reply with 422 (the
  ``PublishError`` text names the missing key).
  """

  def swap(fs_path: Path) -> str:
    try:
      return publish_artifact(fs_path, cfg).url
    except PublishError as e:
      raise SlackReplyError(422, str(e)) from e

  return swap


async def post_reply(session_id: str, text: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> dict:
  """Post *text* to the session's Slack thread and return the readback the CLI prints.

  One-line pass-through to the shared reply path (``thread_entry.post_reply``)
  on the Slack adapter; the rewrite, chunking, refusals, reply event, and
  readback live there.
  """
  return await thread_entry.post_reply(SlackThreadAdapter(), session_id, text, cfg, session_mgr)


# ---------------------------------------------------------------------------
# Round-end audit
# ---------------------------------------------------------------------------


async def deliver_done(session_id: str, done: dict, cfg: CharlieBotConfig, session_mgr: SessionManager) -> bool:
  """Round-end audit for one finished round; True when it nudged or posted the notice.

  One-line pass-through to the shared audit (``thread_entry.deliver_done``) on
  the Slack adapter; the audit gate, the nudge, and the notice live there.
  """
  return await thread_entry.deliver_done(SlackThreadAdapter(), session_id, done, cfg, session_mgr)


# ---------------------------------------------------------------------------
# Boot backfill
# ---------------------------------------------------------------------------


def _lost_summons(events: list[dict], *, owned: set[str], running: set[str]) -> list[dict]:
  """The lost Slack summons of one session's log; the shared check on the Slack platform.

  Kept as the importable Slack name (tests call it with the owned/running
  keywords); the boot backfill goes through the shared core directly.
  """
  return lost_summons(SLACK, events, owned=owned, running=running)


async def backfill_lost_summons(cfg: CharlieBotConfig, session_mgr: SessionManager) -> int:
  """Boot pass over every Slack session; returns how many notices and nudges it produced.

  One-line pass-through to the shared boot audit
  (``thread_entry.backfill_lost_summons``) on the Slack adapter; the
  lost-summon report and the per-round audit live there.
  """
  return await thread_entry.backfill_lost_summons(SlackThreadAdapter(), cfg, session_mgr)


async def _expect_hello(ws: ClientConnection) -> None:
  """Consume the Socket Mode connection's ``hello`` frame."""
  raw = await ws.recv()
  envelope = json.loads(raw)
  if envelope.get("type") != "hello":
    logger.warning("slack_listener_expected_hello", received=envelope.get("type"))


async def run_listener(cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
  """Socket Mode connect/receive/reconnect loop; never returns."""
  # websockets (~13 ms with its asyncio client) rides first use: the import
  # path never opens the Socket Mode connection, and the M99 server import
  # floor (docs/perf_baseline.md) depends on it staying out of the chain.
  import websockets

  http = get_http_client()
  creds = get_credentials()
  client = SlackClient(
      http, bot_token=str(creds.require("slack", "bot_token")), app_token=str(creds.require("slack", "app_token")))
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
      async with websockets.connect(url, max_size=None, close_timeout=timeouts.WS_CLIENT_CLOSE_TIMEOUT) as ws:
        try:
          await _serve_socket_mode(ws, cfg, session_mgr, client, trigger_mgr)
        finally:
          # Slack's Socket Mode endpoint never answers a client close frame, so
          # the context manager's graceful close would wait out close_timeout
          # on every session exit — each server stop and each refresh
          # reconnect. Aborting the transport fires the connection-lost waiter
          # that close's wait polls, ending the close immediately (websockets
          # asyncio implementation, v16: no public abort on the connection).
          ws.transport.abort()
    except Exception as e:
      logger.warning("slack_listener_connection_dropped", error=str(e))

    await asyncio.sleep(backoff)
    backoff = min(backoff * 2, 30.0)


async def _serve_socket_mode(
    ws: ClientConnection, cfg: CharlieBotConfig, session_mgr: SessionManager, client: SlackClient,
    trigger_mgr: TriggerManager) -> None:
  """One Socket Mode connection's serve: hello, thread backfill, envelope loop.

  Returns on the server-sent disconnect; every other exit raises and the
  reconnect loop in ``run_listener`` owns it.
  """
  await _expect_hello(ws)
  logger.info("slack_listener_connected")
  await _backfill_followed_threads(cfg, session_mgr, client, trigger_mgr)
  async for raw in ws:
    envelope = json.loads(raw)
    envelope_id = envelope.get("envelope_id")
    if envelope.get("type") == "disconnect":
      logger.info("slack_listener_disconnect")
      return
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
        logger.info(
            "slack_listener_app_mention_handled",
            channel=channel,
            thread_ts=thread_ts,
            slack_user=slack_user,
            session=sid)
      except Exception as e:
        logger.exception(
            "slack_listener_app_mention_handle_failed",
            channel=channel,
            thread_ts=thread_ts,
            slack_user=slack_user,
            error=str(e))
    elif inner and inner.get("type") == "message":
      try:
        await handle_thread_message(inner, cfg, session_mgr, client, trigger_mgr)
      except Exception as e:
        logger.exception(
            "slack_listener_message_handle_failed",
            channel=inner.get("channel"),
            thread_ts=inner.get("thread_ts"),
            slack_user=inner.get("user"),
            error=str(e))
