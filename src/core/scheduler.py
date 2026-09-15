"""Scheduler — runs cron-like tasks that produce results in dedicated sessions."""

import asyncio
import contextlib
import traceback
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo

from src.core import event_types as ET
from src.core import task_chain
from src.core.backlog_loop import determine_action
from src.core.backup import apply_retention, create_backup
from src.core.config import (
    CharlieBotConfig,
    ScheduledTaskConfig,
    get_config,
    get_scheduled_tasks,
    require_backend_option,
)
from src.core.log_once import LazyStructlogLogger
from src.core.master_trigger import trigger_master
from src.core.models import (
    PROJECT_ROLE,
    LastRunStatus,
    SessionMetadata,
    SpawnRequest,
    TaskType,
    ThreadMetadata,
    parse_utc_datetime,
)
from src.core.sessions import SessionManager
from src.core.spawner import resolve_requested_subagent_backend_model, spawn_worker
from src.core.storage_cool import format_sweep_line, run_cool_sweep
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager

log = LazyStructlogLogger()

_TICK_INTERVAL = 60  # seconds between scheduler ticks


def load_croniter(namespace: dict[str, Any]) -> Any:
  """Bind croniter into *namespace* on first use and return it.

  croniter (+ its dateutil subtree, ~21 ms together) is the server import
  floor's largest deferrable third-party slice and no import path resolves a
  next fire, so the import rides the first due-task or next-run resolution.
  The binding is per consumer module: each caller passes its own ``globals()``
  so its bare-name reads keep working and stay the tests' monkeypatch target
  (tests/test_cron_next_run_memo.py patches ``src.api.cron.croniter``).
  """
  from croniter import croniter

  namespace["croniter"] = croniter
  return croniter


async def _backup_handler() -> str:
  """Built-in handler: create a backup and apply retention policy."""
  loop = asyncio.get_running_loop()
  archive = await loop.run_in_executor(None, create_backup)
  await loop.run_in_executor(None, apply_retention)
  log.info('backup_handler_done', archive=str(archive))
  return str(archive)


async def _cool_storage_handler() -> str:
  """Built-in handler: reclaim cold sessions' readerless bytes (real run, no dry run)."""
  loop = asyncio.get_running_loop()
  result = await loop.run_in_executor(None, run_cool_sweep)
  summary = format_sweep_line(result)
  log.info('cool_storage_handler_done', total_bytes=result.total_bytes)
  return summary


TASK_HANDLERS: dict[str, callable] = {
    'backup': _backup_handler,
    'cool_storage': _cool_storage_handler,
}


def effective_scheduled_task_backend(task_cfg: ScheduledTaskConfig, cfg: CharlieBotConfig) -> str:
  """Return the backend id a scheduled task should use."""
  if task_cfg.backend:
    require_backend_option(cfg, task_cfg.backend, subject="scheduled task ")
    return task_cfg.backend
  if not cfg.backends.options:
    raise ValueError("scheduled task backend resolution requires a configured backends.options entry")
  return cfg.backends.options[0].id


def scheduled_task_session_binding(task_cfg: ScheduledTaskConfig,
                                   cfg: CharlieBotConfig) -> tuple[str, str | None, str | None]:
  """Return the effective backend plus the role/group a task's dedicated session binds to.

  A mode: master task binds its dedicated session to the task's project —
  role=project and group=<project value>; every other mode binds neither.
  """
  backend = effective_scheduled_task_backend(task_cfg, cfg)
  role = PROJECT_ROLE if task_cfg.mode == 'master' else None
  group = task_cfg.project if task_cfg.mode == 'master' else None
  return backend, role, group


