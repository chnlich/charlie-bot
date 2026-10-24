"""The Discord package's turn contribution: the round-end audit, the thread reply contract, and the ``discord_reply`` chat line."""

from __future__ import annotations

from src.features.chat_threads import turn_contribution
from src.features.discord import discord_listener


class DiscordTurnContribution(turn_contribution.ThreadTurnContribution):
  """Runs the Discord round-end audit after every MASTER_DONE, carries the thread reply contract, and renders ``discord_reply`` events."""

  PLATFORM = discord_listener.DISCORD
  LISTENER = discord_listener

  def contract_part(self, _origin: object) -> str:
    """The Discord part names no thread address, so the origin plays no part in it."""
    return discord_listener.thread_contract_part()


CONTRIBUTION = DiscordTurnContribution()
