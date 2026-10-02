"""Platform-neutral shared-thread core.

The shared thread feature exists once per chat platform (Slack and Discord
today): a summon starts a session bound to one thread, the round answers into
that thread, and the audit keeps summon and reply consistent across restarts.
This module holds what every platform shares: the platform description
(``ThreadPlatform``), the pure helpers, the adapter surface both the summon
and follow side and the round side work through (``ThreadAdapter``, one
subclass per entrypoint wrapping the platform's client), the summon and
thread-follow side — summon acceptance (``accept_summon``), the mention
consumption (``consume_mention``), the group assignment (``ensure_group``),
the follow triggers (``arm_follow_trigger``), the thread-message follow
(``follow_message``), the unread readback (``unread_messages``), and the
reconnect backfill (``backfill_followed_threads``) — and the round side: the
reply path (``post_reply``), the freshness gate (``assert_thread_fresh``), the
ack (``ack_messages``), the round-end audit (``deliver_done`` over
``audit_round``), and the lost-summon backfill (``backfill_lost_summons``).
The follow side wakes its session whatever the session's stored status: an
archived thread session is revived first (unarchived, logged, its task-tree
change broadcast), so any eligible thread message brings the session back to
the sidebar's Threads view. Each per-platform entrypoint
(``src.core.slack_listener``, ``src.core.discord_listener``) describes its
platform with one ``ThreadPlatform`` instance built from its own constants and
hands platform plus adapter to these functions. Imports point one way: the
entrypoint imports this module, never the reverse.
"""

import abc
import asyncio
import re
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any
from urllib.parse import unquote
from zoneinfo import ZoneInfo

from src.api.deps import SESSION_NOT_FOUND_DETAIL
from src.api.message_utils import build_agent_message_event, master_done_input_event_ids
from src.core import event_types as ET
from src.core.config import HOUSE_TIMEZONE, CharlieBotConfig
from src.core.constants import FILE_SERVER_MOUNTS
from src.core.log_once import LazyStructlogLogger
from src.core.master_trigger import trigger_master
from src.core.models import (
    CreateSessionRequest,
    PendingTrigger,
    SessionMetadata,
    SessionStatus,
    TriggerStatus,
    utc_now,
)
from src.core.publish import PublishError, publish_artifact
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task
from src.core.triggers import ArchivedSessionError, TriggerManager

logger = LazyStructlogLogger()


@dataclass(frozen=True)
class ThreadPlatform:
  """The per-platform facts the shared helpers and the entrypoint agree on.

  ``name`` is the summon event-block key and the prefix of every persisted
  marker (``slack`` for Slack, ``discord`` for Discord), so the derived keys
  spell the platform's marker payload keys and ack wire type from one string.
  ``id_key`` maps one message id to its ordering key: the platform's id sort
  (``str`` for Slack's dotted ts strings, ``int`` for Discord snowflakes).
  """

  name: str
  display_name: str
  reply_event_type: str
  # The command the reply-format contract (prompts/thread_reply_format.md)
  # names for posting a reply. A summon prompt embeds that contract, so a
  # summon whose content names the command was issued under it; the
  # round-end audit enforces only that contract and leaves rounds issued
  # under the earlier one alone.
  reply_command: str
  max_post_chars: int
  # The platform's scope doc under prompts/ (what personal information may
  # enter the platform's threads): read fresh into every summon prompt and
  # named by every follow wake.
  scope_doc: str
  follow_trigger_prefix: str
  id_key: Callable[[str], Any]
  # ``SessionMetadata`` attribute holding the thread origin (``slack_origin``,
  # ``discord_origin``) and the newest consumed message id
  # (``slack_watermark_ts``, ``discord_watermark_id``): the round side reads
  # and writes both by these names.
  origin_field: str
  watermark_field: str
  # The key naming a message id in readbacks and refusals (``ts`` for Slack,
  # ``id`` for Discord): the 412 payload's per-message key, and the ack
  # readback's ``watermark_<id_label>`` key.
  id_label: str
  # The summon-block key of the mention message (``mention_ts`` for Slack,
  # ``mention_id`` for Discord); a block without it carries no ack to clear.
  mention_key: str
  # The summon-block keys a nudge copies from the summon it re-asks.
  block_keys: tuple[str, ...]
  # Formatted with the summon block when a summon prompt carries no link.
  thread_fallback: str

  @property
  def notice_key(self) -> str:
    """Event payload key of the thread's no-reply notice marker."""
    return f"{self.name}_notice"

  @property
  def backfill_key(self) -> str:
    """Event payload key of the lost-summon backfill marker."""
    return f"{self.name}_backfill"

  @property
  def ack_event_type(self) -> str:
    """Wire type of the platform's persisted ack audit record."""
    return f"{self.name}_ack"


@dataclass(frozen=True)
class ThreadMessage:
  """One eligible thread message as the round side reads it.

  ``id`` is the platform's message id (a Slack ts, a Discord snowflake), ``user``
  its author (None when the platform names none), and ``text`` its text
  (empty when the platform names none).
  """

  id: str
  user: str | None
  text: str


class ThreadAdapter(abc.ABC):
  """The per-platform posting face the round side works through and the summon and follow side arms through.

  One subclass per entrypoint wraps the platform's client: posting into the
  thread, lighting and clearing the summon ack, reading the thread's eligible
  messages, naming the thread (the ``address`` dict ``post`` accepts),
  shaping the log fields that point at the thread, and naming the follow
  wake. The platform description rides on the class, so every core helper
  reads it off the adapter it was handed.
  """

  platform: ThreadPlatform

  @abc.abstractmethod
  async def post(self, address: dict, text: str) -> None:
    """Post one text message into the thread *address* names; raises on failure -- ``post_with_retry`` catches it."""

  @abc.abstractmethod
  async def add_ack(self, block: dict) -> None:
    """Light the summon ack the summon block *block* carries (the mirror of ``remove_ack``)."""

  @abc.abstractmethod
  async def remove_ack(self, block: dict) -> None:
    """Clear the summon ack the summon block *block* carries."""

  @abc.abstractmethod
  async def read_eligible(self, origin: Any, cfg: CharlieBotConfig) -> list[ThreadMessage]:
    """The thread *origin* names' eligible messages, in platform order."""

  @abc.abstractmethod
  def address_of(self, origin: Any) -> dict:
    """The address dict ``post`` accepts for the thread *origin* names."""

  @abc.abstractmethod
  def log_fields(self, address: dict) -> dict:
    """The log fields naming the thread *address* points at."""

  @abc.abstractmethod
  async def thread_link(self, origin: Any) -> str:
    """The permalink naming the thread *origin* points at: the link a wake label names."""

  @abc.abstractmethod
  def follow_wake_message(self, floor: str, link: str) -> str:
    """The armed follow trigger's label: the chain *floor* id, the thread *link*, and the wake contract."""


