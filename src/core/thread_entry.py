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