async def fire_scheduled_worker(
    session: SessionMetadata,
    task_cfg: ScheduledTaskConfig,
    thread: ThreadMetadata,
    event_description: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    *,
    backend_override: str | None,
    prompt_override: str | None,
) -> asyncio.Task:
  """Fire one scheduled task's worker on an already-created thread and broadcast its
  TASK_DELEGATED event; return the worker task's handle.

  The single spawn block every scheduled worker goes through — the cron tick
  (``_spawn_scheduled_worker``) and the steps-chain advance
  (``task_chain.spawn_step``) — so the spawn request, the ``scheduled_worker_*``
  task name, and the TASK_DELEGATED keys cannot drift between paths; the sidebar
  workers panel renders that one event shape.
  """
  effective_backend = backend_override or effective_scheduled_task_backend(task_cfg, cfg)
  resolved_backend, resolved_model = await resolve_requested_subagent_backend_model(
      session.id, cfg, session_mgr, requested_backend=effective_backend)
  handle = create_logged_task(
      spawn_worker(
          session_id=session.id,
          description=thread.description,
          thread_id=thread.id,
          cfg=cfg,
          session_mgr=session_mgr,
          thread_mgr=thread_mgr,
          request=SpawnRequest(
              repo_path=task_cfg.repo,
              prompt_override=prompt_override,
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
  return handle


class Scheduler:
  """Runs enabled ScheduledTaskConfigs on their cron schedules."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    """Take the process-wide SessionManager; a private instance would keep its own
    chat-event cache, so scheduled rounds would never reach the HTTP/WS read paths."""
    self._cfg = cfg
    self._session_mgr = session_mgr
    self._task: asyncio.Task | None = None
    # Process-local registry of the background task each task's most recent
    # *scheduled* fire spawned (keyed by task name). Empty after a restart, so
    # the first fire after a restart is judged idle by design. Manual /run
    # rounds never register here, so they neither block nor are blocked by a
    # scheduled round.
    self._handles: dict[str, asyncio.Task] = {}

  async def start(self) -> None:
    self._task = asyncio.create_task(self._loop(), name="scheduler_loop")
    log.info("scheduler_started")

  async def stop(self) -> None:
    if self._task and not self._task.done():
      self._task.cancel()
      with contextlib.suppress(asyncio.CancelledError):
        await self._task
    log.info("scheduler_stopped")

  async def run_task_now(self, task_name: str) -> dict:
    """Manually trigger a task by name. Returns session_id and thread_id."""
    self._reload_config()
    task_map = {t.name: t for t in get_scheduled_tasks()}
    task_cfg = task_map.get(task_name)
    if task_cfg is None:
      raise ValueError(f"No scheduled task named '{task_name}'")
    # A manual run is an intentional new firing: its identity (the fire time)
    # is distinct from every cron occurrence's due-time identity.
    return await self._execute_task(task_cfg, firing=datetime.now(UTC).isoformat())

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

    # Cache the scheduled sessions once to avoid O(tasks) list_sessions() calls per tick.
    scheduled_sessions = await session_mgr.list_sessions(scheduled=True, include_running_status=False)
    session_cache: dict[str, list[SessionMetadata]] = {}
    for s in scheduled_sessions:
      assert s.scheduled_task is not None
      session_cache.setdefault(s.scheduled_task, []).append(s)

    for task_cfg in tasks:
      if task_cfg.enabled and not task_cfg.session_id:
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
      cfg: CharlieBotConfig | None = None,
  ) -> None:
    cfg = cfg or self._cfg
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)

    if task_cfg.session_id:
      # A bound task fires against its stable task-tree node: the binding is
      # validated here (a missing/legacy node fails the tick visibly), never
      # discovered or replaced.
      from src.api.deps import task_manager
      from src.core.cron_sequence import check_fireable_binding
      session = await check_fireable_binding(task_cfg, task_manager())
    else:
      session = await self._get_or_create_session(task_cfg, cfg, session_mgr, session_cache)
    if session is None:
      return

    # Detect cron expression change — reset last_scheduled_run to now and skip tick
    if session.last_scheduled_cron is not None and session.last_scheduled_cron != task_cfg.cron:
      log.info("scheduler_cron_changed", task=task_cfg.name, old=session.last_scheduled_cron, new=task_cfg.cron)
      session.last_scheduled_run = now.isoformat()
      session.last_scheduled_cron = task_cfg.cron
      session.updated_at = datetime.now(UTC)
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

    if "croniter" not in globals():
      load_croniter(globals())
    next_fire = croniter(task_cfg.cron, last_run_at).get_next(datetime)  # noqa: F821  # bound by load_croniter
    if next_fire <= now:
      handle = self._handles.get(task_cfg.name)
      if handle is not None and not handle.done():
        session.last_scheduled_run = now.isoformat()
        session.last_run_status = LastRunStatus.SKIPPED
        session.updated_at = datetime.now(UTC)
        await session_mgr.save_metadata(session)
        event = {
            'type': ET.SCHEDULED_RUN_SKIPPED,
            'task': task_cfg.name,
            'skipped_at': now.isoformat(),
            'reason': f"previous round still running ({handle.get_name()})",
        }
        await session_mgr.persist_and_broadcast(session.id, event)
        log.info(
            "scheduler_run_skipped",
            task=task_cfg.name,
            skipped_at=now.isoformat(),
            handle=handle.get_name(),
        )
        return
      log.info("scheduler_firing", task=task_cfg.name, next_fire=next_fire.isoformat())
      await self._execute_task(task_cfg, record_handle=True, firing=next_fire.isoformat())

  # ---------------------------------------------------------------------------
  # Task execution
  # ---------------------------------------------------------------------------

  async def _prepare_task_execution(
      self,
      task_cfg: ScheduledTaskConfig,
      initial_status: LastRunStatus | None = None,
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
    session.updated_at = datetime.now(UTC)
    await session_mgr.save_metadata(session)
    return cfg, session_mgr, session

  async def _execute_task(
      self, task_cfg: ScheduledTaskConfig, record_handle: bool = False,
      firing: str | None = None,
  ) -> dict:
    """Route to handler, loop, prompt, or master execution based on task config.

    ``record_handle`` gates whether the background round spawned by this fire is
    registered in the overlap-skip registry. The scheduled path records it via
    ``_maybe_run``; manual ``run_task_now`` leaves it off so manual rounds stay
    outside the skip judgment. A task bound to a task-tree node (``session_id``)
    takes the v2 path — the binding is the session, the firing identity is
    durable, and executions land on Runs; ``firing`` carries the due
    occurrence's time (an explicit manual fire passes its own).
    """
    if task_cfg.session_id:
      return await self._execute_bound_task(
          task_cfg, record_handle=record_handle, firing=firing or datetime.now(UTC).isoformat())
    if task_cfg.mode == 'master':
      return await self._execute_master_task(task_cfg, record_handle=record_handle)
    if task_cfg.handler:
      return await self._execute_handler_task(task_cfg)
    if task_cfg.loop:
      return await self._execute_loop_task(task_cfg, record_handle=record_handle)
    if task_cfg.steps:
      return await self._execute_steps_task(task_cfg, record_handle=record_handle)
    return await self._execute_prompt_task(task_cfg, record_handle=record_handle)

  # ---------------------------------------------------------------------------
  # Bound (task-tree) execution — the v2 path
  # ---------------------------------------------------------------------------

  async def _execute_bound_task(
      self, task_cfg: ScheduledTaskConfig, *, record_handle: bool, firing: str,
  ) -> dict:
    """Fire one bound task against its stable node (src.core.cron_sequence owns the shapes)."""
    from src.api.deps import task_manager
    from src.core.cron_sequence import (
      check_fireable_binding,
      fire_bound_master,
      run_firing_steps,
    )

    cfg = self._reload_config()
    tree = task_manager()
    meta = await check_fireable_binding(task_cfg, tree)
    await self._record_bound_fire(meta, task_cfg, cfg)

    if task_cfg.mode == 'master':
      return await fire_bound_master(task_cfg, meta, tree, firing)

    if task_cfg.handler:
      # The system handler keeps its inline execution; only its bookkeeping
      # borrows the bound node.
      return await self._execute_handler_on_bound(task_cfg, meta, tree)

    backend, model = self._resolve_bound_backend_model(task_cfg, tree)

    if task_cfg.steps:
      leaf = await self._bound_leaf(task_cfg, meta, tree, firing,
                                    goal=f"{task_cfg.name} steps", backend=backend, model=model)
      handle = create_logged_task(
          run_firing_steps(task_cfg, meta, tree, firing, leaf.id),
          name=f"bound_steps_{task_cfg.name}_{firing}")
      if record_handle:
        self._handles[task_cfg.name] = handle
      return {"session_id": meta.id, "leaf_session_id": leaf.id, "firing": firing}

    prompt, event_description, action = await self._resolve_bound_prompt(task_cfg, meta, cfg)
    if prompt is None:
      return {"session_id": meta.id, "firing": firing, "skipped": action}
    leaf = await self._bound_leaf(
        task_cfg, meta, tree, firing, goal=prompt, backend=backend, model=model)
    handle = await self._launch_bound_round(
        task_cfg, meta, tree, firing, leaf.id, backend=backend, model=model,
        event_description=event_description, record_handle=record_handle)
    return {"session_id": meta.id, "leaf_session_id": leaf.id, "firing": firing}

  async def _record_bound_fire(
      self, meta: SessionMetadata, task_cfg: ScheduledTaskConfig, cfg: CharlieBotConfig,
  ) -> None:
    """The scheduler's per-fire bookkeeping on the bound node (same fields as legacy)."""
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)
    meta.last_scheduled_run = now.isoformat()
    meta.last_scheduled_cron = task_cfg.cron
    meta.updated_at = datetime.now(UTC)
    await self._session_mgr.save_metadata(meta)

  def _resolve_bound_backend_model(
      self, task_cfg: ScheduledTaskConfig, tree,
  ) -> tuple[str, str | None]:
    from src.core.cron_sequence import resolved_backend_model
    return resolved_backend_model(task_cfg, tree, task_cfg.backend)

  async def _resolve_bound_prompt(
      self, task_cfg: ScheduledTaskConfig, meta: SessionMetadata, cfg: CharlieBotConfig,
  ) -> tuple[str | None, str, str | None]:
    """The fire's prompt: the loop action's decision, or the task prompt verbatim.

    Returns (prompt, event_description, action); prompt None means the loop
    decided nothing to do (noop/stale_reset) and the fire is recorded as such.
    """
    if task_cfg.loop:
      from src.core.backlog_loop import determine_action
      repo_path = Path(task_cfg.repo) if task_cfg.repo else None
      if repo_path is None:
        raise ValueError(f"loop task '{task_cfg.name}' requires 'repo'")
      action_type, prompt = await determine_action(repo_path / task_cfg.loop.backlog, task_cfg.loop, repo_path)
      if action_type in ('noop', 'stale_reset'):
        meta.last_run_status = LastRunStatus.SUCCESS
        meta.updated_at = datetime.now(UTC)
        await self._session_mgr.save_metadata(meta)
        log.info("bound_loop_task_noop", task=task_cfg.name, action=action_type, session=meta.id)
        return None, "", action_type
      return prompt, f"[{action_type}] {prompt[:200]}", action_type
    return task_cfg.prompt or "", "scheduled_task_fired", None

  async def _bound_leaf(
      self, task_cfg: ScheduledTaskConfig, meta: SessionMetadata, tree,
      firing: str, *, goal: str, backend: str, model: str | None,
  ):
    from src.core.cron_sequence import ensure_firing_leaf
    return await ensure_firing_leaf(task_cfg, meta, tree, firing, goal, backend, model)

  async def _launch_bound_round(
      self, task_cfg: ScheduledTaskConfig, meta: SessionMetadata, tree,
      firing: str, leaf_id: str, *, backend: str, model: str | None,
      event_description: str, record_handle: bool,
  ) -> asyncio.Task:
    """One firing's work round: register, launch (scheduler-owned), await the
    terminal fact for the overlap registry. The ordinary work-Run delivery
    chain owns the report/close follow-ups."""
    from src.core.cron_sequence import launch, register_leaf_run, wait_for_terminal

    async def _round() -> None:
      run = await register_leaf_run(
          tree, leaf_id, task_cfg, firing, kind="work", position=None,
          backend=backend, model=model)
      launch(tree, leaf_id, run.id, prompt=None)
      await wait_for_terminal(tree, leaf_id, run.id)

    handle = create_logged_task(_round(), name=f"bound_worker_{task_cfg.name}_{firing}")
    if record_handle:
      self._handles[task_cfg.name] = handle
    log.info("bound_task_fired", task=task_cfg.name, session=meta.id, leaf=leaf_id, firing=firing)
    return handle

  async def _execute_handler_on_bound(
      self, task_cfg: ScheduledTaskConfig, meta: SessionMetadata, tree,
  ) -> dict:
    """The built-in handler modes on a bound node: inline execution, bound bookkeeping."""
    handler = TASK_HANDLERS.get(task_cfg.handler)
    if handler is None:
      raise ValueError(f"Unknown handler: {task_cfg.handler!r}")
    session = meta
    log.info('handler_task_firing', task=task_cfg.name, handler=task_cfg.handler)
    try:
      result = await handler()
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'ok',
          'message': str(result) if result is not None else 'done',
      }
      session.last_run_status = LastRunStatus.SUCCESS
    except Exception as e:
      log.warning('handler_task_error', task=task_cfg.name, error=str(e), traceback=traceback.format_exc())
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'error',
          'message': str(e),
      }
      session.last_run_status = LastRunStatus.FAILED
    session.updated_at = datetime.now(UTC)
    await self._session_mgr.save_metadata(session)
    await self._session_mgr.persist_and_broadcast(session.id, event)
    return {'session_id': session.id, 'thread_id': None}

  async def _execute_master_task(self, task_cfg: ScheduledTaskConfig, record_handle: bool = False) -> dict:
    """Wake the dedicated session's master with the task prompt plus its group.

    The wake message is the task's resolved prompt: a PM task's host cron file
    carries the path to prompts/project_manager.md under ``prompt_file``, the
    pointed file owns the body, and the loader reads it on every load; a
    `Group: <project>` line is appended, and the yaml is the single control
    point for the wake text. No worker thread and no
    TASK_DELEGATED event: the fire is a single master turn in the task's
    dedicated session, delivered through the shared trigger_master primitive
    (fire-and-forget so the scheduler loop never stalls behind a master turn).
    """
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg)
    wake_prompt = f"{task_cfg.prompt}\n\nGroup: {task_cfg.project}"
    # pull_back=False: a cron wake is a timed wake and must never unarchive the
    # session it lands on — the same opt-out the trigger fire passes. Selection
    # above only hands over active sessions, so this is the declared intent.
    handle = create_logged_task(
        trigger_master(session.id, wake_prompt, cfg, session_mgr, pull_back=False),
        name=f"scheduled_master_{task_cfg.name}",
    )
    if record_handle:
      self._handles[task_cfg.name] = handle
    session.last_run_status = LastRunStatus.SUCCESS
    session.updated_at = datetime.now(UTC)
    await session_mgr.save_metadata(session)
    log.info("master_task_fired", task=task_cfg.name, session=session.id)
    return {"session_id": session.id, "thread_id": None}

  async def _execute_handler_task(self, task_cfg: ScheduledTaskConfig) -> dict:
    """Run a built-in handler inline; track last_scheduled_run via session."""
    handler = TASK_HANDLERS.get(task_cfg.handler)
    if handler is None:
      raise ValueError(f"Unknown handler: {task_cfg.handler!r}")
    _, session_mgr, session = await self._prepare_task_execution(task_cfg, initial_status=LastRunStatus.RUNNING)
    log.info('handler_task_firing', task=task_cfg.name, handler=task_cfg.handler)
    try:
      result = await handler()
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'ok',
          'message': str(result) if result is not None else 'done',
      }
      session.last_run_status = LastRunStatus.SUCCESS
    except Exception as e:
      log.warning('handler_task_error', task=task_cfg.name, error=str(e), traceback=traceback.format_exc())
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': 'error',
          'message': str(e),
      }
      session.last_run_status = LastRunStatus.FAILED
    session.updated_at = datetime.now(UTC)
    await session_mgr.save_metadata(session)
    await session_mgr.persist_and_broadcast(session.id, event)
    return {'session_id': session.id, 'thread_id': None}

  async def _execute_prompt_task(self, task_cfg: ScheduledTaskConfig, record_handle: bool = False) -> dict:
    """Find-or-create session, create thread, fire-and-forget worker."""
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg, initial_status=LastRunStatus.RUNNING)
    return await self._spawn_scheduled_worker(
        session,
        task_cfg,
        task_cfg.prompt,
        task_cfg.prompt,
        "scheduled_task_fired",
        cfg,
        session_mgr,
        require_review=False,
        record_handle=record_handle)

  async def _execute_steps_task(self, task_cfg: ScheduledTaskConfig, record_handle: bool = False) -> dict:
    """Fire step 0 of a steps task; later steps advance from the finalize chain."""
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg, initial_status=LastRunStatus.RUNNING)
    thread_mgr = ThreadManager(cfg)
    result = await task_chain.spawn_step(session, task_cfg, 0, cfg, session_mgr, thread_mgr)
    if record_handle:
      self._handles[task_cfg.name] = result["handle"]
    return result

  async def _execute_loop_task(self, task_cfg: ScheduledTaskConfig, record_handle: bool = False) -> dict:
    """Run an improvement-loop task: determine action, then spawn worker if needed."""
    cfg, session_mgr, session = await self._prepare_task_execution(task_cfg)

    repo_path = Path(task_cfg.repo) if task_cfg.repo else None
    if repo_path is None:
      raise ValueError(f"loop task '{task_cfg.name}' requires 'repo'")

    backlog_path = repo_path / task_cfg.loop.backlog
    action_type, prompt = await determine_action(backlog_path, task_cfg.loop, repo_path)

    if action_type in ('noop', 'stale_reset'):
      session.last_run_status = LastRunStatus.SUCCESS
      await session_mgr.save_metadata(session)
      log.info("loop_task_noop", task=task_cfg.name, action=action_type)
      return {"session_id": session.id, "thread_id": None}

    session.last_run_status = LastRunStatus.RUNNING
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
        action=action_type,
        record_handle=record_handle)

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
      record_handle: bool = False,
      **log_extra: str,
  ) -> dict:
    """Create thread, fire its worker through the shared spawn block, and return the result dict."""
    thread_mgr = ThreadManager(cfg)
    thread = await thread_mgr.create_thread(session, description, require_review=require_review)
    handle = await fire_scheduled_worker(
        session,
        task_cfg,
        thread,
        event_description,
        cfg,
        session_mgr,
        thread_mgr,
        backend_override=None,
        prompt_override=None)
    if record_handle:
      self._handles[task_cfg.name] = handle
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
      session_cache: dict[str, list[SessionMetadata]] | None = None,
  ) -> SessionMetadata | None:
    """Return the active dedicated session for task/backend, rotating if needed.

    When session_cache is provided, uses it instead of scanning the sessions
    directory. Newly created sessions are added to the cache.
    """
    effective_backend, role, group = scheduled_task_session_binding(task_cfg, cfg)
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
    """Refresh the process-wide config so new tasks are picked up dynamically.

    Routes through the fingerprint-cached ``get_config``: an unchanged
    ``config.yaml`` costs one stat-key comparison per tick instead of a full
    YAML parse on the event loop, and a changed file still lands within one
    tick — the same freshness the per-tick disk read guaranteed.
    """
    try:
      self._cfg = get_config()
      return self._cfg
    except Exception as e:
      log.warning("scheduler_config_reload_failed", error=str(e))
      return self._cfg
