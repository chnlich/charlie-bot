"""Scheduler — runs cron-like tasks that produce results in dedicated sessions."""

import asyncio
import traceback
from datetime import datetime, timedelta, timezone
from typing import Optional
from zoneinfo import ZoneInfo

import structlog
from croniter import croniter

from src.core.backup import BACKUP_DIR, apply_retention, create_backup
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, get_scheduled_tasks, load_config
from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager
from src.core.spawner import broadcast_and_persist, spawn_worker
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager

log = structlog.get_logger()

_TICK_INTERVAL = 60  # seconds between scheduler ticks


async def _backup_handler() -> None:
  """Built-in handler: create a backup and apply retention policy."""
  loop = asyncio.get_running_loop()
  archive = await loop.run_in_executor(None, create_backup)
  await loop.run_in_executor(None, apply_retention, BACKUP_DIR)
  log.info('backup_handler_done', archive=str(archive))


TASK_HANDLERS: dict[str, callable] = {
    'backup': _backup_handler,
}


class Scheduler:
  """Runs enabled ScheduledTaskConfigs on their cron schedules."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    self._task: Optional[asyncio.Task] = None

  async def start(self) -> None:
    self._task = asyncio.create_task(self._loop(), name="scheduler_loop")
    log.info("scheduler_started")

  async def stop(self) -> None:
    if self._task and not self._task.done():
      self._task.cancel()
      try:
        await self._task
      except asyncio.CancelledError:
        pass
    log.info("scheduler_stopped")

  async def run_task_now(self, task_name: str) -> dict:
    """Manually trigger a task by name. Returns session_id and thread_id."""
    cfg = self._reload_config()
    task_map = {t.name: t for t in get_scheduled_tasks()}
    task_cfg = task_map.get(task_name)
    if task_cfg is None:
      raise ValueError(f"No scheduled task named '{task_name}'")
    return await self._execute_task(task_cfg)

  # ---------------------------------------------------------------------------
  # Main loop
  # ---------------------------------------------------------------------------

  async def _loop(self) -> None:
    while True:
      try:
        await asyncio.sleep(_TICK_INTERVAL)
        await self._tick()
      except asyncio.CancelledError:
        raise
      except Exception as e:
        log.error("scheduler_tick_error", error=str(e), traceback=traceback.format_exc())

  async def _tick(self) -> None:
    cfg = self._reload_config()
    tasks = get_scheduled_tasks()
    if not tasks:
      return

    session_mgr = SessionManager(cfg)

    for task_cfg in tasks:
      if task_cfg.enabled:
        self._get_or_create_session(task_cfg.name, session_mgr)

    for task_cfg in tasks:
      if not task_cfg.enabled:
        continue
      try:
        await self._maybe_run(task_cfg, session_mgr)
      except Exception as e:
        log.error("scheduler_task_error", task=task_cfg.name, error=str(e), traceback=traceback.format_exc())

  async def _maybe_run(self, task_cfg: ScheduledTaskConfig, session_mgr: SessionManager) -> None:
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)

    last_run_at = await self._get_last_run(task_cfg.name, session_mgr)
    if last_run_at is None:
      # Never run: use a reference 60s before now so it fires immediately if due
      last_run_at = now - timedelta(seconds=_TICK_INTERVAL)

    next_fire = croniter(task_cfg.cron, last_run_at).get_next(datetime)
    if next_fire <= now:
      log.info("scheduler_firing", task=task_cfg.name, next_fire=next_fire.isoformat())
      await self._execute_task(task_cfg)

  # ---------------------------------------------------------------------------
  # Task execution
  # ---------------------------------------------------------------------------

  async def _execute_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Route to handler or prompt execution based on task config."""
    if task_cfg.handler:
      return await self._execute_handler_task(task_cfg)
    return await self._execute_prompt_task(task_cfg)

  async def _execute_handler_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Run a built-in handler inline; track last_scheduled_run via session."""
    handler = TASK_HANDLERS.get(task_cfg.handler)
    if handler is None:
      raise ValueError(f"Unknown handler: {task_cfg.handler!r}")
    cfg = self._reload_config()
    session_mgr = SessionManager(cfg)
    session = await self._get_or_create_session(task_cfg.name, session_mgr)
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)
    session.last_scheduled_run = now.isoformat()
    session.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session)
    log.info('handler_task_firing', task=task_cfg.name, handler=task_cfg.handler)
    asyncio.create_task(handler(), name=f'handler_{task_cfg.name}')
    return {'session_id': session.id, 'thread_id': None}

  async def _execute_prompt_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Find-or-create session, create thread, fire-and-forget worker."""
    cfg = self._reload_config()
    session_mgr = SessionManager(cfg)
    thread_mgr = ThreadManager(cfg)

    session = await self._get_or_create_session(task_cfg.name, session_mgr)

    # Update last_scheduled_run immediately before spawning
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)
    session.last_scheduled_run = now.isoformat()
    session.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session)

    thread = await thread_mgr.create_thread(session, task_cfg.prompt)

    asyncio.create_task(
        spawn_worker(
            session_id=session.id,
            description=task_cfg.prompt,
            thread_id=thread.id,
            cfg=cfg,
            session_mgr=session_mgr,
            thread_mgr=thread_mgr,
            repo_path=task_cfg.repo,
        ),
        name=f"scheduled_worker_{task_cfg.name}_{thread.id[:8]}",
    )

    event = {
        "type": "task_delegated",
        "task": task_cfg.name,
        "session_id": session.id,
        "thread_id": thread.id,
    }
    await streaming_manager.broadcast("sidebar", event)
    log.info("scheduled_task_fired", task=task_cfg.name, session=session.id, thread=thread.id)

    return {"session_id": session.id, "thread_id": thread.id}

  # ---------------------------------------------------------------------------
  # Session helpers
  # ---------------------------------------------------------------------------

  async def _get_or_create_session(self, task_name: str, session_mgr: SessionManager) -> SessionMetadata:
    """Return the existing dedicated session for task_name, or create one."""
    sessions = await session_mgr.list_sessions()
    for s in sessions:
      if s.scheduled_task == task_name:
        return s

    meta = await session_mgr.create_session(CreateSessionRequest(name=f"Scheduled: {task_name}"))
    meta.scheduled_task = task_name
    await session_mgr.save_metadata(meta)
    log.info("scheduled_session_created", task=task_name, session=meta.id)
    return meta

  async def _get_last_run(self, task_name: str, session_mgr: SessionManager) -> Optional[datetime]:
    """Return last_scheduled_run from the dedicated session, or None."""
    sessions = await session_mgr.list_sessions()
    for s in sessions:
      if s.scheduled_task == task_name and s.last_scheduled_run:
        try:
          return datetime.fromisoformat(s.last_scheduled_run)
        except ValueError as e:
          log.warning("scheduler_bad_last_run", task=task_name, value=s.last_scheduled_run, error=str(e))
    return None

  # ---------------------------------------------------------------------------
  # Config reload
  # ---------------------------------------------------------------------------

  def _reload_config(self) -> CharlieBotConfig:
    """Re-read config.yaml from disk so new tasks are picked up dynamically."""
    try:
      return load_config()
    except Exception as e:
      log.warning("scheduler_config_reload_failed", error=str(e))
      return self._cfg
