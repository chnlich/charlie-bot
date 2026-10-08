"""Scheduler — runs cron-like tasks against their bound task-tree nodes.

Every scheduled task carries a ``session_id`` binding to a task-tree manager
node, created and written back by the scheduler itself (auto-bind): the node —
not a dedicated cron session — parents the firings' leaves, follows the task's
backend, and takes over the cron session's wake duties. The legacy cron
sessions (``scheduled_task``-stamped) exist only as read-only history the
auto-bind archives at migration.
"""

import asyncio
import traceback
from datetime import datetime, timedelta
from typing import Any
from zoneinfo import ZoneInfo

from src.features.cron.config import ScheduledTaskConfig
from src.features.cron.cron_files import write_cron_key
from src.features.cron.loader import get_scheduled_tasks
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig, load_config, require_backend_option
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import LastRunStatus, SessionMetadata, SessionStatus, TaskType, parse_utc_datetime, utc_now_iso
from src.infra.tasks import cancel_and_wait, create_logged_task
from src.runtime.hooks import scheduled_handlers
from src.runtime.sessions import SessionManager

log = LazyStructlogLogger()

_TICK_INTERVAL = 60  # seconds between scheduler ticks

# The auto-bind request id prefix: derived from the task name alone, so a crash
# between the node create and the binding write-back — or a task deleted and
# re-created under the same name — replays into the SAME node.
_AUTO_BIND_REQUEST_PREFIX = "scheduled-node:"


def load_croniter(namespace: dict[str, Any]) -> Any:
  """Bind croniter into *namespace* on first use and return it.

  croniter (+ its dateutil subtree, ~21 ms together) is the server import
  floor's largest deferrable third-party slice and no import path resolves a
  next fire, so the import rides the first due-task or next-run resolution.
  The binding is per consumer module: each caller passes its own ``globals()``
  so its bare-name reads keep working.
  """
  from croniter import croniter

  namespace["croniter"] = croniter
  return croniter