class ThreadReplyError(Exception):
  """A reply the endpoint refuses or the platform did not accept; ``status`` is the HTTP status it maps to.

  ``detail`` becomes the HTTPException detail; the 412 refusal carries the
  structured ``stale_thread`` payload instead of a plain string.
  """

  def __init__(self, status: int, detail: str | dict) -> None:
    super().__init__(str(detail))
    self.status = status
    self.detail = detail


def load_prompt_doc(repo_root: Path, name: str, *, likely_cause: str) -> str:
  """Read one prompts doc fresh from disk, raising a ValueError naming the path when missing.

  No caching, so an edit takes effect on the next summon. A missing or
  unreadable doc raises a ValueError naming the path and its most likely cause
  (mirrors the worker-prompt loader in src/core/spawner_prompt.py); a prompt
  without the doc is never built.
  """
  path = repo_root / "prompts" / name
  try:
    return path.read_text(encoding="utf-8").strip()
  except OSError as e:
    raise ValueError(f"{name} prompt not found at {path} — {likely_cause}") from e


def summon_prompt_tail(platform: ThreadPlatform, platform_line: str, cfg: CharlieBotConfig) -> str:
  """The summon prompt's fixed tail: the platform's scope doc, the PII red line, and the reply-format contract.

  All three docs (the platform's scope doc, prompts/thread_reply_redline.md,
  prompts/thread_reply_format.md) are read fresh from prompts/ on every call —
  no caching, so an edit takes effect on the next summon. A missing or
  unreadable doc raises a ValueError naming the path; a prompt without all
  three docs is never built. The reply-format contract defers the
  platform-specific facts to *platform_line* — platform name, reply command,
  per-message limit, and how linked pages reach readers (the shared
  ``LINKED_PAGES_LINE``) — and each platform's entrypoint states its own line
  and reuses the contract unchanged. The scope doc name rides in through
  *platform*; the red line and the reply-format contract are shared unchanged
  across platforms.
  """
  scope_doc = load_prompt_doc(
      cfg.charlie_bot_repo,
      platform.scope_doc,
      likely_cause=f"the repo checkout most likely predates the {platform.scope_doc} addition commit")
  red_line = load_prompt_doc(
      cfg.charlie_bot_repo,
      "thread_reply_redline.md",
      likely_cause="the repo checkout most likely predates the thread-reply-redline rename commit")
  reply_format = load_prompt_doc(
      cfg.charlie_bot_repo,
      "thread_reply_format.md",
      likely_cause="the repo checkout most likely predates the thread-reply-format rename commit")
  return f"{platform_line}\n\n{scope_doc}\n{red_line}\n{reply_format}"


def follow_wake_label(platform: ThreadPlatform, floor: str, link: str, read_rule: str, ack_rule: str) -> str:
  """The armed follow trigger's label: the chain *floor* id, the thread *link*, and the wake contract.

  ``floor=<id>`` on the first line is machine-readable: a re-arm parses it back
  so the wake always reads from the chain's oldest unacked message, independent
  of watermark state. *read_rule* and *ack_rule* are the platform's own
  sentences — how the wake reads the thread, and how the round acks what it
  read — and the frame around them is shared: the trigger prefix, the link
  line, the docs the round re-reads before replying, and the reply command.
  """
  return (
      f"{platform.follow_trigger_prefix} floor={floor}\n"
      f"{platform.display_name} 线程跟帖唤醒：{link}\n"
      f"{read_rule}"
      f"回复之前从仓库重读 prompts/{platform.scope_doc}、prompts/thread_reply_redline.md 与 "
      "prompts/thread_reply_format.md；"
      f"{ack_rule}"
      f"只在值得时用 `{platform.reply_command} --file <path>` 回复。")


def chunk_text(text: str, limit: int) -> list[str]:
  """Split *text* into chunks of at most *limit* chars for sequential posting.

  Greedy packing over paragraph units (a paragraph plus its trailing
  blank-line separator); a unit longer than *limit* falls to newline units,
  and a single line longer than *limit* is hard-cut. Chunks keep the input
  order and no boundary eats content; the whitespace-only chunk dropped at
  return (it carries no postable text) is the only way the posted chunks fall
  short of the input.
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
  # dropped: the platform rejects text-less posts, and no content is lost by it.
  return [c for c in chunks if c.strip()] or chunks


def event_by_id(events: list[dict], event_id: str) -> dict | None:
  """The event with this id, or None."""
  for ev in events:
    if ev.get("id") == event_id:
      return ev
  return None


def summon_of(block: dict, event_id: str) -> str:
  """The summon a round with this summon block answers: the block's ``nudge_of`` for a nudge, else the event itself."""
  return block.get("nudge_of") or event_id


def newest_thread_input(platform: ThreadPlatform, events: list[dict], event_ids: list[str]) -> tuple[str, dict] | None:
  """``(event id, summon block)`` of the newest summon-bearing input among *event_ids*, or None.

  A round answers a list of inputs (one per queued item it merged); only an
  input that carries a summon block under the platform's key (a summon or its
  nudge) binds the reply to a summon, and the newest of those -- latest in log
  order -- is the thread's freshest ask. A browser-typed message, a trigger
  wake, or an empty list binds nothing.
  """
  wanted = set(event_ids)
  bound: tuple[str, dict] | None = None
  for ev in events:
    if ev.get("id") in wanted and isinstance(ev.get(platform.name), dict):
      bound = (str(ev["id"]), ev[platform.name])
  return bound


def replied(platform: ThreadPlatform, events: list[dict], summon_id: str) -> bool:
  """Whether the log holds a reply event answering *summon_id*."""
  return any(
      ev.get("type") == platform.reply_event_type and
      (ev.get(platform.reply_event_type) or {}).get("answers") == summon_id for ev in events)


def nudged(platform: ThreadPlatform, events: list[dict], summon_id: str) -> bool:
  """Whether the log holds the nudge for *summon_id* (a summon-block message carrying ``nudge_of``)."""
  return any(
      ev.get("type") == ET.AGENT_MESSAGE and (ev.get(platform.name) or {}).get("nudge_of") == summon_id
      for ev in events)


