"""Scheduler — runs cron-like tasks that produce results in dedicated sessions."""

import asyncio
import traceback
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional
from zoneinfo import ZoneInfo

import structlog
from croniter import croniter

from src.core import event_types as ET
from src.core.backlog_loop import determine_action
from src.core.backup import apply_retention, create_backup
from src.core.config import (
  CharlieBotConfig,
  ScheduledTaskConfig,
  get_scheduled_tasks,
  load_config,
)
from src.core.master_trigger import trigger_master
from src.core.models import (
  PROJECT_ROLE,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  parse_utc_datetime,
)
from src.core.sessions import SessionManager
from src.core.spawner import resolve_requested_subagent_backend_model, spawn_worker
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager

log = structlog.get_logger()

_TICK_INTERVAL = 60  # seconds between scheduler ticks


async def _backup_handler() -> str:
  """Built-in handler: create a backup and apply retention policy."""
  loop = asyncio.get_running_loop()
  archive = await loop.run_in_executor(None, create_backup)
  await loop.run_in_executor(None, apply_retention)
  log.info('backup_handler_done', archive=str(archive))
  return str(archive)


TASK_HANDLERS: dict[str, callable] = {
    'backup': _backup_handler,
}


def effective_scheduled_task_backend(task_cfg: ScheduledTaskConfig, cfg: CharlieBotConfig) -> str:
  """Return the backend id a scheduled task should use."""
  if task_cfg.backend:
    if cfg.get_backend_option(task_cfg.backend) is None:
      raise ValueError(f"scheduled task backend '{task_cfg.backend}' is not in backend_options")
    return task_cfg.backend
  if not cfg.backend_options:
    raise ValueError("scheduled task backend resolution requires a configured backend_options entry")
  return cfg.backend_options[0].id


