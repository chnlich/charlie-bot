"""Discord gateway listener — the Discord half of the shared-thread entrypoint.

This module holds the Discord-specific pieces: the gateway ``MESSAGE_CREATE``
handler (``handle_message_create`` reads one gateway payload, applies Discord's
drop rules, and hands the rest to the shared core), the summon prompt and the
follow wake label, and the ``DiscordThreadAdapter`` over the bot client
(``src.core.discord_client.DiscordClient``). Everything platform-neutral lives
in ``src.core.thread_entry``: the summon acceptance (``accept_summon`` —
session create/unarchive/reuse, the watermark step, the group, the summon
event, the round and ack tasks), the mention consumption, the group
assignment, the follow triggers, the thread-message follow, the reconnect
backfill, and the round side (``post_reply``, ``assert_thread_fresh``,
``ack_messages``, ``deliver_done``, ``backfill_lost_summons``). This module
describes Discord to the core with the ``DISCORD`` platform and keeps the
public Discord-named wrappers (``post_reply``, ``assert_thread_fresh``,
``ack_messages``, ``deliver_done``, ``backfill_lost_summons``) that the CLI,
the server endpoint, and the session-manager wiring (a later step) import.
The gateway connect/receive/reconnect loop and its server wiring are a later
step too; they call ``handle_message_create`` per payload and
``_backfill_followed_threads`` per (re)connection, both already in place here.

The master posts to its session's thread itself, through ``charliebot discord
reply`` -> the ``post_reply`` wrapper, and reads the outcome back in the same
call; before any chunk posts, the reply path uploads every file-server page
the text links as an attachment on the last chunk and replaces its URL with
the file name — thread readers may not reach this server, so the reply text
stands on its own. The posted text is persisted as a ``discord_reply`` event
whose ``answers`` names the summon the running round was answering (None for a
round no summon started). ``deliver_done`` hangs off the round's terminal
``master_done`` event (called from ``SessionManager.persist_and_broadcast``),
not off a waiting coroutine, so it survives a server restart. The eyes ack
reaction tracks the open question: lit at the summon (the shared accept path's
ack task), cleared when a reply answering it lands, or when the notice or the
lost-summon report closes it.

Unlike Slack, the master never fetches the thread itself: the bot does the
reading server-side. Both the summon prompt and the follow wake tell it to
``charliebot discord read`` the thread — the readback returns the messages the
bot can see there (the same eligibility the round side reads) and marks them
read — and to ack before replying. A mention in a plain text channel starts a
thread from the mention message (named from the stripped content); a mention
inside an existing thread summons into that thread. Every Discord id is a
snowflake, so ids are compared and sorted through ``snowflake_key`` — never as
strings, whose order flips whenever the digit counts differ.

Thread follow: after the first summon, eligible thread messages (human,
allowed, newer than the session's ``discord_watermark_id``) arriving over the
same gateway connection — or found by the reconnect backfill on every
(re)connection — arm one persisted per-session trigger whose wake label names
the chain's floor id and the thread link. The reply path is gated on
freshness: ``assert_thread_fresh`` refuses with 412 until every eligible
message is acked (``ack_messages`` advances the watermark); silence stays a
legal round outcome because trigger wakes enter the log as scheduled-trigger
events with no discord block, outside the audit. A DM mention summons nothing
— there is no guild thread to bind — so the bot answers it with a one-line
notice pointing back to the server channels.
"""

import re
import uuid
from collections.abc import Callable, Sequence
from pathlib import Path

from src.core import event_types as ET
from src.core import thread_entry
from src.core.config import CharlieBotConfig, get_credentials
from src.core.discord_client import DiscordClient, message_link, snowflake_key
from src.core.http import get_http_client
from src.core.log_once import LazyStructlogLogger
from src.core.models import DiscordOrigin
from src.core.sessions import SessionManager
from src.core.thread_entry import ThreadAdapter, ThreadMessage, ThreadPlatform, summon_prompt_tail
from src.core.triggers import TriggerManager

logger = LazyStructlogLogger()

# Fixed namespace UUID for Discord summon session ids. Arbitrary but stable
# across process restarts; changing it would orphan every existing
# Discord-backed session.
DISCORD_NS = uuid.UUID("13667dae-7551-489f-9cf1-d0819d0978e6")

# The 👀 reaction is the summon ack: lit at the summon, cleared when the
# question closes (a reply, the no-reply notice, or the lost-summon report).
_ACCEPTANCE_EMOJI = "👀"