def noticed(platform: ThreadPlatform, events: list[dict], summon_id: str) -> bool:
  """Whether the log holds the thread's no-reply notice marker for *summon_id*."""
  return any((ev.get(platform.notice_key) or {}).get(ET.INPUT_EVENT_ID) == summon_id for ev in events)


def lost_summons(platform: ThreadPlatform, events: list[dict], *, owned: set[str], running: set[str]) -> list[dict]:
  """The summon injections in one session's log that nothing will ever answer.

  A summon (or a nudge: it carries the same summon block) is lost when no
  master_done names it (any id of a merged round's list counts as answered),
  this process does not already own it (queued or running), the session's
  master_run record does not name it among its whole input list (alive but not
  followable), and no earlier backfill marked it. The marker is the
  ``<name>_backfill`` payload, never a synthetic master_done: that event is
  the cut point replay uses to decide which user messages are still unanswered.
  """
  answered: set[str] = set()
  for ev in events:
    if ev.get("type") == ET.MASTER_DONE:
      answered.update(master_done_input_event_ids(ev))
  marked = {ev[platform.backfill_key].get(ET.INPUT_EVENT_ID) for ev in events if platform.backfill_key in ev}
  return [
      ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE and platform.name in ev and ev["id"] not in answered and
      ev["id"] not in marked and ev["id"] not in owned and ev["id"] not in running
  ]


def unread_after(
    messages: list[dict],
    *,
    eligible: Callable[[dict], bool],
    message_id: Callable[[dict], str],
    watermark: str | None,
    id_key: Callable[[str], Any],
) -> list[dict]:
  """The eligible messages whose id sorts strictly above *watermark* under *id_key*.

  A None watermark passes every eligible message. *eligible* is the
  platform's thread-eligibility rule and *message_id* reads one message's id;
  the comparison runs on the platform's id ordering key, so a string-sorted id
  space (Slack ts strings) and an integer-sorted one (Discord snowflakes)
  share the rule.
  """
  if watermark is None:
    return [m for m in messages if eligible(m)]
  floor = id_key(watermark)
  return [m for m in messages if eligible(m) and id_key(message_id(m)) > floor]


# The chain floor carried on every follow label; re-arms parse it back.
_FOLLOW_FLOOR_RE = re.compile(r"floor=([0-9.]+)")


def follow_floor(label: str) -> str | None:
  """The ``floor=<id>`` value on a follow-trigger label, or None when the label carries none.

  The value matches digits and dots only.
  """
  match = _FOLLOW_FLOOR_RE.search(label)
  return match.group(1) if match is not None else None


# ---------------------------------------------------------------------------
# Round side: the posting, ack, freshness-gate, reply, audit, and backfill
# machinery every platform entrypoint shares, working through one adapter.
# ---------------------------------------------------------------------------

# Waits between the retries of one platform call; the answer stays readable in
# the session log either way, so exhausting them logs an error rather than raising.
_RETRY_DELAYS = (1.0, 4.0)


async def post_with_retry(adapter: ThreadAdapter, address: dict, text: str, *, session_id: str) -> bool:
  """Post one thread reply, retrying on failure; True when the platform accepted it.

  Exhausting the retries logs an error and returns False instead of raising:
  the caller decides what a failed post means (a 502 to the CLI, a notice
  left for the boot audit), and the event funnel is never broken by one.
  """
  name = adapter.platform.name
  attempts = len(_RETRY_DELAYS) + 1
  for attempt in range(attempts):
    try:
      await adapter.post(address, text)
      return True
    except Exception as e:
      if attempt == attempts - 1:
        logger.error(
            f"{name}_post_gave_up", session=session_id, **adapter.log_fields(address), attempts=attempts, error=str(e))
        return False
      logger.warning(
          f"{name}_post_retry", session=session_id, **adapter.log_fields(address), attempt=attempt + 1, error=str(e))
      await asyncio.sleep(_RETRY_DELAYS[attempt])
  raise AssertionError("unreachable: the last loop iteration returns (attempt == attempts - 1)")


def ack_clear(adapter: ThreadAdapter, block: dict, session_id: str) -> None:
  """Clear the summon eye once its question is closed (reply landed, notice posted, or lost).

  Fires the remove as its own logged task — a failure there only leaves one
  stale eye plus a background_task_failed log and never touches the caller's
  result. Skipped when the persisted block carries no platform mention key.
  """
  if block.get(adapter.platform.mention_key) is None:
    return
  create_logged_task(adapter.remove_ack(block), name=f"{adapter.platform.name}-ack-clear-{session_id}")


# How much of an unread message's text the 412 refusal and the gate list carry.
_TEXT_PREVIEW_CHARS = 200


async def require_thread_session(
    platform: ThreadPlatform, session_id: str, session_mgr: SessionManager) -> SessionMetadata:
  """The session named by *session_id* when it exists and carries a platform thread.

  The reply-path preamble shared by ``assert_thread_fresh``, ``ack_messages``,
  and ``post_reply``. Refusals raise ``ThreadReplyError``: 404 unknown session,
  409 no platform thread.
  """
  meta = await session_mgr.get_session(session_id)
  if meta is None:
    raise ThreadReplyError(404, SESSION_NOT_FOUND_DETAIL)
  if getattr(meta, platform.origin_field) is None:
    raise ThreadReplyError(409, f"Session has no {platform.display_name} thread")
  return meta


async def unread_messages(adapter: ThreadAdapter, origin: Any, cfg: CharlieBotConfig,
                          watermark: str | None) -> list[ThreadMessage]:
  """The thread *origin* names' eligible messages above *watermark*; None passes all.

  One readback shared by the freshness gate and the reconnect backfill: the
  adapter's eligible read, filtered to ids sorting strictly above the
  watermark under the platform's id key.
  """
  messages = await adapter.read_eligible(origin, cfg)
  if watermark is None:
    return messages
  floor = adapter.platform.id_key(watermark)
  return [m for m in messages if adapter.platform.id_key(m.id) > floor]


