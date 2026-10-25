"""Platform-neutral shared-thread core.

The shared thread feature exists once per chat platform (Slack and Discord):
a summon starts a session bound to one thread, the round answers into
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
caller-claims derivation (``caller_reply_identity``) that feeds the reply's
request id and Run start time, the reply path (``post_reply``) with its
file-server-link gate (``assert_no_file_server_links``), the freshness gate
(``assert_thread_fresh``), the ack (``ack_messages``), the round-end audit
(``deliver_done`` over ``audit_round``), and the lost-summon backfill
(``backfill_lost_summons``).
The follow side wakes its session whatever the session's stored status: an
archived thread session is revived first (unarchived, logged, its task-tree
change broadcast), so any eligible thread message brings the session back to
the sidebar's Threads view. Each platform listener describes its platform
with one ``ThreadPlatform`` instance built from its own constants and hands
platform plus adapter to these functions. Imports point one way: the
entrypoint imports this module, never the reverse.
"""

import abc
import asyncio
import dataclasses
import datetime
import pathlib
import re
import uuid
import zoneinfo
from collections.abc import Callable
from typing import Any

from src.infra import config, locks, log_once, metadata_slots, models, tasks
from src.infra import event_types as ET
from src.runtime import (
    backend_pool,
    control_events,
    file_urls,
    master_cc_queue,
    master_cc_state,
    master_trigger,
    run_token,
    session_events,
    session_lifecycle,
    session_listing,
    session_store,
    session_successor,
    task_execution,
    task_prompts,
    task_sessions,
    triggers,
)
from src.runtime.api import deps, message_utils

logger = log_once.LazyStructlogLogger()

_SESSION_LOCKS: dict[str, asyncio.Lock] = {}


def session_lock_for(session_id: str) -> asyncio.Lock:
  """Return the in-process lock shared by thread replies, summons, and round audits for one session."""
  return locks.lock_for(_SESSION_LOCKS, session_id)


@dataclasses.dataclass(frozen=True)
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
  # names for posting a reply. The thread reply contract embeds that
  # contract, and the round-end audit reads the command off a summon's
  # content to recognize a summon issued under it (the older full-prompt
  # format; the current one is ``summon_line``).
  reply_command: str
  max_post_chars: int
  # The platform's scope doc under prompts/ (what personal information may
  # enter the platform's threads): read fresh into the session's thread
  # reply contract.
  scope_doc: str
  follow_trigger_prefix: str
  id_key: Callable[[str], Any]
  # Fields in this platform's registered session metadata slot: the thread
  # origin and newest consumed message id.
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
  # The whole summon input: one fixed line naming the platform and the
  # event (no link, no rules). The round-end audit reads it to recognize a
  # summon issued under the current contract.
  summon_line: str

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


@dataclasses.dataclass(frozen=True)
class ThreadMessage:
  """One thread message as the round side reads it.

  ``id`` is the platform's message id (a Slack ts, a Discord snowflake), ``user``
  its author (None when the platform names none), and ``text`` its text
  (empty when the platform names none). The remaining fields support keyed
  post lookup and the reply freshness gate.
  """

  id: str
  user: str | None
  text: str
  created_at: datetime.datetime | None = None
  metadata: dict | None = None
  bot_id: str | None = None
  is_bot: bool = False
  eligible: bool = True


class ThreadAdapter(abc.ABC):
  """The per-platform posting face the round side works through and the summon and follow side arms through.

  One subclass per entrypoint wraps the platform's client: posting into the
  thread, lighting and clearing the summon ack, reading the thread's
  messages, naming the thread (the ``address`` dict ``post`` accepts), and
  shaping the log fields that point at the thread. The platform description
  rides on the class, so every core helper reads it off the adapter it was
  handed.
  """

  platform: ThreadPlatform

  @abc.abstractmethod
  async def post(self, address: dict, text: str, post_key: str) -> None:
    """Post one text message into the thread *address* names; raises on failure -- ``post_with_retry`` catches it."""

  @abc.abstractmethod
  async def add_ack(self, block: dict) -> None:
    """Light the summon ack the summon block *block* carries (the mirror of ``remove_ack``)."""

  @abc.abstractmethod
  async def remove_ack(self, block: dict) -> None:
    """Clear the summon ack the summon block *block* carries."""

  @abc.abstractmethod
  async def read_thread(self, origin: Any, cfg: config.CharlieBotConfig) -> list[ThreadMessage]:
    """The thread *origin* names' messages, including bot posts, in platform order."""

  @abc.abstractmethod
  async def find_own_post_by_key(
      self,
      address: dict,
      post_key: str,
      content: str,
      created_after: datetime.datetime,
      cfg: config.CharlieBotConfig,
      *,
      messages: list[ThreadMessage] | None = None,
  ) -> bool:
    """Find this bot's post bound to *post_key*, reading the thread when no snapshot is supplied."""

  async def read_eligible(self, origin: Any, cfg: config.CharlieBotConfig) -> list[ThreadMessage]:
    """The thread *origin* names' eligible messages, in platform order."""
    return [message for message in await self.read_thread(origin, cfg) if message.eligible]

  @abc.abstractmethod
  def address_of(self, origin: Any) -> dict:
    """The address dict ``post`` accepts for the thread *origin* names."""

  @abc.abstractmethod
  def log_fields(self, address: dict) -> dict:
    """The log fields naming the thread *address* points at."""


class ThreadReplyError(Exception):
  """A reply the endpoint refuses or the platform did not accept; ``status`` is the HTTP status it maps to.

  ``detail`` becomes the HTTPException detail; the 412 refusal carries the
  structured ``stale_thread`` payload instead of a plain string.
  """

  def __init__(self, status: int, detail: str | dict) -> None:
    super().__init__(str(detail))
    self.status = status
    self.detail = detail


