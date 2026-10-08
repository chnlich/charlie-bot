"""The Discord package's turn contribution: the round-end audit and the ``discord_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.runtime.hooks import turn_contributions

if TYPE_CHECKING:
  from src.runtime.sessions import SessionManager


def _discord_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Discord: {event.get('content', '')}",
  }


class DiscordTurnContribution(turn_contributions.TurnContribution):
  """Runs the Discord round-end audit after every MASTER_DONE and renders ``discord_reply`` events."""

  async def after_turn(
      self, meta: SessionMetadata, done_event: dict, *, cfg: CharlieBotConfig, sessions: SessionManager) -> None:
    # lazy: discord_listener imports SessionManager from src.runtime.sessions at top level
    from src.features.discord import discord_listener

    await discord_listener.deliver_done(meta.id, done_event, cfg, sessions)

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {ET.DISCORD_REPLY: _discord_reply_message}


CONTRIBUTION = DiscordTurnContribution()