async def assert_thread_fresh(
    adapter: ThreadAdapter, session_id: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
  """Refuse the reply when eligible thread messages sit above the session's watermark.

  The reply-path gate, run by the reply endpoint before ``post_reply``:
  a reply to a thread the running round has not acked through would answer a
  stale state, so nothing posts until the ack advances the watermark. Refusals
  raise ``ThreadReplyError``: 404 unknown session, 409 no platform thread, then
  412 with the structured ``stale_thread`` payload naming each unread message's
  id, user, and a text preview; with a None watermark the whole thread tail
  counts.
  """
  platform = adapter.platform
  meta = await require_thread_session(platform, session_id, session_mgr)
  watermark = getattr(meta, platform.watermark_field)
  unread = await unread_messages(adapter, getattr(meta, platform.origin_field), cfg, watermark)
  if not unread:
    return
  raise ThreadReplyError(
      412, {
          "error": "stale_thread",
          "new_messages":
              [
                  {
                      platform.id_label: m.id,
                      "user": m.user,
                      "text_preview": m.text[:_TEXT_PREVIEW_CHARS],
                  } for m in unread
              ],
          f"watermark_{platform.id_label}": watermark,
      })


# The reply budget the format contract states (prompts/thread_reply_format.md):
# replies should stay under this many chars, with the depth going to a page.
# This is the single measurement point for that budget.
_REPLY_BUDGET_CHARS = 500


async def post_reply(
    adapter: ThreadAdapter, session_id: str, text: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> dict:
  """Post *text* to the session's thread and return the readback the CLI prints.

  Before any chunk posts, the file links in the text are rewritten through
  ``publish_swap`` (``rewrite_file_links``), so the thread receives published
  links its readers can open; the refusal paths there — a linked file gone, a
  publish preflight failure — raise ``ThreadReplyError`` and leave the thread
  untouched. Refusals raise ``ThreadReplyError``: 404 unknown session, 409 no
  platform thread, 422 blank text, 422 a rewrite refusal, 502 when a chunk
  exhausted its retries (nothing is persisted then, so the caller can retry).
  The endpoint runs ``assert_thread_fresh`` first, so a stale thread (412)
  never reaches this function. On success the platform's reply event records
  the text that went out, the summon it answers, and the chunk count; a reply that
  answers a summon clears that summon's ack. The readback carries that outbound
  text plus one line naming any application-route links, which stay as written and
  reach the operator alone.
  """
  platform = adapter.platform
  meta = await require_thread_session(platform, session_id, session_mgr)
  if not text.strip():
    raise ThreadReplyError(422, "Reply text is empty")

  text, operator_only_links = await asyncio.to_thread(rewrite_file_links, text, cfg, publish_swap(cfg))

  # lazy: mirrors the backfill import's agents-package guard
  from src.agents import master_cc_state

  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  # Binding identity: the in-process running round first (authoritative, no
  # metadata-cache race), the disk record as fallback for the restart gap where
  # an orphaned master posts before the re-attach item reaches the consumer.
  # Either way the round answers a list of inputs; the newest thread-bearing one
  # of the list is the summon this reply answers.
  input_event_ids = master_cc_state.running_user_event_ids(session_id)
  if not input_event_ids:
    fresh = await session_mgr.read_metadata_fresh(session_id)
    if fresh is not None and fresh.master_run is not None:
      input_event_ids = fresh.master_run.user_event_ids
  bound = newest_thread_input(platform, events, input_event_ids)
  answers = summon_of(bound[1], bound[0]) if bound is not None else None
  address = adapter.address_of(getattr(meta, platform.origin_field))
  bodies = chunk_text(text, platform.max_post_chars)
  for index, body in enumerate(bodies, start=1):
    ok = await post_with_retry(adapter, address, body, session_id=session_id)
    if not ok:
      raise ThreadReplyError(
          502, f"{platform.display_name} did not accept chunk {index} of {len(bodies)} after {len(_RETRY_DELAYS) + 1} "
          "attempts; nothing was persisted")

  payload = {"answers": answers, "chars": len(text), "chunks": len(bodies)}
  await session_mgr.persist_and_broadcast(
      session_id, {
          "type": platform.reply_event_type,
          "content": text,
          platform.reply_event_type: payload,
      })
  if bound is not None:
    ack_clear(adapter, bound[1], session_id)
  over_budget = len(text) > _REPLY_BUDGET_CHARS
  logger.info(
      f"{platform.name}_reply_posted",
      session=session_id,
      **adapter.log_fields(address),
      chars=len(text),
      chunks=len(bodies),
      over_budget=over_budget,
      budget=_REPLY_BUDGET_CHARS,
      answers=answers)
  return {
      "posted": True,
      "text": text,
      "operator_only_note": operator_only_note(operator_only_links),
      "chars": len(text),
      "chunks": len(bodies),
      "over_budget": over_budget,
      "answers": answers,
  }


async def ack_messages(
    adapter: ThreadAdapter, session_id: str, message_ids: list[str], cfg: CharlieBotConfig,
    session_mgr: SessionManager) -> dict:
  """Advance the session's read watermark over *message_ids*; return the readback the CLI prints.

  The follow round's proof-of-read. Refusals raise ``ThreadReplyError``: 404
  unknown session, 409 no platform thread, 422 an empty set, an unknown or
  ineligible id, or an eligible unread id at or below ``max(message_ids)``
  missing from the set (named) — nothing unread may be jumped over and
  nothing is persisted on a refusal; the natural batch is the gate refusal's
  own list. On success the watermark advances to ``max(message_ids)``, a
  small ack event lands in the session log for the audit trail, and re-acking
  ids at or below the watermark is an idempotent no-op counted as acked.
  """
  platform = adapter.platform
  meta = await require_thread_session(platform, session_id, session_mgr)
  ids = sorted(set(message_ids), key=platform.id_key)
  if not ids:
    raise ThreadReplyError(422, "message_ids is empty")
  eligible = {m.id for m in await adapter.read_eligible(getattr(meta, platform.origin_field), cfg)}
  unknown = [i for i in ids if i not in eligible]
  if unknown:
    raise ThreadReplyError(422, f"Unknown or ineligible message id: {unknown[0]}")
  watermark = getattr(meta, platform.watermark_field)
  ceiling = ids[-1]
  floor = None if watermark is None else platform.id_key(watermark)
  ceiling_key = platform.id_key(ceiling)
  skipped = [
      i for i in sorted(eligible, key=platform.id_key)
      if (floor is None or platform.id_key(i) > floor) and platform.id_key(i) <= ceiling_key and i not in ids
  ]
  if skipped:
    raise ThreadReplyError(422, f"Skipped eligible message id at or below {ceiling}: {skipped[0]}")
  if floor is None or ceiling_key > floor:
    watermark = ceiling
    setattr(meta, platform.watermark_field, watermark)
    meta.updated_at = utc_now()
    await session_mgr.save_metadata(meta)
  await session_mgr.persist_and_broadcast(
      session_id, {
          "type": platform.ack_event_type,
          "content": f"{platform.display_name} thread ack: {len(ids)} message(s) read through {ceiling}",
          platform.ack_event_type: {
              "message_ids": ids,
              f"watermark_{platform.id_label}": watermark,
          },
      })
  logger.info(
      f"{platform.name}_thread_acked",
      session=session_id,
      acked=len(ids),
      **{f"watermark_{platform.id_label}": watermark})
  return {"acked": len(ids), f"watermark_{platform.id_label}": watermark}


# Nudge event content: the summon round ended without a reply, so the master is
# asked once whether the thread should hear something.
_NUDGE_TEMPLATE = (
    "{platform} thread {link}: the round answering this mention ended without posting a reply\n"
    "(no `{command}` call). Decide now: when the thread should hear something, post it with\n"
    "`{command} --file <path>`; when there is nothing to say, end this round and the thread\n"
    "gets a one-line notice pointing to this session.")

# Thread-visible end state after a summon round and its nudge round both posted nothing.
_NO_REPLY_NOTICE = (
    "No reply was posted for this mention; the details are in the session log. "
    "Mention me again for a thread answer.")

# Session-log content of the notice marker; its notice payload names the summon it closes.
_NO_REPLY_CONTENT = "This {platform} mention got no reply from its round or the nudge round; the thread was told so."


def thread_link(platform: ThreadPlatform, summon: dict | None, block: dict) -> str:
  """The thread link as the summon prompt states it; the platform's fallback ids when it has none."""
  match = re.search(r"https?://\S+", (summon or {}).get("content") or "")
  if match is not None:
    return match.group(0)
  return platform.thread_fallback.format(**block)


async def audit_round(
    adapter: ThreadAdapter, session_id: str, events: list[dict], target: dict, input_event_id: str,
    cfg: CharlieBotConfig, session_mgr: SessionManager) -> bool:
  """Act on one finished round whose input carried a summon block; True when it acted.

  Reads the log for the round's summon: a reply answering it ends the audit. A
  summon round without one gets a nudge (once: a second done for the same
  summon finds the nudge event); a nudge round without one gets the thread
  notice (once: the notice marker, persisted only after the post
  succeeded, so a failed post leaves the boot audit a retry). A summon issued
  under the marker contract (its prompt names no reply command) is outside
  this audit.
  """
  platform = adapter.platform
  summon_id = summon_of(target, input_event_id)
  summon = event_by_id(events, summon_id)
  if platform.reply_command not in ((summon or {}).get("content") or ""):
    return False
  if replied(platform, events, summon_id):
    return False

  if "nudge_of" not in target:
    if nudged(platform, events, summon_id):
      return False
    content = _NUDGE_TEMPLATE.format(
        platform=platform.display_name, link=thread_link(platform, summon, target), command=platform.reply_command)
    nudge = build_agent_message_event(content, from_session=session_id, from_session_name=platform.display_name)
    nudge[platform.name] = {key: target[key] for key in platform.block_keys if key in target}
    nudge[platform.name]["nudge_of"] = summon_id
    await session_mgr.persist_and_broadcast(session_id, nudge)
    create_logged_task(
        trigger_master(session_id, content, cfg, session_mgr, ET.AGENT_MESSAGE, user_event_id=nudge["id"]),
        name=f"{platform.name}-nudge-{session_id}")
    logger.info(
        f"{platform.name}_reply_nudge",
        session=session_id,
        **adapter.log_fields(target),
        summon_id=summon_id,
        nudge_id=nudge["id"])
    return True

  if noticed(platform, events, summon_id):
    return False
  ok = await post_with_retry(adapter, target, _NO_REPLY_NOTICE, session_id=session_id)
  if not ok:
    return False  # the platform's post_gave_up log is the only trace; no marker, so the boot audit retries
  await session_mgr.persist_and_broadcast(
      session_id, {
          "type": ET.ASSISTANT_ERROR,
          "content": _NO_REPLY_CONTENT.format(platform=platform.display_name),
          platform.notice_key: {
              ET.INPUT_EVENT_ID: summon_id
          },
      })
  ack_clear(adapter, target, session_id)
  logger.info(f"{platform.name}_reply_notice", session=session_id, **adapter.log_fields(target), summon_id=summon_id)
  return True


async def deliver_done(
    adapter: ThreadAdapter, session_id: str, done: dict, cfg: CharlieBotConfig, session_mgr: SessionManager) -> bool:
  """Round-end audit for one finished round; True when it nudged or posted the notice.

  Called as a fire-and-forget task from ``persist_and_broadcast`` for every
  ``master_done``; returns False without acting unless the round belongs to a
  thread-bound session and answered a summon or a nudge. Guard-path dones
  (src/api/chat.py) carry no input_event_id and browser-typed rounds carry no
  summon block, so both leave the thread alone.
  """
  platform = adapter.platform
  meta = await session_mgr.get_session(session_id)
  if meta is None or getattr(meta, platform.origin_field) is None:
    return False
  input_event_ids = master_done_input_event_ids(done)
  if not input_event_ids:
    return False
  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  # The round-end audit targets the same input the reply binding does: the
  # newest thread-bearing one of the batch.
  bound = newest_thread_input(platform, events, input_event_ids)
  if bound is None:
    return False
  input_event_id, target = bound
  return await audit_round(adapter, session_id, events, target, input_event_id, cfg, session_mgr)


_LOST_SUMMON_NOTICE = "上一次召唤在服务重启时丢失了，没有被处理。需要的话请重新 @ 我一次。"

_LOST_SUMMON_CONTENT = "这条 {platform} 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。"


async def backfill_lost_summons(adapter: ThreadAdapter, cfg: CharlieBotConfig, session_mgr: SessionManager) -> int:
  """Boot pass over every thread-bound session; returns how many notices and nudges it produced.

  First the summons lost while queued: the startup replay covers ``ET.USER``
  only (src/core/init_master_recovery.py), so a summon injection sitting in the
  queue when the process died is picked up by nothing else and gets the
  lost-summon notice. Then the round-end audit over every finished round, which
  closes the crash windows between a done and its nudge, and between a nudge
  round's done and its notice. Every predicate reads the log, so a second pass
  finds nothing. Runs once per boot, after re-attach and replay have had their
  chance.
  """
  platform = adapter.platform
  from src.agents import master_cc  # lazy: mirrors the spawner import's cycle guard

  sessions = await session_mgr.list_sessions()  # archived included: a thread can be summoned again
  reported = 0
  for meta in sessions:
    if getattr(meta, platform.origin_field) is None:
      continue
    events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)
    lost = lost_summons(
        platform,
        events,
        owned=master_cc.queued_user_event_ids(meta.id),
        running=set(meta.master_run.user_event_ids) if meta.master_run else set())
    for ev in lost:
      # Persist the marker before posting: a crash in between costs one notice,
      # while posting first would re-post it on every boot until the marker landed.
      await session_mgr.persist_and_broadcast(
          meta.id, {
              "type": ET.ASSISTANT_ERROR,
              "content": _LOST_SUMMON_CONTENT.format(platform=platform.display_name),
              platform.backfill_key: {
                  ET.INPUT_EVENT_ID: ev["id"]
              },
          })
      block = ev[platform.name]
      await post_with_retry(adapter, block, _LOST_SUMMON_NOTICE, session_id=meta.id)
      ack_clear(adapter, block, meta.id)
      reported += 1
      logger.info(
          f"{platform.name}_backfill_lost_summon",
          session=meta.id,
          **adapter.log_fields(block),
          input_event_id=ev["id"])
    if lost:
      events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)

    dones = [ev for ev in events if ev.get("type") == ET.MASTER_DONE and master_done_input_event_ids(ev)]
    for done in dones:
      bound = newest_thread_input(platform, events, master_done_input_event_ids(done))
      if bound is None:
        continue
      done_input_id, target = bound
      if await audit_round(adapter, meta.id, events, target, done_input_id, cfg, session_mgr):
        reported += 1
        # The action appended an event the next done's predicates must see.
        events = await asyncio.to_thread(session_mgr.load_chat_events_sync, meta.id)
  return reported


