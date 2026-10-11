"""The chat-channel entrypoint service: one gateway listener task plus the boot backfill task.

Every chat channel boots the same shape: a long-lived gateway listener and one
backfill pass over the summons lost while the server was down. The channel's
own module keeps its wiring-facing ``start_service``/``stop_service`` pair and
its configuration gate; the shared task bookkeeping lives here.
"""

import asyncio
from collections.abc import Awaitable, Callable
from typing import Protocol

from src.features.chat_threads import backfill
from src.infra import config, log_once, tasks
from src.runtime import session_events, session_lifecycle, session_listing, session_store, session_successor
from src.runtime.hooks import wiring

log = log_once.LazyStructlogLogger()


class ChannelListener(Protocol):
  """The listener surface a chat channel hands the entrypoint service: the gateway loop and its boot backfill."""

  run_listener: Callable[[
      config.CharlieBotConfig, session_listing.SessionListing, session_store.SessionStore, session_lifecycle.
      SessionLifecycle, session_events.SessionEvents, session_successor.SessionSuccessor
  ], Awaitable[None]]
  backfill_lost_summons: Callable[[
      config.CharlieBotConfig, session_listing.SessionListing, session_store.SessionStore, session_lifecycle.
      SessionLifecycle, session_events.SessionEvents, session_successor.SessionSuccessor
  ], Awaitable[int]]


class ChannelService:
  """One chat channel's entrypoint tasks: the gateway listener and the boot backfill.

  A caller relies on: start launches nothing and logs ``<channel>_entrypoint_off`` unless
  *configured* holds; a started service runs its tasks under the ``<channel>-listener`` and
  ``<channel>-backfill`` names and logs ``<channel>_entrypoint_started``; stop cancels what
  start launched and returns at once on a service that never started.
  """

  def __init__(self, channel: str, listener: ChannelListener) -> None:
    self._channel = channel
    self._listener = listener
    self._listener_task: asyncio.Task | None = None
    self._backfill_task: asyncio.Task | None = None

  async def start(self, ctx: wiring.ServiceContext, *, configured: bool) -> None:
    """Launch the listener and the backfill when the channel is configured; log the off event otherwise."""
    if not configured:
      log.info(f"{self._channel}_entrypoint_off")
      return
    listing, store, lifecycle = session_listing.listing(), session_store.store(), session_lifecycle.lifecycle()
    events, successor = session_events.events(), session_successor.successor()
    self._listener_task = tasks.create_logged_task(
        self._listener.run_listener(ctx.cfg, listing, store, lifecycle, events, successor),
        name=f"{self._channel}-listener")
    self._backfill_task = tasks.create_logged_task(
        backfill.run_backfill(
            ctx.cfg, listing, store, lifecycle, events, successor, ctx.recovery_task,
            self._listener.backfill_lost_summons, self._channel),
        name=f"{self._channel}-backfill")
    log.info(f"{self._channel}_entrypoint_started")

  async def stop(self) -> None:
    """Cancel the listener and the backfill; returns at once when start launched neither."""
    listener_task, self._listener_task = self._listener_task, None
    backfill_task, self._backfill_task = self._backfill_task, None
    await tasks.cancel_and_wait(listener_task)
    await tasks.cancel_and_wait(backfill_task)