def load_prompt_doc(repo_root: pathlib.Path, name: str, *, likely_cause: str) -> str:
  """Read one prompts doc fresh from disk, raising a ValueError naming the path when missing.

  No caching, so an edit takes effect on the next summon. A missing or
  unreadable doc raises a ValueError naming the path and its most likely cause
  (mirrors the worker-prompt loader in src/runtime/spawner_prompt.py); a prompt
  without the doc is never built.
  """
  path = repo_root / "prompts" / name
  try:
    return path.read_text(encoding="utf-8").strip()
  except OSError as e:
    raise ValueError(f"{name} prompt not found at {path} — {likely_cause}") from e


def thread_reply_contract(platform: ThreadPlatform, platform_part: str, cfg: config.CharlieBotConfig) -> str:
  """The thread reply contract: *platform_part* joined with the three rule docs.

  A thread session's managed instructions carry this contract once, so the
  summon input stays one line and a rule edit takes effect on the next round.
  The three docs (the platform's scope doc, prompts/thread_reply_redline.md,
  prompts/thread_reply_format.md) are read fresh from prompts/ on every call —
  no caching. A missing or unreadable doc raises a ValueError naming the
  path; a session without all three docs is never launched. *platform_part*
  is the platform's own text — the platform line (reply command, per-message
  limit, how linked pages reach readers) and how the round reads the thread
  and finds the summon — and each platform's turn contribution states it and
  reuses the join unchanged. The scope doc name rides in through *platform*;
  the red line and the reply-format contract are shared unchanged across
  platforms.
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
  return f"{platform_part}\n\n{scope_doc}\n{red_line}\n{reply_format}"


def thread_reply_segment(platform: ThreadPlatform, text: str) -> task_prompts.RuleSegment:
  """The thread reply contract *text* as one rule segment of a session's managed instructions.

  The sources name the three docs ``thread_reply_contract`` joins into *text*,
  so the prompt-source audit reaches every prompt file the contract draws from.
  """
  return task_prompts.RuleSegment(
      text=text,
      sources=tuple(
          task_prompts.PromptSource(scope=task_prompts.SCOPE_BASE, source_ref=f"prompts/{ref}")
          for ref in (platform.scope_doc, "thread_reply_redline.md", "thread_reply_format.md")),
  )


def follow_wake_label(platform: ThreadPlatform, floor: str) -> str:
  """The armed follow trigger's label: the platform prefix and the chain *floor* id.

  ``floor=<id>`` is machine-readable: a re-arm parses it back
  (:func:`follow_floor`) so the wake always reads from the chain's oldest
  unacked message, independent of watermark state. The label carries no link
  and no rule text — the session's thread reply contract states how the round
  reads the thread and replies.
  """
  return f"{platform.follow_trigger_prefix} floor={floor}"


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
  Run record does not name it among its whole input list (alive but not
  followable), and no earlier backfill marked it. The marker is the
  ``<name>_backfill`` payload, never a synthetic master_done: that event is
  the cut point replay uses to decide which user messages are still unanswered.
  """
  answered: set[str] = set()
  for ev in events:
    if ev.get("type") == ET.MASTER_DONE:
      answered.update(message_utils.master_done_input_event_ids(ev))
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


async def post_with_retry(
    adapter: ThreadAdapter,
    address: dict,
    text: str,
    *,
    session_id: str,
    post_key: str,
    cfg: config.CharlieBotConfig,
    created_after: datetime.datetime,
    messages: list[ThreadMessage] | None = None,
) -> bool:
  """Find or post one keyed thread message, retrying failures; True when it is present.

  Exhausting the retries logs an error and returns False instead of raising:
  the caller decides what a failed post means (a 502 to the CLI, a notice
  left for the boot audit), and the event funnel is never broken by one.
  """
  name = adapter.platform.name
  if await adapter.find_own_post_by_key(address, post_key, text, created_after, cfg, messages=messages):
    return True
  attempts = len(_RETRY_DELAYS) + 1
  for attempt in range(attempts):
    try:
      await adapter.post(address, text, post_key)
      return True
    except Exception as e:
      if name == "slack" and await adapter.find_own_post_by_key(address, post_key, text, created_after, cfg):
        return True
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
  tasks.create_logged_task(adapter.remove_ack(block), name=f"{adapter.platform.name}-ack-clear-{session_id}")


# How much of an unread message's text the 412 refusal and the gate list carry.
_TEXT_PREVIEW_CHARS = 200


async def require_thread_session(
    platform: ThreadPlatform, session_id: str, store: session_store.SessionStore) -> models.SessionMetadata:
  """The session named by *session_id* when it exists and carries a platform thread.

  The reply-path preamble shared by ``assert_thread_fresh``, ``ack_messages``,
  and ``post_reply``. Refusals raise ``ThreadReplyError``: 404 unknown session,
  409 no platform thread.
  """
  meta = await store.get_session(session_id)
  if meta is None:
    raise ThreadReplyError(404, deps.SESSION_NOT_FOUND_DETAIL)
  if getattr(metadata_slots.fields_of(meta, platform.name), platform.origin_field) is None:
    raise ThreadReplyError(409, f"Session has no {platform.display_name} thread")
  return meta


async def unread_messages(adapter: ThreadAdapter, origin: Any, cfg: config.CharlieBotConfig,
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
    adapter: ThreadAdapter, session_id: str, cfg: config.CharlieBotConfig,
    store: session_store.SessionStore) -> list[ThreadMessage]:
  """Refuse the reply when eligible thread messages sit above the session's watermark.

  The reply-path gate, run inside ``post_reply`` while the session lock is held:
  a reply to a thread the running round has not acked through would answer a
  stale state, so nothing posts until the ack advances the watermark. Refusals
  raise ``ThreadReplyError``: 404 unknown session, 409 no platform thread, then
  412 with the structured ``stale_thread`` payload naming each unread message's
  id, user, and a text preview; with a None watermark the whole thread tail
  counts.
  """
  platform = adapter.platform
  meta = await require_thread_session(platform, session_id, store)
  fields = metadata_slots.fields_of(meta, platform.name)
  watermark = getattr(fields, platform.watermark_field)
  messages = await adapter.read_thread(getattr(fields, platform.origin_field), cfg)
  _assert_thread_fresh_messages(platform, watermark, messages)
  return messages