# ---------------------------------------------------------------------------
# Thread follow: the persisted per-session wake a later thread message arms
# ---------------------------------------------------------------------------

# Thread-follow windows: a batch sleeps out this quiet delay from the newest
# message, and a chain never runs past this cap from its first message, so a
# steady trickle still flushes.
_FOLLOW_QUIET_SECONDS = 45
_FOLLOW_CHAIN_CAP_SECONDS = 300


async def armed_follow_triggers(platform: ThreadPlatform, trigger_mgr: TriggerManager,
                                session_id: str) -> list[PendingTrigger]:
  """The session's pending thread-follow trigger records (at most one by construction)."""
  return [
      t for t in await trigger_mgr.list_triggers(session_id)
      if t.status == TriggerStatus.PENDING and t.message.startswith(platform.follow_trigger_prefix)
  ]


async def cancel_armed_follow_triggers(platform: ThreadPlatform, trigger_mgr: TriggerManager, session_id: str) -> int:
  """Cancel every armed thread-follow trigger of the session; return how many."""
  armed = await armed_follow_triggers(platform, trigger_mgr, session_id)
  for trigger in armed:
    await trigger_mgr.cancel_trigger(session_id, trigger.id)
  return len(armed)


async def arm_follow_trigger(
    platform: ThreadPlatform,
    trigger_mgr: TriggerManager,
    session_id: str,
    *,
    floor: str,
    wake_label: Callable[[str], str],
    log_fields: dict,
) -> PendingTrigger | None:
  """Cancel-then-create the session's one persisted follow trigger; return the fresh record.

  The replaced record's ``created_at`` and floor id are read BEFORE the cancel:
  the new record is stamped with that same ``created_at`` — the chain's start —
  so a steady trickle still flushes at ``chain_start + _FOLLOW_CHAIN_CAP_SECONDS``
  no matter how many re-arms land, and the label keeps the chain's oldest
  unacked id as the floor — *wake_label* builds it after that re-arm floor is
  resolved. *log_fields* names the thread in the arm logs. Returns None without
  arming when the session was archived mid-flight: the thread-follow stops with
  its session.
  """
  chain_start: datetime | None = None
  for old in await armed_follow_triggers(platform, trigger_mgr, session_id):
    if chain_start is None:  # exactly one armed record exists by construction
      chain_start = old.created_at
      parsed_floor = follow_floor(old.message)
      if parsed_floor is not None:
        floor = parsed_floor
    await trigger_mgr.cancel_trigger(session_id, old.id)
  now = utc_now()
  start = chain_start or now
  fire_at = min(now + timedelta(seconds=_FOLLOW_QUIET_SECONDS), start + timedelta(seconds=_FOLLOW_CHAIN_CAP_SECONDS))
  delay = max(0, int((fire_at - now).total_seconds()))
  try:
    # The re-arm is never rejected by the pending-trigger limit: it replaces its
    # own record, and a thread's new message must never silently stop waking
    # its session. Its record still counts toward the limit.
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay,
        wake_label(floor),
        created_at=start,
        enforce_pending_limit=False,
    )
  except ArchivedSessionError as e:
    # The archive raced the re-arm between the caller's status check (and any
    # revival it performed) and the create: log and leave without a new
    # trigger record.
    logger.info(f"{platform.name}_follow_trigger_not_armed_archived", session=session_id, **log_fields, error=str(e))
    return None
  logger.info(
      f"{platform.name}_follow_trigger_armed",
      session=session_id,
      **log_fields,
      **{f"floor_{platform.id_label}": floor},
      fire_at=trigger.fire_at.isoformat(),
      chain_start=start.isoformat())
  return trigger


