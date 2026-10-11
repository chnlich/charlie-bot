"""The boot backfill both chat channels run once the crash-recovery task has had its chance."""

import asyncio
from collections.abc import Awaitable, Callable

from src.infra import config, log_once
from src.runtime import session_events, session_lifecycle, session_listing, session_store, session_successor

log = log_once.LazyStructlogLogger()


async def run_backfill(
    cfg: config.CharlieBotConfig,
    listing: session_listing.SessionListing,
    store: session_store.SessionStore,
    lifecycle: session_lifecycle.SessionLifecycle,
    events: session_events.SessionEvents,
    successor: session_successor.SessionSuccessor,
    recovery_task: asyncio.Task,
    backfill_lost_summons: Callable[[
        config.CharlieBotConfig, session_listing.SessionListing, session_store.SessionStore, session_lifecycle.
        SessionLifecycle, session_events.SessionEvents, session_successor.SessionSuccessor
    ], Awaitable[int]],
    platform: str,
) -> None:
  """Report one platform's summons lost across the restart, once recovery has had its chance.

  Waits on the crash-recovery task first so re-attach and the user-message
  replay have already answered everything they can; whatever is still
  unanswered after that is genuinely lost and gets a notice in its thread.
  """
  await recovery_task
  reported = await backfill_lost_summons(cfg, listing, store, lifecycle, events, successor)
  log.info(f"{platform}_backfill_done", count=reported)