def _assert_thread_fresh_messages(
    platform: ThreadPlatform, watermark: str | None, messages: list[ThreadMessage]) -> None:
  """Raise the freshness refusal when the supplied thread read has unacked eligible messages."""
  unread = [message for message in messages if message.eligible]
  if watermark is not None:
    floor = platform.id_key(watermark)
    unread = [message for message in unread if platform.id_key(message.id) > floor]
  if not unread:
    return
  raise ThreadReplyError(
      412, {
          "error": "stale_thread",
          "new_messages":
              [
                  {
                      platform.id_label: message.id,
                      "user": message.user,
                      "text_preview": message.text[:_TEXT_PREVIEW_CHARS],
                  } for message in unread
              ],
          f"watermark_{platform.id_label}": watermark,
      })


# The reply budget the format contract states (prompts/thread_reply_format.md):
# replies should stay under this many chars, with the depth going to a page.
# This is the single measurement point for that budget.
_REPLY_BUDGET_CHARS = 500


def _event_created_at(event: dict) -> datetime.datetime:
  """Return the UTC timestamp when an event's request key was born."""
  value = event["timestamp"]
  parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
  return models.ensure_utc(parsed)


async def _persist_request_event(
    tree: Any,
    events: session_events.SessionEvents,
    session_id: str,
    request_id: str,
    event_type: str,
    content: str,
    fields: dict,
) -> dict:
  """Append one stable request event under the task tree's control lock."""
  event_id = control_events.stable_request_event_id(session_id, request_id)
  epoch = await events.prime_aggregator(session_id)
  async with tree.control_lock:
    existing = control_events.replay_request_event(
        tree.fact_history(session_id),
        event_id=event_id,
        request_id=request_id,
        event_type=event_type,
        content=content,
        fields=fields,
    )
    if existing is not None:
      return existing
    event = {
        "id": event_id,
        "request_id": request_id,
        "type": event_type,
        "content": content,
        **fields,
    }
    await tree.session_events.save_chat_event(session_id, event)
  await events.announce_appended_event(session_id, event, epoch=epoch)
  return event


def _reply_receipt(event: dict, operator_only_links: list[str], *, platform: ThreadPlatform) -> dict:
  """Build the CLI readback from a persisted thread reply event."""
  payload = event[f"{platform.name}_reply"]
  return {
      "posted": True,
      "text": event["content"],
      "operator_only_note": operator_only_note(operator_only_links),
      "chars": payload["chars"],
      "chunks": payload["chunks"],
      "over_budget": payload["chars"] > _REPLY_BUDGET_CHARS,
      "answers": payload["answers"],
  }


async def caller_reply_identity(
    caller: run_token.CallerIdentity, task_mgr: task_sessions.TaskTreeManager, *, verb: str, session_id: str,
    text: str) -> tuple[str | None, datetime.datetime | None]:
  """The request id and Run start time one reply derives from the caller's Run claims.

  *verb* names the reply kind inside the derived id, so a retried reply of the
  same text under the same Run binds to one id. A caller without claims posts on
  operator credentials and gets ``(None, None)``: the reply records no request
  identity. The Run start time floors the own-post lookup, so a post from an
  earlier Run never answers this reply.
  """
  if caller.claims is None:
    return None, None
  request_id = control_events.derived_agent_send_request_id(caller.claims.run_id, verb, session_id, text)
  run = await task_mgr.runs.get_run(caller.claims.session_id, caller.claims.run_id)
  assert run is not None and run.started_at is not None
  return request_id, run.started_at


