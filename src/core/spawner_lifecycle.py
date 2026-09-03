"""Worker run entry points — spawn and resume, from process setup through the finalize chain."""

import asyncio
import signal
import traceback
from collections.abc import Awaitable, Callable
from pathlib import Path

import structlog

from src.agents.worker import QuotaExhaustedException, Worker
from src.core import runs, spawner_backends, spawner_finalize, spawner_launch
from src.core.config import CharlieBotConfig
from src.core.models import SpawnRequest, TaskType
from src.core.process import kill_process_group
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()


async def spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    request: SpawnRequest,
) -> None:
  """Spawn a worker for the given thread on its resolved backend. Fire-and-forget via asyncio.create_task()."""
  thread = None
  worker = None
  outcome = spawner_finalize._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error="")
  cancelled = False
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("spawn_worker_thread_missing", session=session_id, thread_id=thread_id)
      return

    if request.repo_path is None:
      worker = await spawner_launch._create_repoless_process(session_id, thread, description, cfg, thread_mgr, request)
    else:
      resolved_repo = Path(request.repo_path).resolve()
      worker = await spawner_launch._create_worktree_and_process(
          session_id, thread, description, cfg, session_mgr, thread_mgr, resolved_repo, request)

    outcome = await spawner_finalize._stream_worker_events(worker, session_id, thread, thread_mgr, session_mgr)

    # Re-run an exhausted VERIFY once on the next untried checking-role backend;
    # with none remaining (selection empty, or looped back to the exhausted
    # backend) the existing outcome is already spawner_finalize._QUOTA_EXHAUSTED_OUTCOME and
    # stands as-is.
    if request.task_type == TaskType.VERIFY and outcome.quota_exhausted:
      current_backend, _ = spawner_backends.require_thread_backend_model(thread, cfg)
      tried_backends = thread.tried_backends
      retry_backend = await spawner_backends.select_verify_backend(session_id, cfg, session_mgr, tried_backends)
      if retry_backend is None or retry_backend[0] == current_backend:
        log.warning(
            "verify_quota_retry_backend_unavailable",
            thread_id=thread.id,
            current_backend=current_backend,
            tried=tried_backends,
        )
      else:
        resolved_backend, resolved_model, tried_backends = retry_backend
        log.warning(
            "verify_quota_retry",
            thread_id=thread.id,
            exhausted_backend=thread.backend,
            retry_backend=resolved_backend,
        )
        request.resolved_backend = resolved_backend
        request.resolved_model = resolved_model
        thread.tried_backends = tried_backends
        worker = await spawner_launch._create_repoless_process(
            session_id, thread, description, cfg, thread_mgr, request)
        outcome = await spawner_finalize._stream_worker_events(worker, session_id, thread, thread_mgr, session_mgr)

    if outcome.failed and not outcome.error:
      outcome = outcome._replace(
          exit_code=await spawner_finalize._maybe_override_exit_code_from_result(
              outcome.exit_code, session_id, thread, thread_mgr))

  except asyncio.CancelledError:
    # Only live trigger: event-loop shutdown (graceful restart). Never write a
    # terminal state here — the thread stays as-is on disk and the next boot's
    # reconcile judges the truth via resolve_run: covered transports are
    # re-attached for their real result, everything else is finalized with an
    # explicit reason. Covered workers keep running on their own raw-log fds
    # (detach stops the closing loop's transports from killing them);
    # uncovered transports and improve iterations (whose loop dies with this
    # process) cannot outlive the server usefully, so their processes are
    # still terminated.
    cancelled = True
    transport = runs.backend_type(cfg, thread.backend if thread else None)
    let_go = (
        worker is not None and transport not in runs.UNCOVERED_BACKEND_TYPES and
        not description.startswith(runs.IMPROVE_ITERATION_PREFIX))
    log.warning(
        "spawn_worker_cancelled",
        session=session_id,
        thread_id=thread_id,
        transport=transport,
        action="let_go" if let_go else ("terminate" if worker else "none"),
    )
    if worker:
      if let_go:
        worker.detach()
      else:
        await worker.terminate()
    raise
  except Exception as e:
    # Every failure landing here is a generic setup error, never quota
    # exhaustion — a VERIFY retry's own setup failure included — so the
    # outcome is rebuilt rather than patched in place.
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    outcome = spawner_finalize._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error=str(e))
  finally:
    if thread is not None and not cancelled:
      await spawner_finalize._finalize_worker_safely(
          session_id,
          description,
          thread,
          outcome,
          thread_mgr,
          session_mgr,
          cfg,
          skip_notify=request.skip_notify,
          task_type=request.task_type)


