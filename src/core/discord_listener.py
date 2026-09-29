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

import uuid

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.discord_client import snowflake_key
from src.core.log_once import LazyStructlogLogger
from src.core.thread_entry import ThreadPlatform, summon_prompt_tail

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