async def consume_mention(
    platform: ThreadPlatform, session_mgr: SessionManager, trigger_mgr: TriggerManager, session_id: str,
    mention_id: str) -> None:
  """The mention round consumes its own id: advance the watermark to it and cancel armed follows."""
  meta = await session_mgr.get_session(session_id)
  # A None here would silently skip the watermark advance, leaving the summon's own
  # mention permanently unread; the invariant break fails loudly instead.
  assert meta is not None, "unreachable: the summon path resolves the session just before this call"
  watermark = getattr(meta, platform.watermark_field)
  if watermark is None or platform.id_key(watermark) < platform.id_key(mention_id):
    setattr(meta, platform.watermark_field, mention_id)
    meta.updated_at = utc_now()
    await session_mgr.save_metadata(meta)
  cancelled = await cancel_armed_follow_triggers(platform, trigger_mgr, session_id)
  if cancelled:
    logger.info(f"{platform.name}_follow_trigger_cancelled_for_mention", session=session_id, cancelled=cancelled)


async def ensure_group(platform: ThreadPlatform, session_mgr: SessionManager, session_id: str, label: str) -> None:
  """Group a summon session under its label, unless it already has a group.

  The label is resolved once by the summon path (the single resolution point)
  and passed through — this function resolves nothing. It only writes when the
  session has a platform origin and an empty group (None or '') — an existing
  group is never overwritten. Best-effort: any failure logs a warning and is
  swallowed, so summon, round, and reply behavior are unaffected.
  """
  try:
    meta = await session_mgr.get_session(session_id)
    if meta is None or getattr(meta, platform.origin_field) is None or meta.group:
      return
    await session_mgr.set_group(session_id, label)
  except Exception as e:
    logger.warning(f"{platform.name}_group_assignment_failed", session=session_id, label=label, error=str(e))


