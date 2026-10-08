"""The Discord package's turn contribution: the round-end audit and the ``discord_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable

from src.features.discord import discord_listener
from src.features.discord.event_types import DISCORD_REPLY
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.runtime import session_events, session_lifecycle, session_store, session_successor
from src.runtime.hooks import turn_contributions


def _discord_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Discord: {event.get('content', '')}",
  }


class DiscordTurnContribution(turn_contributions.TurnContribution):
  """Runs the Discord round-end audit after every MASTER_DONE and renders ``discord_reply`` events."""

  async def after_turn(self, meta: SessionMetadata, done_event: dict, *, cfg: CharlieBotConfig) -> None:
    await discord_listener.deliver_done(
        meta.id, done_event, cfg, session_store.store(), session_lifecycle.lifecycle(), session_events.events(),
        session_successor.successor())

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {DISCORD_REPLY: _discord_reply_message}


CONTRIBUTION = DiscordTurnContribution()
