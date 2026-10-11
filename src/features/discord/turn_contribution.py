"""The Discord package's turn contribution: the round-end audit, the thread reply contract, and the ``discord_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable

from src.features.chat_threads import thread_entry
from src.features.discord import discord_listener, event_types
from src.infra import config, metadata_slots, models
from src.runtime import session_events, session_lifecycle, session_store, session_successor, task_prompts
from src.runtime.hooks import turn_contributions


def _discord_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Discord: {event.get('content', '')}",
  }


class DiscordTurnContribution(turn_contributions.TurnContribution):
  """Runs the Discord round-end audit after every MASTER_DONE, carries the thread reply contract, and renders ``discord_reply`` events."""

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    await discord_listener.deliver_done(
        meta.id, done_event, cfg, session_store.store(), session_lifecycle.lifecycle(), session_events.events(),
        session_successor.successor())

  def instruction_segments(self, meta: models.SessionMetadata, kind: str,
                           cfg: config.CharlieBotConfig) -> list[task_prompts.RuleSegment]:
    """The thread reply contract for a manager round of a Discord thread session.

    Only a manager round of a session whose origin is this platform's gets it
    (the summon line stays one line because the contract rides here once);
    every other session or kind gets none.
    """
    if kind not in task_prompts.MANAGER_KINDS:
      return []
    if metadata_slots.fields_of(meta, "discord").discord_origin is None:
      return []
    platform = discord_listener.DISCORD
    text = thread_entry.thread_reply_contract(platform, discord_listener.thread_contract_part(), cfg)
    return [
        task_prompts.RuleSegment(
            text=text,
            sources=tuple(
                task_prompts.PromptSource(scope=task_prompts.SCOPE_BASE, source_ref=f"prompts/{ref}")
                for ref in (platform.scope_doc, "thread_reply_redline.md", "thread_reply_format.md")),
        )
    ]

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {event_types.DISCORD_REPLY: _discord_reply_message}


CONTRIBUTION = DiscordTurnContribution()