_LOCAL_TZ = ZoneInfo(HOUSE_TIMEZONE)


def _local_time() -> str:
  """Local wall-clock stamp for a session display name."""
  return datetime.now(_LOCAL_TZ).strftime("%Y-%m-%d %H:%M")


async def accept_summon(
    adapter: ThreadAdapter,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    trigger_mgr: TriggerManager,
    *,
    session_id: str,
    label: str,
    origin: Any,
    block: dict,
    content: str,
    user: str | None,
) -> str:
  """Accept one summon: resolve the session, persist the summon, fire the round, light the ack eye.

  The entrypoint has already dropped the disallowed mention and resolved
  everything this path reads: the deterministic *session_id*, the *label*
  naming both the session and its group, the thread *origin*, the summon
  *block* (the platform's persisted marker payload), the summon prompt
  *content*, and the mentioning *user*. The session is created — named
  ``<label> <local time>``, born with *origin* under the platform's origin
  field —, unarchived, or reused; the mention round consumes its own id and
  any armed follow trigger is cancelled; the session is grouped under *label*;
  the summon event is persisted under the platform's key; the round and the
  ack eye fire as logged tasks. Returns the session id.
  """
  platform = adapter.platform
  fields = {**adapter.log_fields(block), f"{platform.name}_user": user}

  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    session_name = f"{label} {_local_time()}"
    await session_mgr.create_session(
        CreateSessionRequest(session_id=session_id, name=session_name, **{platform.origin_field: origin}))
    logger.info(f"{platform.name}_mention_session_created", **fields, session=session_id)
  elif session_meta.status == SessionStatus.ARCHIVED:
    await session_mgr.unarchive_session(session_id)
    logger.info(f"{platform.name}_mention_session_unarchived", **fields, session=session_id)
    # The unarchive write alone notifies nobody: an open sidebar refetches its
    # current filter only on a task-tree notification, so the revived session
    # reappears on Threads (or vanishes from Archive) without a manual refresh.
    await session_mgr.broadcast_task_tree_changed(session_id, "session_unarchived")
  else:
    logger.info(f"{platform.name}_mention_session_existing", **fields, session=session_id)

  await consume_mention(platform, session_mgr, trigger_mgr, session_id, block[platform.mention_key])

  await ensure_group(platform, session_mgr, session_id, label)

  evt = build_agent_message_event(content, from_session=session_id, from_session_name=platform.display_name)
  evt[platform.name] = block
  await session_mgr.persist_and_broadcast(session_id, evt)
  round_event_id = evt.get("id")
  logger.info(f"{platform.name}_mention_round_started", **fields, session=session_id, user_event_id=round_event_id)

  create_logged_task(
      trigger_master(session_id, content, cfg, session_mgr, ET.AGENT_MESSAGE, user_event_id=round_event_id),
      name=f"{platform.name}-round-{session_id}")
  create_logged_task(adapter.add_ack(block), name=f"{platform.name}-ack-{session_id}")
  return session_id


async def revive_and_arm_follow(
    platform: ThreadPlatform,
    adapter: ThreadAdapter,
    session_mgr: SessionManager,
    trigger_mgr: TriggerManager,
    meta: SessionMetadata,
    *,
    session_id: str,
    floor: str,
    log_fields: dict,
) -> PendingTrigger | None:
  """Revive an archived thread session, then arm its follow trigger; the fresh record.

  The one home of the revival sequence the two follow paths share (the live
  thread message and the reconnect backfill; the @ summon's unarchive branch
  repeats the unarchive-log-broadcast trio itself, arming nothing): an
  ARCHIVED session is unarchived first — the
  order is forced, ``create_trigger`` rejects an archived session — the
  unarchive is logged, and a ``task_tree_changed`` notification rides it, so
  an open sidebar refetches its current filter and the revived session
  reappears on Threads without a manual refresh. An ACTIVE session arms
  directly.
  """
  if meta.status == SessionStatus.ARCHIVED:
    await session_mgr.unarchive_session(session_id)
    logger.info(f"{platform.name}_follow_session_unarchived", session=session_id, **log_fields)
    await session_mgr.broadcast_task_tree_changed(session_id, "session_unarchived")
  origin = getattr(meta, platform.origin_field)
  link = await adapter.thread_link(origin)
  return await arm_follow_trigger(
      platform,
      trigger_mgr,
      session_id,
      floor=floor,
      wake_label=lambda floor: adapter.follow_wake_message(floor, link),
      log_fields=log_fields)


async def follow_message(
    adapter: ThreadAdapter,
    session_mgr: SessionManager,
    trigger_mgr: TriggerManager,
    session_id: str,
    message_id: str,
    *,
    origin_matches: Callable[[Any], bool],
) -> str | None:
  """Arm the session's follow trigger for one eligible thread message; the session id when armed.

  Guards 4 and 5 of the entrypoint's guard chain: the session exists, its
  origin is set and passes *origin_matches*, and the message id sorts strictly
  above the session's watermark (None passes). An ARCHIVED session is revived
  first (:func:`revive_and_arm_follow` — unarchived, logged, its task-tree
  change broadcast), so the arm lands exactly as for an active session and the
  revived session reappears on the Threads view. A passed message arms (or
  re-arms) the session's one persisted follow trigger — the chain floor is the
  message id, the wake label names the thread link — with guards 1 to 3 (the
  event's own shape: subtype, thread targeting, human sender) staying in the
  entrypoint, which reads them off the raw event.
  """
  platform = adapter.platform
  meta = await session_mgr.get_session(session_id)
  origin = getattr(meta, platform.origin_field) if meta is not None else None
  if meta is None or origin is None or not origin_matches(origin):
    return None
  watermark = getattr(meta, platform.watermark_field)
  if watermark is not None and not (platform.id_key(message_id) > platform.id_key(watermark)):
    return None
  trigger = await revive_and_arm_follow(
      platform,
      adapter,
      session_mgr,
      trigger_mgr,
      meta,
      session_id=session_id,
      floor=message_id,
      log_fields=adapter.log_fields(adapter.address_of(origin)))
  return session_id if trigger is not None else None


