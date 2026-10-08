"""The scheduler service: runs the cron scheduler for the server's lifetime."""

from __future__ import annotations

from typing import TYPE_CHECKING

from src.runtime.hooks import wiring

if TYPE_CHECKING:
  from src.features.cron import scheduler as scheduler_module

_scheduler: scheduler_module.Scheduler | None = None


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Build the scheduler, publish it on app.state for the cron API's run-now route, and start it."""
  global _scheduler
  from src.features.cron import scheduler as scheduler_module

  _scheduler = scheduler_module.Scheduler(ctx.cfg, ctx.session_mgr)
  ctx.app.state.scheduler = _scheduler
  await _scheduler.start()


async def stop_service() -> None:
  """Stop the scheduler; returns at once when start_service never ran."""
  global _scheduler
  running, _scheduler = _scheduler, None
  if running is not None:
    await running.stop()
