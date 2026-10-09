"""The Slack entrypoint service: the Socket Mode listener and the boot backfill of lost summons."""

import asyncio

from src.features.chat_threads import backfill
from src.features.slack import slack_listener
from src.infra import config, log_once, tasks
from src.runtime import session_events, session_lifecycle, session_listing, session_store, session_successor
from src.runtime.hooks import wiring

log = log_once.LazyStructlogLogger()

_listener_task: asyncio.Task | None = None
_backfill_task: asyncio.Task | None = None


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the listener and the backfill when the Slack credentials and an allowed user are configured."""
  global _listener_task, _backfill_task
  creds = config.get_credentials()
  if creds.get("slack", "bot_token") and creds.get("slack", "app_token") and ctx.cfg.slack.allowed_user_ids:
    listing, store, lifecycle = session_listing.listing(), session_store.store(), session_lifecycle.lifecycle()
    events, successor = session_events.events(), session_successor.successor()
    _listener_task = tasks.create_logged_task(
        slack_listener.run_listener(ctx.cfg, listing, store, lifecycle, events, successor), name="slack-listener")
    _backfill_task = tasks.create_logged_task(
        backfill.run_backfill(
            ctx.cfg, listing, store, lifecycle, events, successor, ctx.recovery_task,
            slack_listener.backfill_lost_summons, "slack"),
        name="slack-backfill")
    log.info("slack_entrypoint_started")
  else:
    log.info("slack_entrypoint_off")


async def stop_service() -> None:
  """Cancel the listener and the backfill; returns at once when start_service started neither."""
  global _listener_task, _backfill_task
  listener_task, _listener_task = _listener_task, None
  backfill_task, _backfill_task = _backfill_task, None
  await tasks.cancel_and_wait(listener_task)
  await tasks.cancel_and_wait(backfill_task)
