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
from typing import Any


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