# Discord's hard per-message text limit, already the chunking target: every
# posted piece stays under it without a fallback split.
_MAX_POST_CHARS = 2000

# The command the reply-format contract (prompts/thread_reply_format.md) names
# for posting a reply. A summon prompt embeds that contract, so a summon whose
# content names the command was issued under it; the round-end audit enforces
# only that contract and leaves rounds issued under the earlier one alone.
_REPLY_COMMAND = "charliebot discord reply"

# The command the summon prompt and the follow wake name for reading the
# thread: the bot reads server-side (the gateway payload alone is not the
# thread), and the read marks the returned messages read.
_READ_COMMAND = "charliebot discord read"

# Trigger-label prefix identifying a session's armed thread-follow record.
_FOLLOW_TRIGGER_PREFIX = "discord-thread-follow"

# The new thread's name is cut to this before the client's own 100-char cap:
# the name comes from the mention's content, and a shorter cut keeps room for
# Discord's own decorations.
_THREAD_NAME_CHARS = 80

# The one-line answer to an allowed DM mention: a DM binds no guild thread, so
# the summon path cannot run there.
_DM_NOTICE = "请在服务器频道里 @ 我。"

# The platform description every shared thread helper takes; each value keeps
# its one home in the constants above.
DISCORD = ThreadPlatform(
    name="discord",
    display_name="Discord",
    reply_event_type=ET.DISCORD_REPLY,
    reply_command=_REPLY_COMMAND,
    max_post_chars=_MAX_POST_CHARS,
    follow_trigger_prefix=_FOLLOW_TRIGGER_PREFIX,
    id_key=snowflake_key,
    origin_field="discord_origin",
    watermark_field="discord_watermark_id",
    id_label="id",
    mention_key="mention_id",
    block_keys=("guild_id", "channel_id", "thread_id", "mention_id"),
    thread_fallback="(guild {guild_id}, thread {thread_id})",
    attaches_files=True,
)


def summon_session_id(guild_id: str, thread_id: str) -> str:
  """Return the deterministic session id for a Discord thread."""
  return str(uuid.uuid5(DISCORD_NS, f"discord:{guild_id}:{thread_id}"))


# ---------------------------------------------------------------------------
# Prompt texts
# ---------------------------------------------------------------------------

# The summon prompt's platform line. The shared reply-format contract
# (prompts/thread_reply_format.md) defers the platform-specific facts to
# this line: platform name, reply command, per-message limit, and how
# linked pages reach readers. Another platform's entrypoint states its own
# line and reuses the contract unchanged.
_PLATFORM_LINE = (
    f"Platform: Discord. Reply command: `{_REPLY_COMMAND} --file <path>`. "
    f"Per-message limit: {_MAX_POST_CHARS} characters. "
    "Linked pages: the reply path uploads each linked file-server page as an attachment and replaces its URL with "
    "the file name; thread readers may not reach this server, so the reply text stands on its own.")


def _build_summon_prompt(link: str, cfg: CharlieBotConfig) -> str:
  """The persisted summon: the mention-message link plus the server-side read hint, ending at the fixed notices.

  Only the link is stored because the bot reads the thread server-side when the
  round runs — a snapshot persisted here would go stale as the thread keeps
  changing after the mention, and the gateway payload that started the round
  carries only one message of it.

  The tail after the platform line (citation boundary, PII red line,
  reply-format contract) is the shared one from thread_entry, read fresh from
  prompts/ on every call — no caching, so an edit takes effect on the next
  summon.
  """
  return (
      f"Discord 线程召唤：{link}\n\n"
      f"用 `{_READ_COMMAND}` 读这条线程（服务端代读，返回 bot 在这条线程里能看到的消息，未读的随之记为已读）。\n\n"
      f"{summon_prompt_tail(_PLATFORM_LINE, cfg)}")


def _build_follow_wake_message(floor: str, link: str) -> str:
  """The armed follow trigger's label: the chain floor id, the thread link, and the wake contract.

  ``floor=<id>`` on the first line is machine-readable: a re-arm parses it back
  so the wake always reads from the chain's oldest unacked message, independent
  of watermark state. The read is server-side too, so the wake orders the read
  first — the round must read even when it plans to stay silent — and the
  reply under the shared redline and format docs.
  """
  return (
      f"{_FOLLOW_TRIGGER_PREFIX} floor={floor}\n"
      f"Discord 线程跟帖唤醒：{link}\n"
      f"用 `{_READ_COMMAND}` 读线程里的新消息（本次返回的未读消息随之记为已读，本轮沉默也要先读）；"
      "回复之前从仓库重读 prompts/thread_reply_redline.md 与 prompts/thread_reply_format.md；"
      f"只在值得时用 `{_REPLY_COMMAND} --file <path>` 回复。")