async def post_reply(
    adapter: ThreadAdapter,
    session_id: str,
    text: str,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    events: session_events.SessionEvents,
    *,
    request_id: str | None = None,
    run_started_at: datetime.datetime | None = None,
    tree: Any | None = None) -> dict:
  """Post *text* to the session's thread and return the readback the CLI prints.

  The text posts exactly as written — nothing in it is rewritten. Before any
  chunk posts, ``assert_no_file_server_links`` refuses the whole reply with 422
  when the text still links a CharlieBot file-server URL: the round publishes
  the page with ``charliebot publish`` and writes the printed URL into the
  reply instead. Refusals raise ``ThreadReplyError``: 404 unknown session, 409 no
  platform thread, 422 blank text, 422 a file-server link, 502 when a chunk
  exhausted its retries (nothing is persisted then, so the caller can retry).
  A stale thread (412) refuses the whole reply before any chunk post. On success the platform's reply event records
  the text that went out, the summon it answers, and the chunk count; a reply that
  answers a summon clears that summon's ack. The readback carries that outbound
  text plus one line naming any application-route links, which stay as written and
  reach the operator alone.
  """
  platform = adapter.platform
  epoch = await events.prime_aggregator(session_id)
  async with session_lock_for(session_id):
    meta = await require_thread_session(platform, session_id, store)
    if not text.strip():
      raise ThreadReplyError(422, "Reply text is empty")

    task_tree = tree or task_execution.task_manager()
    chat_events = task_tree.fact_history(session_id)
    input_event_ids = master_cc_state.running_user_event_ids(session_id)
    bound = newest_thread_input(platform, chat_events, input_event_ids)
    answers = summon_of(bound[1], bound[0]) if bound is not None else None
    fields = metadata_slots.fields_of(meta, platform.name)
    address = adapter.address_of(getattr(fields, platform.origin_field))
    bodies = chunk_text(text, platform.max_post_chars)
    payload = {"answers": answers, "chars": len(text), "chunks": len(bodies)}
    event_id = control_events.stable_request_event_id(session_id, request_id) if request_id is not None else None
    if request_id is not None:
      existing = control_events.replay_request_event(
          chat_events,
          event_id=event_id,
          request_id=request_id,
          event_type=platform.reply_event_type,
          content=text,
      )
      if existing is not None:
        operator_only_links = application_route_links(text, cfg)
        return _reply_receipt(existing, operator_only_links, platform=platform)

    scan_context = file_urls.ScanContext(
        cfg.server.port,
        cfg.charliebot_home,
        store.session_dir(session_id),
        pathlib.Path.home(),
    )
    assert_no_file_server_links(text, scan_context)
    operator_only_links = application_route_links(text, cfg)
    thread_messages = await adapter.read_thread(getattr(fields, platform.origin_field), cfg)
    _assert_thread_fresh_messages(platform, getattr(fields, platform.watermark_field), thread_messages)
    post_request_id = request_id or f"operator-reply:{uuid.uuid4()}"
    created_after = run_started_at or models.utc_now()
    for index, body in enumerate(bodies, start=1):
      ok = await post_with_retry(
          adapter,
          address,
          body,
          session_id=session_id,
          post_key=f"{post_request_id}#{index}",
          cfg=cfg,
          created_after=created_after,
          messages=thread_messages,
      )
      if not ok:
        raise ThreadReplyError(
            502, f"{platform.display_name} did not accept chunk {index} of {len(bodies)} after "
            f"{len(_RETRY_DELAYS) + 1} attempts; nothing was persisted")

    event = {
        "type": platform.reply_event_type,
        "content": text,
        platform.reply_event_type: payload,
    }
    if event_id is not None:
      event["id"] = event_id
      event["request_id"] = request_id
    async with task_tree.control_lock:
      if request_id is not None:
        existing = control_events.replay_request_event(
            task_tree.fact_history(session_id),
            event_id=event_id,
            request_id=request_id,
            event_type=platform.reply_event_type,
            content=text,
        )
        if existing is not None:
          return _reply_receipt(existing, operator_only_links, platform=platform)
      await task_tree.session_events.save_chat_event(session_id, event)
    await events.announce_appended_event(session_id, event, epoch=epoch)
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
    adapter: ThreadAdapter, session_id: str, message_ids: list[str], cfg: config.CharlieBotConfig,
    store: session_store.SessionStore, events: session_events.SessionEvents) -> dict:
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
  meta = await require_thread_session(platform, session_id, store)
  ids = sorted(set(message_ids), key=platform.id_key)
  if not ids:
    raise ThreadReplyError(422, "message_ids is empty")
  fields = metadata_slots.fields_of(meta, platform.name)
  eligible = {m.id for m in await adapter.read_eligible(getattr(fields, platform.origin_field), cfg)}
  unknown = [i for i in ids if i not in eligible]
  if unknown:
    raise ThreadReplyError(422, f"Unknown or ineligible message id: {unknown[0]}")
  watermark = getattr(fields, platform.watermark_field)
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
    metadata_slots.set_fields(meta, platform.name, **{platform.watermark_field: watermark})
    meta.updated_at = models.utc_now()
    await store.save_metadata(meta)
  await events.persist_and_broadcast(
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
# asked once whether the thread should hear something. No link and no command
# usage: the thread reply contract in the session's managed instructions
# states the reply path.
_NUDGE_TEMPLATE = (
    "{platform} thread: the round answering this mention ended without posting a reply. Decide now:\n"
    "when the thread should hear something, post a reply; when there is nothing to say, end this\n"
    "round and the thread gets a one-line notice pointing to this session.")

# Thread-visible end state after a summon round and its nudge round both posted nothing.
_NO_REPLY_NOTICE = (
    "No reply was posted for this mention; the details are in the session log. "
    "Mention me again for a thread answer.")

# Session-log content of the notice marker; its notice payload names the summon it closes.
_NO_REPLY_CONTENT = "This {platform} mention got no reply from its round or the nudge round; the thread was told so."


async def audit_round(
    adapter: ThreadAdapter,
    session_id: str,
    input_event_id: str,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    successor: session_successor.SessionSuccessor,
    tree: Any | None = None) -> bool:
  """Serialize one round audit and decide from the session's current log."""
  async with session_lock_for(session_id):
    current = await asyncio.to_thread(events.load_chat_events_sync, session_id)
    current_target = event_by_id(current, input_event_id)
    if current_target is None or not isinstance(current_target.get(adapter.platform.name), dict):
      return False
    return await _audit_round_locked(
        adapter, session_id, current, current_target[adapter.platform.name], input_event_id, cfg, store, lifecycle,
        events, successor, tree or task_execution.task_manager())


async def _audit_round_locked(
    adapter: ThreadAdapter, session_id: str, chat_events: list[dict], target: dict, input_event_id: str,
    cfg: config.CharlieBotConfig, store: session_store.SessionStore, lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents, successor: session_successor.SessionSuccessor, tree: Any) -> bool:
  """Act on one finished round whose input carried a summon block; True when it acted.

  Reads the log for the round's summon: a reply answering it ends the audit. A
  summon round without one gets a nudge (once: a second done for the same
  summon finds the nudge event); a nudge round without one gets the thread
  notice (once: the notice marker, persisted only after the post
  succeeded, so a failed post leaves the boot audit a retry). A summon issued
  under the earlier marker contract (its content is neither the platform's
  summon line nor the older full prompt naming the reply command) is outside
  this audit.
  """
  platform = adapter.platform
  summon_id = summon_of(target, input_event_id)
  summon = event_by_id(chat_events, summon_id)
  # The current contract's summon is the one-line input; the older full-prompt
  # format named the reply command in its content. A summon from the earlier
  # marker contract (neither) stays outside this audit.
  content = (summon or {}).get("content") or ""
  if content != platform.summon_line and platform.reply_command not in content:
    return False
  if replied(platform, chat_events, summon_id):
    return False

  if "nudge_of" not in target:
    if nudged(platform, chat_events, summon_id):
      return False
    content = _NUDGE_TEMPLATE.format(platform=platform.display_name)
    nudge_request_id = f"{platform.name}-nudge:{summon_id}"
    nudge_block = {key: target[key] for key in platform.block_keys if key in target}
    nudge_block["nudge_of"] = summon_id
    nudge = await tree.dispatch.admit_input(
        session_id,
        event_type=ET.AGENT_MESSAGE,
        content=content,
        actor="agent",
        from_session=session_id,
        from_session_name=platform.display_name,
        request_id=nudge_request_id,
        fields={platform.name: nudge_block},
    )
    master_trigger.spawn_master_turn(
        session_id,
        content,
        store,
        successor,
        lifecycle,
        input_id=nudge["id"],
        event_type=ET.AGENT_MESSAGE,
        actor="agent",
        from_session=session_id,
        from_session_name=platform.display_name,
        task_name=f"{platform.name}-nudge-{session_id}")
    logger.info(
        f"{platform.name}_reply_nudge",
        session=session_id,
        **adapter.log_fields(target),
        summon_id=summon_id,
        nudge_id=nudge["id"])
    return True

  if noticed(platform, chat_events, summon_id):
    return False
  summon = event_by_id(chat_events, summon_id)
  notice_request_id = f"{platform.name}-notice:{summon_id}"
  ok = await post_with_retry(
      adapter,
      target,
      _NO_REPLY_NOTICE,
      session_id=session_id,
      post_key=f"{notice_request_id}#1",
      cfg=cfg,
      created_after=_event_created_at(summon),
  )
  if not ok:
    return False  # the platform's post_gave_up log is the only trace; no marker, so the boot audit retries
  await _persist_request_event(
      tree,
      events,
      session_id,
      notice_request_id,
      ET.ASSISTANT_ERROR,
      _NO_REPLY_CONTENT.format(platform=platform.display_name),
      {platform.notice_key: {
          ET.INPUT_EVENT_ID: summon_id
      }},
  )
  ack_clear(adapter, target, session_id)
  logger.info(f"{platform.name}_reply_notice", session=session_id, **adapter.log_fields(target), summon_id=summon_id)
  return True


async def deliver_done(
    adapter: ThreadAdapter,
    session_id: str,
    done: dict,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    successor: session_successor.SessionSuccessor,
    tree: Any | None = None) -> bool:
  """Round-end audit for one finished round; True when it nudged or posted the notice.

  Called as a fire-and-forget task from ``persist_and_broadcast`` for every
  ``master_done``; returns False without acting unless the round belongs to a
  thread-bound session and answered a summon or a nudge. Guard-path dones
  (src/runtime/api/chat.py) carry no input_event_id and browser-typed rounds carry no
  summon block, so both leave the thread alone.
  """
  platform = adapter.platform
  meta = await store.get_session(session_id)
  if meta is None or getattr(metadata_slots.fields_of(meta, platform.name), platform.origin_field) is None:
    return False
  input_event_ids = message_utils.master_done_input_event_ids(done)
  if not input_event_ids:
    return False
  chat_events = await asyncio.to_thread(events.load_chat_events_sync, session_id)
  # The round-end audit targets the same input the reply binding does: the
  # newest thread-bearing one of the batch.
  bound = newest_thread_input(platform, chat_events, input_event_ids)
  if bound is None:
    return False
  input_event_id, _ = bound
  return await audit_round(adapter, session_id, input_event_id, cfg, store, lifecycle, events, successor, tree)


_LOST_SUMMON_NOTICE = "上一次召唤在服务重启时丢失了，没有被处理。需要的话请重新 @ 我一次。"

_LOST_SUMMON_CONTENT = "这条 {platform} 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。"


async def backfill_lost_summons(
    adapter: ThreadAdapter,
    cfg: config.CharlieBotConfig,
    listing: session_listing.SessionListing,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    successor: session_successor.SessionSuccessor,
    tree: Any | None = None) -> int:
  """Boot pass over every thread-bound session; returns how many notices and nudges it produced.

  First the summons lost while queued: the startup replay covers ``ET.USER``
  only during startup reconciliation, so a summon injection sitting in the
  queue when the process died is picked up by nothing else and gets the
  lost-summon notice. Then the round-end audit over every finished round, which
  closes the crash windows between a done and its nudge, and between a nudge
  round's done and its notice. Every predicate reads the log, so a second pass
  finds nothing. Runs once per boot, after re-attach and replay have had their
  chance.
  """
  platform = adapter.platform
  task_tree = tree or task_execution.task_manager()

  sessions = await listing.list_sessions()  # archived included: a thread can be summoned again
  reported = 0
  for meta in sessions:
    if getattr(metadata_slots.fields_of(meta, platform.name), platform.origin_field) is None:
      continue
    async with session_lock_for(meta.id):
      chat_events = await asyncio.to_thread(events.load_chat_events_sync, meta.id)
      lost = lost_summons(
          platform,
          chat_events,
          owned=master_cc_queue.queued_user_event_ids(meta.id),
          running=set(master_cc_state.running_user_event_ids(meta.id)))
      for ev in lost:
        block = ev[platform.name]
        request_id = f"{platform.name}-lost:{ev['id']}"
        ok = await post_with_retry(
            adapter,
            block,
            _LOST_SUMMON_NOTICE,
            session_id=meta.id,
            post_key=f"{request_id}#1",
            cfg=cfg,
            created_after=_event_created_at(ev),
        )
        if not ok:
          continue
        await _persist_request_event(
            task_tree,
            events,
            meta.id,
            request_id,
            ET.ASSISTANT_ERROR,
            _LOST_SUMMON_CONTENT.format(platform=platform.display_name),
            {platform.backfill_key: {
                ET.INPUT_EVENT_ID: ev["id"]
            }},
        )
        ack_clear(adapter, block, meta.id)
        reported += 1
        logger.info(
            f"{platform.name}_backfill_lost_summon",
            session=meta.id,
            **adapter.log_fields(block),
            input_event_id=ev["id"])
      chat_events = await asyncio.to_thread(events.load_chat_events_sync, meta.id)

    dones = [
        ev for ev in chat_events if ev.get("type") == ET.MASTER_DONE and message_utils.master_done_input_event_ids(ev)
    ]
    for done in dones:
      bound = newest_thread_input(platform, chat_events, message_utils.master_done_input_event_ids(done))
      if bound is None:
        continue
      done_input_id, _ = bound
      if await audit_round(adapter, meta.id, done_input_id, cfg, store, lifecycle, events, successor, task_tree):
        reported += 1
        # The action appended an event the next done's predicates must see.
        chat_events = await asyncio.to_thread(events.load_chat_events_sync, meta.id)
  return reported


# ---------------------------------------------------------------------------
# Thread follow: the persisted per-session wake a later thread message arms
# ---------------------------------------------------------------------------

# Thread-follow windows: a batch sleeps out this quiet delay from the newest
# message, and a chain never runs past this cap from its first message, so a
# steady trickle still flushes.
_FOLLOW_QUIET_SECONDS = 10
_FOLLOW_CHAIN_CAP_SECONDS = 300


async def armed_follow_triggers(platform: ThreadPlatform, trigger_mgr: triggers.TriggerManager,
                                session_id: str) -> list[models.PendingTrigger]:
  """The session's pending thread-follow trigger records (at most one by construction)."""
  return [
      t for t in await trigger_mgr.list_triggers(session_id)
      if t.status == models.TriggerStatus.PENDING and t.message.startswith(platform.follow_trigger_prefix)
  ]


async def cancel_armed_follow_triggers(
    platform: ThreadPlatform, trigger_mgr: triggers.TriggerManager, session_id: str) -> int:
  """Cancel every armed thread-follow trigger of the session; return how many."""
  armed = await armed_follow_triggers(platform, trigger_mgr, session_id)
  for trigger in armed:
    await trigger_mgr.cancel_trigger(session_id, trigger.id)
  return len(armed)


async def arm_follow_trigger(
    platform: ThreadPlatform,
    trigger_mgr: triggers.TriggerManager,
    session_id: str,
    *,
    floor: str,
    log_fields: dict,
) -> models.PendingTrigger | None:
  """Cancel-then-create the session's one persisted follow trigger; return the fresh record.

  The replaced record's ``created_at`` and floor id are read BEFORE the cancel:
  the new record is stamped with that same ``created_at`` — the chain's start —
  so a steady trickle still flushes at ``chain_start + _FOLLOW_CHAIN_CAP_SECONDS``
  no matter how many re-arms land, and the label keeps the chain's oldest
  unacked id as the floor (:func:`follow_wake_label` builds it after that
  re-arm floor is resolved). *log_fields* names the thread in the arm logs.
  Returns None without arming when the session was archived mid-flight: the
  thread-follow stops with its session.
  """
  chain_start: datetime.datetime | None = None
  for old in await armed_follow_triggers(platform, trigger_mgr, session_id):
    if chain_start is None:  # exactly one armed record exists by construction
      chain_start = old.created_at
      parsed_floor = follow_floor(old.message)
      if parsed_floor is not None:
        floor = parsed_floor
    await trigger_mgr.cancel_trigger(session_id, old.id)
  now = models.utc_now()
  start = chain_start or now
  fire_at = min(
      now + datetime.timedelta(seconds=_FOLLOW_QUIET_SECONDS),
      start + datetime.timedelta(seconds=_FOLLOW_CHAIN_CAP_SECONDS))
  delay = max(0, int((fire_at - now).total_seconds()))
  try:
    # The re-arm is never rejected by the pending-trigger limit: it replaces its
    # own record, and a thread's new message must never silently stop waking
    # its session. Its record still counts toward the limit.
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay,
        follow_wake_label(platform, floor),
        created_at=start,
        enforce_pending_limit=False,
    )
  except triggers.ArchivedSessionError as e:
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
    platform: ThreadPlatform, store: session_store.SessionStore, trigger_mgr: triggers.TriggerManager, session_id: str,
    mention_id: str) -> None:
  """The mention round consumes its own id: advance the watermark to it and cancel armed follows."""
  meta = await store.get_session(session_id)
  # A None here would silently skip the watermark advance, leaving the summon's own
  # mention permanently unread; the invariant break fails loudly instead.
  assert meta is not None, "unreachable: the summon path resolves the session just before this call"
  fields = metadata_slots.fields_of(meta, platform.name)
  watermark = getattr(fields, platform.watermark_field)
  if watermark is None or platform.id_key(watermark) < platform.id_key(mention_id):
    metadata_slots.set_fields(meta, platform.name, **{platform.watermark_field: mention_id})
    meta.updated_at = models.utc_now()
    await store.save_metadata(meta)
  cancelled = await cancel_armed_follow_triggers(platform, trigger_mgr, session_id)
  if cancelled:
    logger.info(f"{platform.name}_follow_trigger_cancelled_for_mention", session=session_id, cancelled=cancelled)