def effective_scheduled_task_backend(task_cfg: ScheduledTaskConfig, cfg: CharlieBotConfig) -> str:
  """Return the backend id a scheduled task should use."""
  if task_cfg.backend:
    require_backend_option(cfg, task_cfg.backend, subject="scheduled task ")
    return task_cfg.backend
  if not cfg.backends.options:
    raise ValueError("scheduled task backend resolution requires a configured backends.options entry")
  return cfg.backends.options[0].id


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
    await cancel_and_wait(self._task)
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
    return await self._execute_task(task_cfg, firing=utc_now_iso())

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

    # Binding pass: every valid task — enabled or disabled, handler tasks
    # included — is bound on the tick that sees it (the plan guarantees the
    # session_id, and a disabled task must still show as a node), and a bound
    # task never keeps an active cron session. One task's failure must not
    # starve the others' binding or firing.
    for task_cfg in tasks:
      try:
        if not task_cfg.session_id:
          await self._auto_bind(task_cfg, cfg, session_cache)
        else:
          await self._archive_active_cron_sessions(task_cfg.name, session_cache)
      except Exception as e:
        log.error("scheduler_bind_error", task=task_cfg.name, error=str(e), traceback=traceback.format_exc())

    # Backend pass: the bound node's backend follows the task config (a
    # cron-editor change, or a hand-edited yaml seen by this tick), switched in
    # place — never by creating a session.
    for task_cfg in tasks:
      if not task_cfg.session_id:
        continue  # the binding pass already reported this task's error
      try:
        await self._align_bound_backend(task_cfg, cfg)
      except Exception as e:
        log.error("scheduler_backend_align_error", task=task_cfg.name, error=str(e), traceback=traceback.format_exc())

    for task_cfg in tasks:
      if not task_cfg.enabled or not task_cfg.session_id:
        continue  # disabled tasks fire nothing; an unbound one already errored above
      try:
        await self._maybe_run(task_cfg, session_mgr, session_cache, cfg)
      except Exception as e:
        log.error("scheduler_task_error", task=task_cfg.name, error=str(e), traceback=traceback.format_exc())

  # ---------------------------------------------------------------------------
  # Auto-bind: the scheduler guarantees the session_id
  # ---------------------------------------------------------------------------

  async def _auto_bind(
      self,
      task_cfg: ScheduledTaskConfig,
      cfg: CharlieBotConfig,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
  ) -> None:
    """Bind an unbound task to its task-tree manager node, replacing the
    cron-session creation path.

    Fixed order, each step idempotent: (1) create or reattach the node, (2)
    copy the old cron session's scheduler bookkeeping, (3) write the binding
    back, (4) archive the old cron sessions. ``task_cfg.session_id`` is set
    only after the write-back lands, so the same tick's fire evaluates against
    the node instead of routing back to a cron session.
    """
    from src.runtime.api.deps import task_manager

    tree = task_manager()
    backend = effective_scheduled_task_backend(task_cfg, cfg)
    # Step 1 — create or reattach the node. The request id derives from the
    # task name alone, so a crash replays into the same node, and a task
    # deleted and re-created under the same name reattaches to its original
    # one. A replayed node the user archived comes back: the task must fire
    # into an open node, so the archived chain restores with the auto-bind as
    # its named source (a legacy stored-status archive keeps its legacy
    # restore).
    node = await tree.create_task(
        request_id=f"{_AUTO_BIND_REQUEST_PREFIX}{task_cfg.name}",
        task_parent_id=None,
        profile="manager",
        task=None,
        name=task_cfg.name,
        backend=backend,
        group=task_cfg.project,
        caller="system",
    )
    if node.status == SessionStatus.ARCHIVED:
      unarchived = await self._session_mgr.unarchive_session(node.id)
      if unarchived is None:
        raise RuntimeError(f"scheduled task '{task_cfg.name}' node {node.id} vanished during unarchive")
      node = unarchived
    if tree.task_state(node.id) != "open":
      restored = await tree.completion.restore_chain(
          node.id, request_id=f"{_AUTO_BIND_REQUEST_PREFIX}{task_cfg.name}:restore", reason="cron auto-bind")
      if not restored:
        raise RuntimeError(f"scheduled task '{task_cfg.name}' node {node.id} stayed archived during auto-bind")
      node = await tree.load_task_meta(node.id)
    # Step 2 — copy the newest active cron session's scheduler bookkeeping
    # onto the node, so the next fire is computed from the true last
    # occurrence: no catch-up, no missed fire at the migration moment.
    old = await self._newest_active_cron_session(task_cfg.name, session_cache)
    if old is not None:
      await tree.adopt_metadata_slot(
          node.id, old, "cron", fields=("last_scheduled_run", "last_scheduled_cron", "last_run_status"))
    # Step 3 — write the binding back through the single-key write: only the
    # session_id key changes.
    await asyncio.to_thread(write_cron_key, task_cfg.name, "session_id", node.id)
    task_cfg.session_id = node.id
    # Step 4 — archive every active cron session of the task, unconditionally
    # (no busy gate: a legacy thread stuck at running must not hold the
    # archive off for the 30-day scan window; a firing leaf still running
    # under it finishes normally).
    await self._archive_active_cron_sessions(task_cfg.name, session_cache)
    log.info("scheduled_task_auto_bound", task=task_cfg.name, session=node.id, backend=backend)

  async def _newest_active_cron_session(
      self,
      task_name: str,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
  ) -> SessionMetadata | None:
    """The task's newest active cron session, or None when it has none.

    Sorted by created_at, not updated_at: unarchiving refreshes updated_at, so
    a pulled-back old generation would otherwise rank newest and donate its
    (stale) bookkeeping at the migration.
    """
    sessions = session_cache.get(task_name) if session_cache is not None else None
    if sessions is None:
      sessions = [
          s for s in await self._session_mgr.list_sessions(scheduled=True, include_running_status=False)
          if s.scheduled_task == task_name
      ]
    active = sorted((s for s in sessions if s.status == SessionStatus.ACTIVE), key=lambda s: s.created_at, reverse=True)
    return active[0] if active else None

  async def _archive_active_cron_sessions(
      self,
      task_name: str,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
  ) -> list[str]:
    """Archive every active cron session of *task_name*, unconditionally.

    Runs every tick for bound tasks too: it covers a crash between the binding
    write-back and the archive, and any later wake that re-activates one.
    Archiving never touches the task's yaml.
    """
    sessions = session_cache.get(task_name) if session_cache is not None else None
    if sessions is None:
      sessions = [
          s for s in await self._session_mgr.list_sessions(scheduled=True, include_running_status=False)
          if s.scheduled_task == task_name
      ]
    archived: list[str] = []
    for session in sessions:
      if session.status != SessionStatus.ACTIVE:
        continue
      await self._session_mgr.archive_session(session.id)
      session.status = SessionStatus.ARCHIVED  # the tick's cache copy stays honest
      archived.append(session.id)
    if archived:
      log.info("scheduled_cron_sessions_archived", task=task_name, sessions=archived)
    return archived

  async def _align_bound_backend(self, task_cfg: ScheduledTaskConfig, cfg: CharlieBotConfig) -> None:
    """The bound node's backend follows the task config, switched in place.

    A cron-editor backend change or a hand-edited yaml reaches the node here:
    the existing in-place switch, never a new session. The fire itself resolves
    the backend from the task config independently, so a switched node and a
    due fire in the same tick agree.
    """
    backend = effective_scheduled_task_backend(task_cfg, cfg)
    node = await self._session_mgr.get_session(task_cfg.session_id)
    if node is None or node.backend == backend:
      return
    await self._session_mgr.switch_backend(task_cfg.session_id, backend)
    log.info("scheduled_node_backend_aligned", task=task_cfg.name, session=node.id, backend=backend)

  async def _maybe_run(
      self,
      task_cfg: ScheduledTaskConfig,
      session_mgr: SessionManager,
      session_cache: dict[str, list[SessionMetadata]],
      cfg: CharlieBotConfig | None,
  ) -> None:
    cfg = cfg or self._cfg
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)

    if not task_cfg.session_id:
      # An unbound task binds on the tick that sees it (auto-bind); a binding
      # failure propagates to the tick's per-task error log.
      await self._auto_bind(task_cfg, cfg, session_cache)
    # A bound task fires against its stable task-tree node: the binding is
    # validated here (a missing/legacy node fails the tick visibly), never
    # discovered or replaced.
    from src.features.cron.cron_sequence import check_fireable_binding
    from src.runtime.api.deps import task_manager
    tree = task_manager()
    session = await check_fireable_binding(task_cfg, tree)

    # Detect cron expression change — reset last_scheduled_run to now and skip tick
    if session.last_scheduled_cron is not None and session.last_scheduled_cron != task_cfg.cron:
      log.info("scheduler_cron_changed", task=task_cfg.name, old=session.last_scheduled_cron, new=task_cfg.cron)
      await tree.update_slot_fields(
          session.id, "cron", last_scheduled_run=now.isoformat(), last_scheduled_cron=task_cfg.cron)
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
        await tree.update_slot_fields(
            session.id, "cron", last_scheduled_run=now.isoformat(), last_run_status=LastRunStatus.SKIPPED)
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

  async def _execute_task(
      self,
      task_cfg: ScheduledTaskConfig,
      record_handle: bool = False,
      firing: str | None = None,
  ) -> dict:
    """Execute one fire through the single bound code path.

    An unbound task binds first (auto-bind), so the fire — a manual
    ``run_task_now`` included — evaluates against the node, never a cron
    session. ``firing`` carries the due occurrence's time (an explicit manual
    fire passes its own). ``record_handle`` gates whether the background round
    spawned by this fire is registered in the overlap-skip registry: the
    scheduled path records it via ``_maybe_run``; manual ``run_task_now``
    leaves it off so manual rounds stay outside the skip judgment.
    """
    if not task_cfg.session_id:
      await self._auto_bind(task_cfg, self._reload_config())
    return await self._execute_bound_task(task_cfg, record_handle=record_handle, firing=firing or utc_now_iso())

  # ---------------------------------------------------------------------------
  # Bound (task-tree) execution — the v2 path
  # ---------------------------------------------------------------------------

  async def _execute_bound_task(
      self,
      task_cfg: ScheduledTaskConfig,
      *,
      record_handle: bool,
      firing: str,
  ) -> dict:
    """Fire one task against its node (src.features.cron.cron_sequence owns the shapes).

    The binding resolves strictly by the task's ``session_id``.
    """
    from src.features.cron.cron_sequence import check_fireable_binding, fire_bound_master, run_firing_steps
    from src.runtime.api.deps import task_manager

    self._reload_config()
    tree = task_manager()
    meta = await check_fireable_binding(task_cfg, tree)

    if task_cfg.mode == 'master':
      # mode: master wakes the bound manager node — the durable input IS the
      # admission: the checkpoint advances only once the firing's product
      # exists durably, so a crash or admission failure in between leaves the
      # occurrence unconsumed and the replay re-admits the SAME input (stable
      # firing identity), never a duplicate.
      await fire_bound_master(task_cfg, meta, tree, firing)
      await self._record_bound_fire(meta, task_cfg)
      return {"session_id": meta.id, "firing": firing}

    if task_cfg.handler:
      # The system handler keeps its inline execution (its side effects were
      # never exactly-once); its bookkeeping borrows the bound node in the
      # legacy order — recorded before the handler runs, so a handler crash
      # consumes the occurrence rather than re-running the side effect.
      await self._record_bound_fire(meta, task_cfg)
      return await self._execute_handler_on_bound(task_cfg, meta)

    # Backend resolution fails before anything durable exists: the occurrence
    # stays unconsumed and the tick error is visible.
    backend, model = self._resolve_bound_backend_model(task_cfg, tree)

    if task_cfg.steps:
      leaf = await self._bound_leaf(task_cfg, meta, tree, firing, goal=f"{task_cfg.name} steps")
      from src.features.cron.cron_sequence import register_leaf_run
      # The firing's first durable product is the leaf's first step Run: the
      # checkpoint advances only after it exists, so a replayed occurrence
      # re-admits the same leaf/Run (stable firing identity).
      await register_leaf_run(
          tree, leaf.id, task_cfg, firing, kind="scheduled_step", position=0, backend=backend, model=model)
      await self._record_bound_fire(meta, task_cfg)
      handle = create_logged_task(
          run_firing_steps(task_cfg, meta, tree, firing, leaf.id), name=f"bound_steps_{task_cfg.name}_{firing}")
      if record_handle:
        self._handles[task_cfg.name] = handle
      return {"session_id": meta.id, "leaf_session_id": leaf.id, "firing": firing}

    prompt, action = await self._resolve_bound_prompt(task_cfg, meta)
    if prompt is None:
      # The loop decided nothing to do: the occurrence is consumed by that
      # decision (the no-op loop semantics, preserved — the checkpoint still
      # advances, or the same occurrence would refire every tick).
      await self._record_bound_fire(meta, task_cfg)
      return {"session_id": meta.id, "firing": firing, "skipped": action}
    # The loop's implement action is implementation delegation: its leaf
    # carries the implement task type, so the review + landing delivery policy
    # applies with the repo's default branch as the merge target. Every other
    # action stays a type-less leaf whose success closes it.
    leaf = await self._bound_leaf(
        task_cfg, meta, tree, firing, goal=prompt, task_type=TaskType.IMPLEMENT if action == "implement" else None)
    from src.features.cron.cron_sequence import register_leaf_run
    await register_leaf_run(tree, leaf.id, task_cfg, firing, kind="work", position=None, backend=backend, model=model)
    await self._record_bound_fire(meta, task_cfg)
    handle = await self._launch_bound_round(
        task_cfg, meta, tree, firing, leaf.id, backend=backend, model=model, record_handle=record_handle)
    return {"session_id": meta.id, "leaf_session_id": leaf.id, "firing": firing}

  async def _record_bound_fire(
      self,
      meta: SessionMetadata,
      task_cfg: ScheduledTaskConfig,
  ) -> None:
    """The scheduler's per-fire bookkeeping on the firing's node.

    The write goes through the tree metadata owner, which re-reads the node
    under the control lock: a concurrent task edit between the caller's load
    and this write is preserved instead of being overwritten by the stale
    SessionMetadata snapshot. The node is the bound task's stable binding.
    """
    tz = ZoneInfo(task_cfg.timezone)
    now = datetime.now(tz)
    from src.runtime.api.deps import task_manager
    await task_manager().update_slot_fields(
        meta.id, "cron", last_scheduled_run=now.isoformat(), last_scheduled_cron=task_cfg.cron)

  def _resolve_bound_backend_model(
      self,
      task_cfg: ScheduledTaskConfig,
      tree,
  ) -> tuple[str, str | None]:
    from src.features.cron.cron_sequence import resolved_backend_model
    return resolved_backend_model(task_cfg, tree, task_cfg.backend)

  async def _resolve_bound_prompt(
      self,
      task_cfg: ScheduledTaskConfig,
      meta: SessionMetadata,
  ) -> tuple[str | None, str | None]:
    """The fire's prompt: the loop action's decision, or the task prompt verbatim.

    Returns (prompt, action); prompt None means the registered loop action decided
    nothing to do (noop/stale_reset) and the fire is recorded as such.
    """
    if task_cfg.loop:
      action_type, prompt = await scheduled_handlers.loop_action()(
          name=task_cfg.name, repo=task_cfg.repo, loop=task_cfg.loop)
      if prompt is None:
        from src.runtime.api.deps import task_manager
        await task_manager().update_slot_fields(meta.id, "cron", last_run_status=LastRunStatus.SUCCESS)
        log.info("bound_loop_task_noop", task=task_cfg.name, action=action_type, session=meta.id)
        return None, action_type
      return prompt, action_type
    return task_cfg.prompt or "", None

  async def _bound_leaf(
      self,
      task_cfg: ScheduledTaskConfig,
      meta: SessionMetadata,
      tree,
      firing: str,
      *,
      goal: str,
      task_type: TaskType | None = None,
  ):
    from src.features.cron.cron_sequence import ensure_firing_leaf
    return await ensure_firing_leaf(task_cfg, meta, tree, firing, goal, task_type=task_type)

  async def _launch_bound_round(
      self,
      task_cfg: ScheduledTaskConfig,
      meta: SessionMetadata,
      tree,
      firing: str,
      leaf_id: str,
      *,
      backend: str,
      model: str | None,
      record_handle: bool,
  ) -> asyncio.Task:
    """One firing's work round: launch (scheduler-owned) and settle for the
    overlap registry. The ordinary work-Run delivery chain owns the success
    and failure follow-ups; a launch withheld by its preconditions leaves the
    queued Run as the retained pending request with its durable withheld
    record and blocked report, and only releases the overlap handle."""
    from src.features.cron.cron_sequence import launch_and_settle, register_leaf_run

    async def _round() -> None:
      run = await register_leaf_run(
          tree, leaf_id, task_cfg, firing, kind="work", position=None, backend=backend, model=model)
      observation = await launch_and_settle(tree, leaf_id, run.id, prompt=None)
      if observation.withheld is not None:
        # No process started and no terminal fact will arrive. The launch
        # itself recorded the durable run_launch_withheld fact and delivered
        # the ONE blocked report to the parent (once, by stable id); releasing
        # the overlap handle is all this round still owes.
        log.info(
            "bound_round_launch_withheld", task=task_cfg.name, leaf=leaf_id, run=run.id, reason=observation.withheld)

    handle = create_logged_task(_round(), name=f"bound_worker_{task_cfg.name}_{firing}")
    if record_handle:
      self._handles[task_cfg.name] = handle
    log.info("bound_task_fired", task=task_cfg.name, session=meta.id, leaf=leaf_id, firing=firing)
    return handle

  async def _execute_handler_on_bound(
      self,
      task_cfg: ScheduledTaskConfig,
      meta: SessionMetadata,
  ) -> dict:
    """The built-in handler modes on a bound node: inline execution, bound bookkeeping."""
    handler = scheduled_handlers.handler(task_cfg.handler)
    if handler is None:
      raise ValueError(f"Unknown handler: {task_cfg.handler!r}")
    session = meta
    log.info('handler_task_firing', task=task_cfg.name, handler=task_cfg.handler)
    from src.runtime.api.deps import task_manager
    try:
      result = await handler()
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': ET.HANDLER_STATUS_OK,
          'message': str(result) if result is not None else 'done',
      }
      status = LastRunStatus.SUCCESS
    except Exception as e:
      log.warning('handler_task_error', task=task_cfg.name, error=str(e), traceback=traceback.format_exc())
      event = {
          'type': ET.HANDLER_RESULT,
          'task': task_cfg.name,
          'status': ET.HANDLER_STATUS_ERROR,
          'message': str(e),
      }
      status = LastRunStatus.FAILED
    await task_manager().update_slot_fields(session.id, "cron", last_run_status=status)
    await self._session_mgr.persist_and_broadcast(session.id, event)
    return {'session_id': session.id, 'thread_id': None}

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