class Scheduler:
  """Runs enabled ScheduledTaskConfigs on their cron schedules."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager):
    """Take the process-wide SessionManager; a private instance would keep its own
    chat-event cache, so scheduled rounds would never reach the HTTP/WS read paths."""
    self._cfg = cfg
    self._session_mgr = session_mgr
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
    self._reload_config()
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

    session_mgr = self._session_mgr

    # Cache all sessions once to avoid O(N) list_sessions() calls per tick.
    all_sessions = await session_mgr.list_sessions(include_running_status=False)
    session_cache: dict[str, list[SessionMetadata]] = {}
    for s in all_sessions:
      if s.scheduled_task:
        session_cache.setdefault(s.scheduled_task, []).append(s)

    for task_cfg in tasks:
      if task_cfg.enabled:
        await self._get_or_create_session(task_cfg, cfg, session_mgr, session_cache)

    for task_cfg in tasks:
      if not task_cfg.enabled:
        continue
      try:
        await self._maybe_run(task_cfg, session_mgr, session_cache, cfg)
      except Exception as e:
        log.error("scheduler_task_error", task=task_cfg.name, error=str(e), traceback=traceback.format_exc())

  async def _maybe_run(
      self,
      task_cfg: ScheduledTaskConfig,
      session_mgr: SessionManager,
      session_cache: dict[str, list[SessionMetadata]],
      cfg: Optional[CharlieBotConfig] = None,
  ) -> None:
    cfg = cfg or self._cfg
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)

    session = await self._get_or_create_session(task_cfg, cfg, session_mgr, session_cache)
    if session is None:
      return

    # Detect cron expression change — reset last_scheduled_run to now and skip tick
    if session.last_scheduled_cron is not None and session.last_scheduled_cron != task_cfg.cron:
      log.info("scheduler_cron_changed", task=task_cfg.name, old=session.last_scheduled_cron, new=task_cfg.cron)
      session.last_scheduled_run = now.isoformat()
      session.last_scheduled_cron = task_cfg.cron
      session.updated_at = datetime.now(timezone.utc)
      await session_mgr.save_metadata(session)
      return

    if session.last_scheduled_run:
      try:
        last_run_at = parse_utc_datetime(session.last_scheduled_run)
      except ValueError as e:
        log.warning("scheduler_bad_last_run", task=task_cfg.name, value=session.last_scheduled_run, error=str(e))
        last_run_at = now - timedelta(seconds=_TICK_INTERVAL)
    else:
      # Never run: use a reference 60s before now so it fires immediately if due
      last_run_at = now - timedelta(seconds=_TICK_INTERVAL)

    next_fire = croniter(task_cfg.cron, last_run_at).get_next(datetime)
    if next_fire <= now:
      log.info("scheduler_firing", task=task_cfg.name, next_fire=next_fire.isoformat())
      await self._execute_task(task_cfg)

  # ---------------------------------------------------------------------------
  # Task execution
  # ---------------------------------------------------------------------------

  async def _prepare_task_execution(
      self,
      task_cfg: ScheduledTaskConfig,
      initial_status: Optional[str] = None,
  ) -> tuple[CharlieBotConfig, SessionManager, SessionMetadata]:
    """Shared preamble: reload config, get/create session, persist bookkeeping fields."""
    cfg = self._reload_config()
    session_mgr = self._session_mgr
    session = await self._get_or_create_session(task_cfg, cfg, session_mgr)
    if session is None:
      raise RuntimeError(f"scheduled task '{task_cfg.name}' session is busy during backend rotation")
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)
    session.last_scheduled_run = now.isoformat()
    session.last_scheduled_cron = task_cfg.cron
    if initial_status:
      session.last_run_status = initial_status
    session.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session)
    return cfg, session_mgr, session

  async def _execute_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Route to handler, loop, prompt, or master execution based on task config."""
    if task_cfg.mode == 'master':
      return await self._execute_master_task(task_cfg)
    if task_cfg.handler:
      return await self._execute_handler_task(task_cfg)
    if task_cfg.loop:
      return await self._execute_loop_task(task_cfg)
    return await self._execute_prompt_task(task_cfg)

  async def _execute_master_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Wake the dedicated session's master with the PM inline prompt.

    No worker thread and no TASK_DELEGATED event: the fire is a single master
    turn in the task's dedicated session, delivered through the shared
    trigger_master primitive (fire-and-forget so the scheduler loop never
    stalls behind a master turn).
    """
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg)
    inline_prompt = (
        "Read prompts/project_manager.md in the charlie-bot repo and run your "
        f"Project Manager duties for group {task_cfg.project}.")
    create_logged_task(
        trigger_master(session.id, inline_prompt, cfg, session_mgr),
        name=f"scheduled_master_{task_cfg.name}",
    )
    session.last_run_status = "success"
    session.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session)
    log.info("master_task_fired", task=task_cfg.name, session=session.id)
    return {"session_id": session.id, "thread_id": None}

  async def _execute_handler_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Run a built-in handler inline; track last_scheduled_run via session."""
    handler = TASK_HANDLERS.get(task_cfg.handler)
    if handler is None:
      raise ValueError(f"Unknown handler: {task_cfg.handler!r}")
    _, session_mgr, session = await self._prepare_task_execution(task_cfg, initial_status="running")
    log.info('handler_task_firing', task=task_cfg.name, handler=task_cfg.handler)
    try:
      result = await handler()
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'ok',
          'message': str(result) if result is not None else 'done',
      }
      session.last_run_status = "success"
    except Exception as e:
      log.warning('handler_task_error', task=task_cfg.name, error=str(e), traceback=traceback.format_exc())
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'error',
          'message': str(e),
      }
      session.last_run_status = "failed"
    session.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session)
    await session_mgr.persist_and_broadcast(session.id, event)
    return {'session_id': session.id, 'thread_id': None}

  async def _execute_prompt_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Find-or-create session, create thread, fire-and-forget worker."""
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg, initial_status="running")
    return await self._spawn_scheduled_worker(
        session,
        task_cfg,
        task_cfg.prompt,
        task_cfg.prompt,
        "scheduled_task_fired",
        cfg,
        session_mgr,
        require_review=False)

  async def _execute_loop_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Run an improvement-loop task: determine action, then spawn worker if needed."""
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg)

    repo_path = Path(task_cfg.repo) if task_cfg.repo else None
    if repo_path is None:
      raise ValueError(f"loop task '{task_cfg.name}' requires 'repo'")

    backlog_path = repo_path / task_cfg.loop.backlog
    action_type, prompt = await determine_action(backlog_path, task_cfg.loop, repo_path)

    if action_type in ('noop', 'stale_reset'):
      session.last_run_status = "success"
      await session_mgr.save_metadata(session)
      log.info("loop_task_noop", task=task_cfg.name, action=action_type)
      return {"session_id": session.id, "thread_id": None}

    session.last_run_status = "running"
    await session_mgr.save_metadata(session)
    return await self._spawn_scheduled_worker(
        session,
        task_cfg,
        prompt,
        f"[{action_type}] {prompt[:200]}",
        "loop_task_fired",
        cfg,
        session_mgr,
        require_review=(action_type == 'implement'),
        action=action_type)

  async def _spawn_scheduled_worker(
      self,
      session: SessionMetadata,
      task_cfg: ScheduledTaskConfig,
      description: str,
      event_description: str,
      log_event: str,
      cfg: CharlieBotConfig,
      session_mgr: SessionManager,
      require_review: bool = True,
      **log_extra: str,
  ) -> dict:
    """Create thread, spawn worker, broadcast task_delegated, and return result dict."""
    thread_mgr = ThreadManager(cfg)
    thread = await thread_mgr.create_thread(session, description, require_review=require_review)
    effective_backend = effective_scheduled_task_backend(task_cfg, cfg)

    resolved_backend, resolved_model = await resolve_requested_subagent_backend_model(
        session.id, cfg, session_mgr, requested_backend=effective_backend)

    create_logged_task(
        spawn_worker(
            session_id=session.id,
            description=description,
            thread_id=thread.id,
            cfg=cfg,
            session_mgr=session_mgr,
            thread_mgr=thread_mgr,
            request=SpawnRequest(
                repo_path=task_cfg.repo,
                resolved_backend=resolved_backend,
                resolved_model=resolved_model,
                task_type=TaskType.IMPLEMENT,
            ),
        ),
        name=f"scheduled_worker_{task_cfg.name}_{thread.id[:8]}",
    )

    event = {
        "type": ET.TASK_DELEGATED,
        "task": task_cfg.name,
        "description": event_description,
        "session_id": session.id,
        "thread_id": thread.id,
        "backend": resolved_backend or "",
        "model": resolved_model or "",
    }
    await session_mgr.persist_and_broadcast(session.id, event)
    log.info(log_event, task=task_cfg.name, session=session.id, thread=thread.id, **log_extra)

    return {"session_id": session.id, "thread_id": thread.id}

  # ---------------------------------------------------------------------------
  # Session helpers
  # ---------------------------------------------------------------------------

  async def _get_or_create_session(
      self,
      task_cfg: ScheduledTaskConfig,
      cfg: CharlieBotConfig,
      session_mgr: SessionManager,
      session_cache: Optional[dict[str, list[SessionMetadata]]] = None,
  ) -> Optional[SessionMetadata]:
    """Return the active dedicated session for task/backend, rotating if needed.

    When session_cache is provided, uses it instead of scanning the sessions
    directory. Newly created sessions are added to the cache. A mode: master
    task binds its dedicated session to the task's project: role=project and
    group=<project value> ride onto both creation paths downstream.
    """
    effective_backend = effective_scheduled_task_backend(task_cfg, cfg)
    role: Optional[str] = None
    group: Optional[str] = None
    if task_cfg.mode == 'master':
      role = PROJECT_ROLE
      group = task_cfg.project
    return await session_mgr.ensure_scheduled_session_backend(
        task_cfg.name,
        effective_backend,
        session_cache=session_cache,
        skip_if_busy=True,
        role=role,
        group=group,
    )

  # ---------------------------------------------------------------------------
  # Config reload
  # ---------------------------------------------------------------------------

  def _reload_config(self) -> CharlieBotConfig:
    """Re-read config.yaml from disk so new tasks are picked up dynamically."""
    try:
      self._cfg = load_config()
      return self._cfg
    except Exception as e:
      log.warning("scheduler_config_reload_failed", error=str(e))
      return self._cfg
