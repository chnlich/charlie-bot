"""Platform-neutral shared-thread core.

The shared thread feature exists once per chat platform (Slack today, Discord
next): a summon starts a session bound to one thread, the round answers into
that thread, and the audit keeps summon and reply consistent across restarts.
This module holds what every platform shares — the platform description
(``ThreadPlatform``) and the pure, platform-neutral helpers. The per-platform
entrypoint (``src.core.slack_listener`` today) describes its platform with one
``ThreadPlatform`` instance built from its own constants and calls these
helpers with it; the async orchestration (summon handling, follow triggers,
reply, audit, backfill) stays in the entrypoint. Imports point one way: the
entrypoint imports this module, never the reverse.
"""

import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from src.core.config import CharlieBotConfig


@dataclass(frozen=True)
class ThreadPlatform:
  """The per-platform facts the shared helpers and the entrypoint agree on.

  ``name`` is the summon event-block key and the prefix of every persisted
  marker (``slack`` for Slack, ``discord`` later), so the derived keys spell
  the platform's marker payload keys and ack wire type from one string.
  ``id_key`` maps one message id to its ordering key: the platform's id sort
  (``str`` for Slack's dotted ts strings, ``int`` for Discord snowflakes).
  """

  name: str
  display_name: str
  reply_event_type: str
  reply_command: str
  max_post_chars: int
  follow_trigger_prefix: str
  id_key: Callable[[str], Any]

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


class ThreadReplyError(Exception):
  """A reply the endpoint refuses or the platform did not accept; ``status`` is the HTTP status it maps to.

  ``detail`` becomes the HTTPException detail; the 412 refusal carries the
  structured ``stale_thread`` payload instead of a plain string.
  """

  def __init__(self, status: int, detail: str | dict) -> None:
    super().__init__(str(detail))
    self.status = status
    self.detail = detail

# Fixed citation boundary appended to every platform-sourced summon prompt so
# the master scopes its citations to the thread and public content only.
CITATION_BOUNDARY = ("引用边界：只引用这条频道／线程本身、公开仓库、公开频道；"
                     "现场只读命令取得的运行状态可引用并附取数命令；已成文的私有内容不引用。")


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


def summon_prompt_tail(platform_line: str, cfg: CharlieBotConfig) -> str:
  """The summon prompt's fixed tail: the citation boundary, the PII red line, and the reply-format contract.

  Both docs (prompts/thread_reply_redline.md, prompts/thread_reply_format.md)
  are read fresh from prompts/ on every call — no caching, so an edit takes
  effect on the next summon. A missing or unreadable doc raises a ValueError
  naming the path; a prompt without both docs is never built. The
  platform-specific facts ride in through *platform_line*; the shared
  reply-format contract is reused unchanged across platforms.
  """
  red_line = load_prompt_doc(
      cfg.charlie_bot_repo,
      "thread_reply_redline.md",
      likely_cause="the repo checkout most likely predates the thread-reply-redline rename commit")
  reply_format = load_prompt_doc(
      cfg.charlie_bot_repo,
      "thread_reply_format.md",
      likely_cause="the repo checkout most likely predates the thread-reply-format rename commit")
  return f"{platform_line}\n\n{CITATION_BOUNDARY}\n{red_line}\n{reply_format}"


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


def newest_thread_input(
    platform: ThreadPlatform, events: list[dict], event_ids: list[str]) -> tuple[str, dict] | None:
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