async def ensure_group(
    platform: ThreadPlatform, store: session_store.SessionStore, lifecycle: session_lifecycle.SessionLifecycle,
    session_id: str, label: str) -> None:
  """Group a summon session under its label, unless it already has a group.

  The label is resolved once by the summon path (the single resolution point)
  and passed through — this function resolves nothing. It only writes when the
  session has a platform origin and an empty group (None or '') — an existing
  group is never overwritten. Best-effort: any failure logs a warning and is
  swallowed, so summon, round, and reply behavior are unaffected.
  """
  try:
    meta = await store.get_session(session_id)
    if (meta is None or getattr(metadata_slots.fields_of(meta, platform.name), platform.origin_field) is None or
        meta.group):
      return
    await lifecycle.set_group(session_id, label)
  except Exception as e:
    logger.warning(f"{platform.name}_group_assignment_failed", session=session_id, label=label, error=str(e))


_LOCAL_TZ = zoneinfo.ZoneInfo(config.HOUSE_TIMEZONE)


def _local_time() -> str:
  """Local wall-clock stamp for a session display name."""
  return datetime.datetime.now(_LOCAL_TZ).strftime("%Y-%m-%d %H:%M")


async def _route_thread_session(
    adapter: ThreadAdapter,
    cfg: config.CharlieBotConfig,
    origin: Any,
) -> tuple[str | None, backend_pool.BackendRoute | None]:
  """The backend a new thread session runs on, and the router's route when one ran.

  The whole thread is read through the adapter before the session exists, and every readable
  message goes to the router in time order, untrimmed, under the ``thread`` role. An empty route
  list falls back to the pool; a router error or a pool with no available member posts the error
  into the thread and raises — no session is created.
  """
  routing = cfg.backends.routing if cfg is not None and cfg.backends is not None else None
  if routing is None:
    return None, None
  messages = await adapter.read_eligible(origin, cfg)
  text = "\n".join(f"{m.user}: {m.text}" if m.user else m.text for m in messages)
  candidates = [option.id for option in cfg.backends.options]
  try:
    route = await backend_pool.route(cfg, role="thread", text=text, candidates=candidates)
  except backend_pool.RouterError as e:
    await _post_thread_error(adapter, origin, f"backend router failed: {e}")
    raise
  option = None
  if route.backends:
    option = backend_pool.pick_ordered(cfg, caller="thread", backends=route.backends)
  if option is None:
    option = backend_pool.select(cfg, caller="thread")
  if option is None:
    message = f"no backend is available for the thread session: {backend_pool.recovery_report(cfg)}"
    await _post_thread_error(adapter, origin, message)
    raise backend_pool.RouterError(message)
  return option.id, route


