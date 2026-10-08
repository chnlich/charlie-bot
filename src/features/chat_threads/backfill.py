"""The boot backfill both chat channels run once the crash-recovery task has had its chance."""

import asyncio
from collections.abc import Awaitable, Callable

from src.infra import config, log_once
from src.runtime import sessions

log = log_once.LazyStructlogLogger()


async def run_backfill(
    cfg: config.CharlieBotConfig,
    session_mgr: sessions.SessionManager,
    recovery_task: asyncio.Task,
    backfill_lost_summons: Callable[[config.CharlieBotConfig, sessions.SessionManager], Awaitable[int]],
    platform: str,
) -> None:
  """Report one platform's summons lost across the restart, once recovery has had its chance.

  Waits on the crash-recovery task first so re-attach and the user-message
  replay have already answered everything they can; whatever is still
  unanswered after that is genuinely lost and gets a notice in its thread.
  """
  await recovery_task
  reported = await backfill_lost_summons(cfg, session_mgr)
  log.info(f"{platform}_backfill_done", count=reported)
