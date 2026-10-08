"""The boot backfill both chat channels run once the crash-recovery task has had its chance."""

import asyncio
from collections.abc import Awaitable, Callable

from src.infra import config, log_once
from src.runtime.session_events import SessionEvents
from src.runtime.session_lifecycle import SessionLifecycle
from src.runtime.session_listing import SessionListing
from src.runtime.session_store import SessionStore
from src.runtime.session_successor import SessionSuccessor

log = log_once.LazyStructlogLogger()


async def run_backfill(
    cfg: config.CharlieBotConfig,
    listing: SessionListing,
    store: SessionStore,
    lifecycle: SessionLifecycle,
    events: SessionEvents,
    successor: SessionSuccessor,
    recovery_task: asyncio.Task,
    backfill_lost_summons: Callable[
        [config.CharlieBotConfig, SessionListing, SessionStore, SessionLifecycle, SessionEvents, SessionSuccessor],
        Awaitable[int]],
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