# Recovery-event reason recorded when the finalize liveness gate keeps a run
# alive: resume hit an exception or cancellation while the run's death was
# unproven, so the FAILED finalize was skipped (resume-exception-alive).
RESUME_EXCEPTION_ALIVE_REASON = "resume-exception-alive"


async def resume_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    *,
    is_alive: Callable[[], bool],
    interrupt_reason: str,
    on_silence: Callable[[], Awaitable[None]] | None,
) -> None:
  """Re-attach to an interrupted run's raw stream, then run the finalize chain.

  Server-startup entry point: RUNNING/STALLED outcomes pass the recorded
  (pid, pid_start) liveness judgment as ``is_alive``; COMPLETED/DIED drains
  pass ``lambda: False``. Finalize is judgment-idempotent, so finishing here
  and finishing without a restart take exactly the same path.

  ``interrupt_reason`` is the reconcile's resolve_run reason: when the drain
  ends without a successful result and no harder error occurred, it becomes
  the finalize error, so the master's summary states why the run failed
  instead of a bare exit -1.

  ``on_silence`` is the follow-time silence recheck, forwarded to the
  re-attached follow loop.

  Liveness gate: before a FAILED outcome is recorded under any exception or
  cancellation path (asyncio.CancelledError included — it bypasses
  ``except Exception`` and lands in the same finally), the same ``is_alive``
  probe is consulted. Probe true (alive, or constant-true because death is
  unverifiable) means the failure is our own, not the run's: a recovery event
  is emitted and the FAILED finalize is skipped, leaving the thread running.
  Probe false (proven dead) finalizes exactly as before. Normal completion
  (exit_code 0) and the quota branch (which kills the process itself) are
  not gated.
  """
  thread = None
  worker = None
  outcome = spawner_finalize._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error="")
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("resume_worker_thread_missing", session=session_id, thread_id=thread_id)
      return
    backend_option = None
    if thread.backend:
      try:
        backend_option = spawner_backends.resolve_backend_option(cfg, thread.backend, thread.model)
      except ValueError as e:
        # Translate-only fallback: a stale backend id degrades result
        # detection to the raw claude shape, never crashes recovery.
        log.warning("resume_backend_unresolved", thread_id=thread.id, error=str(e))
    events_log = await thread_mgr.get_events_log_path(session_id, thread.id)
    working_dir = Path(thread.worktree_path) if thread.worktree_path else events_log.parent
    worker = Worker(thread, working_dir, events_log, description, cfg, backend_option=backend_option)
    outcome = spawner_finalize._WorkerRunOutcome(
        exit_code=await worker.resume(is_alive=is_alive, on_silence=on_silence), quota_exhausted=False, error="")
  except QuotaExhaustedException:
    log.warning("resume_worker_quota_exhausted", thread_id=thread_id)
    if is_alive() and thread is not None and thread.pid is not None:
      kill_process_group(thread.pid, signal.SIGTERM)
    outcome = spawner_finalize._QUOTA_EXHAUSTED_OUTCOME
  except Exception as e:
    log.error("resume_worker_failed", thread_id=thread_id, error=str(e), traceback=traceback.format_exc())
    outcome = spawner_finalize._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error=str(e))
  finally:
    if outcome.failed and not outcome.error and interrupt_reason:
      outcome = outcome._replace(error=interrupt_reason)
    if thread is not None:
      if outcome.failed and is_alive():
        # Alive or unverifiable death: never record FAILED on our own error.
        log.warning("resume_finalize_skipped_alive", thread_id=thread_id, error=outcome.error)
        from src.core import (
            init as init_module,  # lazy: init imports this module lazily too
        )
        await init_module._report_recovery_event(
            session_mgr, session_id,
            f"Worker thread {thread_id[:8]} hit a resume error but its process cannot be proven "
            f"dead ({RESUME_EXCEPTION_ALIVE_REASON}). It is NOT being killed — left running "
            "and judged again on the next restart.")
      else:
        await spawner_finalize._finalize_worker_safely(
            session_id,
            description,
            thread,
            outcome,
            thread_mgr,
            session_mgr,
            cfg,
            skip_notify=False,
            task_type=thread.task_type or TaskType.IMPLEMENT)