# ---------------------------------------------------------------------------
# Message eligibility and the bot client
# ---------------------------------------------------------------------------


def _eligible_message(message: dict, allowed_user_ids: list[str]) -> bool:
  """The message-eligibility rule both the follow path and the adapter's readback apply.

  A message is eligible when a human authored it (no ``bot`` flag on the
  author, no ``webhook_id`` on the message), it is a plain message or a reply
  (type 0 or 19), and its author is allowed. Gate eligibility equals guard
  eligibility, so nothing is demanded of an ack that the session would never
  consume; the follow side restates the same rule against the raw payload.
  """
  author = message.get("author") or {}
  return (
      not author.get("bot") and message.get("webhook_id") is None and message.get("type") in (0, 19) and
      author.get("id") in allowed_user_ids)


def _bot_client() -> DiscordClient:
  """The client every outbound path (reply, notice, backfill) posts through."""
  creds = get_credentials()
  return DiscordClient(get_http_client(), bot_token=str(creds.require("discord", "bot_token")))


# ---------------------------------------------------------------------------
# The adapter the shared core works through
# ---------------------------------------------------------------------------


class DiscordThreadAdapter(ThreadAdapter):
  """The round side's face onto the Discord REST client.

  Discord receives files: its link swap hands the linked page's file name back
  for the URL and collects the path, and ``post`` rides the last chunk's
  attachments (``attaches_files``). The client is built lazily — a given one
  is used as is; without one, ``_bot_client`` runs on the first platform call
  and is reused, resolved as the module global at that moment so tests can
  stub the factory.
  """

  platform = DISCORD

  def __init__(self, client: DiscordClient | None = None) -> None:
    self._client = client

  def _ensure_client(self) -> DiscordClient:
    """The bot client every platform call posts through: built once, then reused."""
    if self._client is None:
      self._client = _bot_client()
    return self._client

  async def post(self, address: dict, text: str, files: Sequence[Path]) -> None:
    await self._ensure_client().create_message(address["thread_id"], text, files=files)

  async def add_ack(self, block: dict) -> None:
    await self._ensure_client().add_reaction(block["channel_id"], block[self.platform.mention_key], _ACCEPTANCE_EMOJI)

  async def remove_ack(self, block: dict) -> None:
    await self._ensure_client().remove_own_reaction(
        block["channel_id"], block[self.platform.mention_key], _ACCEPTANCE_EMOJI)

  async def read_eligible(self, origin: DiscordOrigin, cfg: CharlieBotConfig) -> list[ThreadMessage]:
    """The thread's eligible messages, paged oldest-first through the REST readback.

    Discord returns at most 100 messages per call, so the read pages from id
    ``"0"`` (before every snowflake), continuing after the page's last id,
    until a page comes back shorter than the limit. Each page already arrives
    sorted oldest-first by the client; eligibility is the shared rule.
    """
    client = self._ensure_client()
    messages: list[ThreadMessage] = []
    after = "0"
    while True:
      page = await client.get_messages(origin.thread_id, after=after, limit=100)
      messages.extend(
          ThreadMessage(m["id"], m["author"]["id"], m.get("content") or "") for m in page
          if _eligible_message(m, cfg.discord.allowed_user_ids))
      if len(page) < 100:
        return messages
      after = page[-1]["id"]

  def address_of(self, origin: DiscordOrigin) -> dict:
    return {"guild_id": origin.guild_id, "thread_id": origin.thread_id}

  def link_swap(self, cfg: CharlieBotConfig) -> tuple[Callable[[Path], str], list[Path]]:
    """The attachment swap for the shared link rewrite: keep the file name, collect the path.

    Each distinct linked path is appended once (a path linked twice attaches
    once) and its URL becomes the bare file name — the attachment rides the
    same reply, so thread readers see the page without reaching this server.
    """
    files: list[Path] = []

    def swap(fs_path: Path) -> str:
      if fs_path not in files:
        files.append(fs_path)
      return fs_path.name

    return swap, files

  def log_fields(self, address: dict) -> dict:
    return {"guild": address["guild_id"], "thread": address["thread_id"]}

  async def thread_link(self, origin: DiscordOrigin) -> str:
    return message_link(origin.guild_id, origin.thread_id)

  def follow_wake_message(self, floor: str, link: str) -> str:
    return _build_follow_wake_message(floor, link)


