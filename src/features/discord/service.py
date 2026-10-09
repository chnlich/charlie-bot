"""The Discord entrypoint service: the gateway listener and the boot backfill of lost summons."""

import asyncio

from src.features.chat_threads import backfill
from src.features.discord import discord_listener
from src.infra import config, log_once, tasks
from src.runtime import session_events, session_lifecycle, session_listing, session_store, session_successor
from src.runtime.hooks import wiring

log = log_once.LazyStructlogLogger()

_listener_task: asyncio.Task | None = None
_backfill_task: asyncio.Task | None = None


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the listener and the backfill when the Discord bot token and an allowed user are configured."""
  global _listener_task, _backfill_task
  creds = config.get_credentials()
  if creds.get("discord", "bot_token") and ctx.cfg.discord.allowed_users:
    listing, store, lifecycle = session_listing.listing(), session_store.store(), session_lifecycle.lifecycle()
    events, successor = session_events.events(), session_successor.successor()
    _listener_task = tasks.create_logged_task(
        discord_listener.run_listener(ctx.cfg, listing, store, lifecycle, events, successor), name="discord-listener")
    _backfill_task = tasks.create_logged_task(
        backfill.run_backfill(
            ctx.cfg, listing, store, lifecycle, events, successor, ctx.recovery_task,
            discord_listener.backfill_lost_summons, "discord"),
        name="discord-backfill")
    log.info("discord_entrypoint_started")
  else:
    log.info("discord_entrypoint_off")


async def stop_service() -> None:
  """Cancel the listener and the backfill; returns at once when start_service started neither."""
  global _listener_task, _backfill_task
  listener_task, _listener_task = _listener_task, None
  backfill_task, _backfill_task = _backfill_task, None
  await tasks.cancel_and_wait(listener_task)
  await tasks.cancel_and_wait(backfill_task)