async def backfill_followed_threads(
    adapter: ThreadAdapter, cfg: CharlieBotConfig, session_mgr: SessionManager, trigger_mgr: TriggerManager) -> int:
  """Arm the follow trigger of every followed session holding unread messages; return the count.

  Runs once per successful (re)connection: one eligible read per followed
  thread closes the socket-down window, which persisted triggers cannot cover
  (no events arrive while the socket is down). Both statuses ride the listing:
  an archived session whose thread shows an unread eligible message is revived
  through the same sequence the live follow path uses
  (:func:`revive_and_arm_follow` — unarchive, log, broadcast, arm), so a
  message posted while the socket was down still wakes its session; an
  archived session with no unread message stays archived. A session whose
  thread shows no unread eligible message arms nothing, and each armed session
  arms exactly once, independent of its unread count.
  """
  platform = adapter.platform
  armed = 0
  # Both status filters ride the readonly listings: the shared cached metas
  # are handed out uncopied (the backfill only reads them) and the corpus
  # outside the followed threads is never copied+stamped.
  active, _ = await session_mgr.list_sessions_readonly(status=SessionStatus.ACTIVE)
  archived, _ = await session_mgr.list_sessions_readonly(status=SessionStatus.ARCHIVED)
  for meta in [*active, *archived]:
    origin = getattr(meta, platform.origin_field)
    if origin is None:
      continue
    try:
      unread = await unread_messages(adapter, origin, cfg, getattr(meta, platform.watermark_field))
      if not unread:
        continue
      trigger = await revive_and_arm_follow(
          platform,
          adapter,
          session_mgr,
          trigger_mgr,
          meta,
          session_id=meta.id,
          floor=unread[0].id,
          log_fields=adapter.log_fields(adapter.address_of(origin)))
      if trigger is not None:
        armed += 1
    except Exception as e:
      logger.warning(
          f"{platform.name}_follow_backfill_thread_failed",
          session=meta.id,
          **adapter.log_fields(adapter.address_of(origin)),
          error=str(e))
  if armed:
    logger.info(f"{platform.name}_follow_backfill_armed", sessions=armed)
  return armed


# The file-service URL prefixes: the mounted mounts with the trailing slash the
# rewrite gate matches on.
_FILE_URL_PREFIXES = tuple(mount + "/" for mount in FILE_SERVER_MOUNTS)

# The file-server URL shapes the reply path rewrites: scheme, any host, this
# server's port, one of the file-service prefixes, then the absolute filesystem
# path, with the query string and fragment carried onto the published URL unchanged.
_FILE_SERVER_URL_RE = re.compile(
    r"https?://(?P<host>\[[^\]\s]+\]|[^/\s:]+):(?P<port>\d+)/(?P<prefix>" +
    "|".join(mount.lstrip("/") for mount in FILE_SERVER_MOUNTS) +
    r")(?P<fs_path>/[^\s?#]*)(?P<query>\?[^\s#]*)?(?P<fragment>#[^\s]*)?")

# Any URL naming a port, for the application-route naming: the matches whose port is
# this server's and whose path is not a file-service prefix reach the operator alone.
_SERVER_PORT_URL_RE = re.compile(
    r"https?://(?:\[[^\]\s]+\]|[^/\s:]+):(?P<port>\d+)(?P<path>/[^\s?#]*)?"
    r"(?:\?[^\s#]*)?(?:#[^\s]*)?")


def rewrite_file_links(text: str, cfg: CharlieBotConfig, swap: Callable[[Path], str]) -> tuple[str, list[str]]:
  """Rewrite every file-server artifact URL the reply links through *swap*.

  A URL on this server's port under the canonical ``/absolute_filepath/`` prefix names an
  artifact file. Each existing target goes through *swap* — the text that
  replaces the URL before its query string and fragment are re-attached — so
  the reply carries links its readers can open. A match whose target file is
  gone raises ``ThreadReplyError`` naming the link — nothing of this reply
  posts — and a refusal raised by *swap* itself (for example an unconfigured
  publish lane, whose error text names the missing key) propagates the same
  way. Application-route URLs on the same port (``/diff``, ``/perfetto``,
  ...) are not static files, so they stay as written and come back named for
  the readback's operator-alone line.
  """
  routes: list[str] = []
  for m in _SERVER_PORT_URL_RE.finditer(text):
    if int(m.group("port")) != cfg.server.port:
      continue
    if (m.group("path") or "").startswith(_FILE_URL_PREFIXES):
      continue
    routes.append(m.group(0))

  out: list[str] = []
  cursor = 0
  for m in _FILE_SERVER_URL_RE.finditer(text):
    if int(m.group("port")) != cfg.server.port:
      continue
    fs_path = Path(unquote(m.group("fs_path")))
    if not fs_path.is_file():
      raise ThreadReplyError(422, f"reply links a file-server URL whose file is gone: {m.group(0)}")
    out.append(text[cursor:m.start()])
    out.append(swap(fs_path) + (m.group("query") or "") + (m.group("fragment") or ""))
    cursor = m.end()
  out.append(text[cursor:])
  return "".join(out), list(dict.fromkeys(routes))


# The page-delivery sentence every platform's summon-prompt line carries: every
# platform's reply goes through post_reply, whose rewrite runs publish_swap.
LINKED_PAGES_LINE = (
    "Linked pages: the reply path publishes each linked file-server page and swaps in its published URL.")


def publish_swap(cfg: CharlieBotConfig) -> Callable[[Path], str]:
  """The swap ``post_reply`` hands ``rewrite_file_links``: publish the file, return its published URL.

  A ``PublishError`` (publish lane unconfigured or not deployed) refuses the
  whole reply with 422; its text names the missing key or file.
  """

  def swap(fs_path: Path) -> str:
    try:
      return publish_artifact(fs_path, cfg).url
    except PublishError as e:
      raise ThreadReplyError(422, str(e)) from e

  return swap


def operator_only_note(links: list[str]) -> str | None:
  """The readback's one line naming the application-route links that reach the operator alone."""
  if not links:
    return None
  return "Application-route links stay as written; the operator alone can open them: " + ", ".join(links)
