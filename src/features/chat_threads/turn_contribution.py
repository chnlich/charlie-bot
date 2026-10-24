"""The chat-threads package's turn contributions: the thread brief, and one per chat platform.

Every chat platform's contribution adds the same three things to a master turn — its round-end
audit after each MASTER_DONE, its part of the thread reply contract on manager rounds, and the
chat line its reply event renders as — so that behavior lives here once and each platform module
supplies only its own facts.
"""

from __future__ import annotations

import types
from collections.abc import Callable
from typing import Any

from src.features.chat_threads import thread_entry, thread_sessions
from src.infra import config, metadata_slots, models
from src.runtime import session_events, session_lifecycle, session_store, session_successor, task_prompts
from src.runtime.hooks import turn_contributions


class ChatThreadsTurnContribution(turn_contributions.TurnContribution):
  """Names ``thread_session.md`` as the workflow rules file."""

  def workflow_rules_file(self, meta: models.SessionMetadata) -> str | None:
    return "thread_session.md" if thread_sessions.is_thread_session(meta) else None


class ThreadTurnContribution(turn_contributions.TurnContribution):
  """One chat platform's part in a master turn.

  A subclass sets ``PLATFORM`` to the platform's ``ThreadPlatform`` facts and ``LISTENER`` to the
  listener module that owns its ``deliver_done``, and implements :meth:`contract_part`. The audit
  resolves ``deliver_done`` on that module at call time, so a test patches the module attribute.
  """

  PLATFORM: thread_entry.ThreadPlatform
  LISTENER: types.ModuleType

  def contract_part(self, origin: Any) -> str:
    """This platform's own part of the thread reply contract, built from the session's *origin*."""
    raise NotImplementedError

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    await self.LISTENER.deliver_done(
        meta.id, done_event, cfg, session_store.store(), session_lifecycle.lifecycle(), session_events.events(),
        session_successor.successor())

  def instruction_segments(self, meta: models.SessionMetadata, kind: str,
                           cfg: config.CharlieBotConfig) -> list[task_prompts.RuleSegment]:
    """The thread reply contract for a manager round of this platform's thread session.

    Only a manager round of a session whose origin is this platform's gets it
    (the summon line stays one line because the contract rides here once);
    every other session or kind gets none.
    """
    if kind not in task_prompts.MANAGER_KINDS:
      return []
    origin = getattr(metadata_slots.fields_of(meta, self.PLATFORM.name), self.PLATFORM.origin_field)
    if origin is None:
      return []
    text = thread_entry.thread_reply_contract(self.PLATFORM, self.contract_part(origin), cfg)
    return [thread_entry.thread_reply_segment(self.PLATFORM, text)]

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {self.PLATFORM.reply_event_type: self.reply_message}

  def reply_message(self, event: dict) -> dict:
    """The chat line one posted reply of this platform renders as."""
    return {"role": "system", "content": f"Posted to {self.PLATFORM.display_name}: {event.get('content', '')}"}


CONTRIBUTION = ChatThreadsTurnContribution()
