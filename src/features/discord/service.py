"""The Discord entrypoint service: the gateway listener and the boot backfill of lost summons."""

from src.features.chat_threads import channel_service
from src.features.discord import discord_listener
from src.infra import config
from src.runtime.hooks import wiring

_service = channel_service.ChannelService("discord", discord_listener)


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the listener and the backfill when the Discord bot token and an allowed user are configured."""
  creds = config.get_credentials()
  await _service.start(ctx, configured=bool(creds.get("discord", "bot_token") and ctx.cfg.discord.allowed_users))


async def stop_service() -> None:
  """Cancel the listener and the backfill; returns at once when start_service started neither."""
  await _service.stop()