async def _post_thread_error(adapter: ThreadAdapter, origin: Any, text: str) -> None:
  """Post one error line into the thread *origin* names, unacked — the summon produced no session."""
  await adapter.post(adapter.address_of(origin), text, f"{adapter.platform.name}-routerror:{uuid.uuid4()}#1")


async def accept_summon(
    adapter: ThreadAdapter,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    successor: session_successor.SessionSuccessor,
    trigger_mgr: triggers.TriggerManager,
    *,
    session_id: str,
    label: str,
    origin: Any,
    block: dict,
    content: str,
    user: str | None,
    tree: Any | None = None,
) -> str:
  """Accept one summon: resolve the session, persist the summon, fire the round, light the ack eye.

  The entrypoint has already dropped the disallowed mention and resolved
  everything this path reads: the deterministic *session_id*, the *label*
  naming both the session and its group, the thread *origin*, the summon
  *block* (the platform's persisted marker payload), the summon line
  *content*, and the mentioning *user*. The session is created — a manager
  root named ``<label> <local time>``, born with *origin* under the platform's
  origin field —, unarchived, or reused; the mention round consumes its own id and
  any armed follow trigger is cancelled; the session is grouped under *label*;
  the summon event is persisted under the platform's key; the round and the
  ack eye fire as logged tasks. Returns the session id.
  """
  platform = adapter.platform
  fields = {**adapter.log_fields(block), f"{platform.name}_user": user}
  request_id = f"{platform.name}-mention:{block[platform.mention_key]}"
  event_id = control_events.stable_request_event_id(session_id, request_id)
  task_tree = tree or task_execution.task_manager()

  async with session_lock_for(session_id):
    session_meta = await store.get_session(session_id)
    if session_meta is not None:
      existing = control_events.replay_request_event(
          task_tree.fact_history(session_id),
          event_id=event_id,
          request_id=request_id,
          event_type=ET.AGENT_MESSAGE,
          content=content,
          fields={platform.name: block},
      )
      if existing is not None:
        return session_id

    if session_meta is None:
      session_name = f"{label} {_local_time()}"
      backend_id, route = await _route_thread_session(adapter, cfg, origin)
      await task_tree.create_task(
          request_id=f"{platform.name}-summon-{session_id}",
          task_parent_id=None,
          profile="manager",
          task=None,
          name=session_name,
          backend=backend_id,
          group=None,
          session_id=session_id,
          slot_values={platform.origin_field: origin},
          caller="system",
      )
      if route is not None:
        await task_tree.update_slot_fields(session_id, backend_pool.METADATA_OWNER, route=route.model_dump(mode="json"))
      logger.info(f"{platform.name}_mention_session_created", **fields, session=session_id)
    elif session_meta.status == models.SessionStatus.ARCHIVED:
      await lifecycle.unarchive_session(session_id)
      logger.info(f"{platform.name}_mention_session_unarchived", **fields, session=session_id)
      await events.broadcast_task_tree_changed(session_id, "session_unarchived")
    else:
      logger.info(f"{platform.name}_mention_session_existing", **fields, session=session_id)

    await consume_mention(platform, store, trigger_mgr, session_id, block[platform.mention_key])
    await ensure_group(platform, store, lifecycle, session_id, label)

    evt = await task_tree.dispatch.admit_input(
        session_id,
        event_type=ET.AGENT_MESSAGE,
        content=content,
        actor="agent",
        from_session=session_id,
        from_session_name=platform.display_name,
        request_id=request_id,
        fields={platform.name: block},
    )
    round_event_id = evt["id"]
    logger.info(f"{platform.name}_mention_round_started", **fields, session=session_id, user_event_id=round_event_id)

    master_trigger.spawn_master_turn(
        session_id,
        content,
        store,
        successor,
        lifecycle,
        input_id=round_event_id,
        event_type=ET.AGENT_MESSAGE,
        actor="agent",
        from_session=session_id,
        from_session_name=platform.display_name,
        task_name=f"{platform.name}-round-{session_id}")
    tasks.create_logged_task(adapter.add_ack(block), name=f"{platform.name}-ack-{session_id}")
    return session_id