# ---------------------------------------------------------------------------
# The gateway MESSAGE_CREATE handler
# ---------------------------------------------------------------------------

# The mention tokens a thread name strips: <@id> (user), <@!id> (nickname),
# and <@&id> (role).
_MENTION_TOKEN_RE = re.compile(r"<@[!&]?\d+>")


def _mention_ids(message: dict) -> list[str]:
  """The ids the payload's ``mentions`` user objects carry; the bot's own among them marks a mention."""
  return [m["id"] for m in message.get("mentions") or []]


def _thread_name(content: str) -> str:
  """The name for a thread started from one mention message.

  The content minus its mention tokens, whitespace collapsed, cut to
  ``_THREAD_NAME_CHARS``; a content of only tokens names the thread
  CharlieBot.
  """
  return " ".join(_MENTION_TOKEN_RE.sub("", content).split())[:_THREAD_NAME_CHARS] or "CharlieBot"


async def handle_message_create(
    message: dict,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    client: DiscordClient,
    trigger_mgr: TriggerManager,
    *,
    bot_user_id: str,
) -> str | None:
  """Accept or drop one gateway MESSAGE_CREATE payload; returns the session id when it summoned or armed a follow.

  Guard chain, in order — the payload is dropped when any check fails:
  (1) the sender is human (no ``bot`` flag on the author, no ``webhook_id``)
  and the message is a plain message or a reply (type 0 or 19);
  (2) the author is allowed;
  (3) a DM (no ``guild_id``) never summons — there is no guild thread to bind
  — so an allowed mention there earns the one-line DM notice and nothing
  else, and no session is touched either way;
  (4) an unmentioned message is follow traffic for an existing thread: the
  shared follow path arms (or re-arms) the session's one persisted follow
  trigger when the session exists, is ACTIVE, matches the channel, and the id
  sorts above the watermark.

  A mentioned payload summons. The channel holding the mention decides the
  shape: types 10, 11, and 12 are threads (the summon binds that thread, the
  label names the parent channel); types 0 and 5 start a thread from the
  mention message, named from the stripped content (the label names the
  channel); any other type drops with an info log. The summon block carries
  the channel holding the mention, the thread the session binds, and the
  mention message id (the ack reaction's target). The session resolution
  (create with the ``discord_origin``, unarchive, or reuse), the watermark
  step, the group assignment, the summon persistence, and the round and ack
  tasks are the shared core's (``thread_entry.accept_summon``).
  """
  guild_id = message.get("guild_id")
  channel_id = message.get("channel_id")
  message_id = message.get("id")
  author_id = (message.get("author") or {}).get("id")
  if not _eligible_message(message, cfg.discord.allowed_user_ids):
    return None
  mentioned = bot_user_id in _mention_ids(message)

  if guild_id is None:
    if mentioned:
      await client.create_message(channel_id, _DM_NOTICE)
      logger.info("discord_dm_notice_posted", channel=channel_id, user=author_id)
    return None

  if not mentioned:
    return await thread_entry.follow_message(
        DiscordThreadAdapter(client),
        session_mgr,
        trigger_mgr,
        summon_session_id(guild_id, channel_id),
        message_id,
        origin_matches=lambda origin: origin.thread_id == channel_id,
    )

  channel = await client.get_channel(channel_id)
  if channel["type"] in (10, 11, 12):  # the mention already sits in a thread
    thread_id = channel_id
    parent_channel_id = channel["parent_id"]
    parent = await client.get_channel(parent_channel_id)
  elif channel["type"] in (0, 5):  # a text or announcement mention starts its own thread
    thread = await client.start_thread_from_message(channel_id, message_id, _thread_name(message["content"]))
    thread_id = thread["id"]
    parent_channel_id = channel_id
    parent = channel
  else:
    logger.info(
        "discord_mention_dropped_channel_type", guild=guild_id, channel=channel_id, channel_type=channel["type"])
    return None

  return await thread_entry.accept_summon(
      DiscordThreadAdapter(client),
      cfg,
      session_mgr,
      trigger_mgr,
      session_id=summon_session_id(guild_id, thread_id),
      label=f"Discord #{parent['name']}",
      origin=DiscordOrigin(guild_id=guild_id, parent_channel_id=parent_channel_id, thread_id=thread_id),
      block={"guild_id": guild_id, "channel_id": channel_id, "thread_id": thread_id, "mention_id": message_id},
      content=_build_summon_prompt(message_link(guild_id, channel_id, message_id), cfg),
      user=author_id,
  )