async def revive_and_arm_follow(
    platform: ThreadPlatform,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    trigger_mgr: triggers.TriggerManager,
    meta: models.SessionMetadata,
    *,
    session_id: str,
    floor: str,
    log_fields: dict,
) -> models.PendingTrigger | None:
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
  if meta.status == models.SessionStatus.ARCHIVED:
    await lifecycle.unarchive_session(session_id)
    logger.info(f"{platform.name}_follow_session_unarchived", session=session_id, **log_fields)
    await events.broadcast_task_tree_changed(session_id, "session_unarchived")
  return await arm_follow_trigger(platform, trigger_mgr, session_id, floor=floor, log_fields=log_fields)


async def follow_message(
    adapter: ThreadAdapter,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    trigger_mgr: triggers.TriggerManager,
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
  re-arms) the session's one persisted follow trigger — the chain floor is
  the message id — with guards 1 to 3 (the event's own shape: subtype, thread
  targeting, human sender) staying in the entrypoint, which reads them off
  the raw event.
  """
  platform = adapter.platform
  meta = await store.get_session(session_id)
  if meta is None:
    return None
  fields = metadata_slots.fields_of(meta, platform.name)
  origin = getattr(fields, platform.origin_field)
  if origin is None or not origin_matches(origin):
    return None
  watermark = getattr(fields, platform.watermark_field)
  if watermark is not None and not (platform.id_key(message_id) > platform.id_key(watermark)):
    return None
  trigger = await revive_and_arm_follow(
      platform,
      lifecycle,
      events,
      trigger_mgr,
      meta,
      session_id=session_id,
      floor=message_id,
      log_fields=adapter.log_fields(adapter.address_of(origin)))
  return session_id if trigger is not None else None


async def backfill_followed_threads(
    adapter: ThreadAdapter, cfg: config.CharlieBotConfig, listing: session_listing.SessionListing,
    lifecycle: session_lifecycle.SessionLifecycle, events: session_events.SessionEvents,
    trigger_mgr: triggers.TriggerManager) -> int:
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
  active, _ = await listing.list_sessions_readonly(status=models.SessionStatus.ACTIVE)
  archived, _ = await listing.list_sessions_readonly(status=models.SessionStatus.ARCHIVED)
  for meta in [*active, *archived]:
    fields = metadata_slots.fields_of(meta, platform.name)
    origin = getattr(fields, platform.origin_field)
    if origin is None:
      continue
    try:
      unread = await unread_messages(adapter, origin, cfg, getattr(fields, platform.watermark_field))
      if not unread:
        continue
      trigger = await revive_and_arm_follow(
          platform,
          lifecycle,
          events,
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


def assert_no_file_server_links(text: str, scan_context: file_urls.ScanContext | None = None) -> None:
  """Refuse the reply when *text* still links a CharlieBot file-server URL.

  The reply path posts the text as written, so a file-server link — the
  password-protected file browser, or a retired or local path, would reach
  thread readers as a link they cannot open. The first finding raises
  ``ThreadReplyError`` (422) naming the link and the ``charliebot publish``
  command whose printed URL replaces it; nothing posts.
  """
  for finding in file_urls.scan_file_links(text, scan_context):
    if finding.form != file_urls.FileLinkForm.CANONICAL and not finding.dead:
      continue
    publish_path = str(finding.path) if finding.path is not None else finding.link
    if finding.form == file_urls.FileLinkForm.CANONICAL:
      detail = f"reply links a CharlieBot file-server URL, which opens only behind the access key: {finding.link}. "
    else:
      detail = f"reply links a retired or local file path that thread readers cannot open: {finding.link}. "
    raise ThreadReplyError(
        422, detail + f"Publish the page with `charliebot publish {publish_path}` and write the URL the command "
        "prints into the reply")


def application_route_links(text: str, cfg: config.CharlieBotConfig) -> list[str]:
  """The application-route URLs on this server's port that the reply names for the operator.

  ``/diff``, ``/perfetto`` and the server's other pages are not static files:
  they stay as written (the operator's devices reach them, a thread reader's do
  not), and the readback's operator-alone line names each once, in
  first-appearance order.
  """
  routes = [
      m.group(0)
      for m in file_urls._URL_RE.finditer(text)
      if m.group("port") is not None and int(m.group("port")) == cfg.server.port
  ]
  return list(dict.fromkeys(routes))


# The page-delivery sentence every platform's summon-prompt line carries: the
# reply path posts the text as written and refuses a file-server link.
LINKED_PAGES_LINE = (
    "Linked pages: publish each page with `charliebot publish <path>` and write the URL it prints "
    "into the reply; a CharlieBot file-server link refuses the reply.")


def operator_only_note(links: list[str]) -> str | None:
  """The readback's one line naming the application-route links that reach the operator alone."""
  if not links:
    return None
  return "Application-route links stay as written; the operator alone can open them: " + ", ".join(links)
