"""Run outcome and the finalize chain — status write, worktree cleanup, completion notify."""

import traceback
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple

import structlog

from src.agents.worker import QuotaExhaustedException, Worker
from src.core import event_types as ET
from src.core import finalize_effects, review, runs, spawner_events
from src.core.config import CharlieBotConfig, get_scheduled_tasks
from src.core.git import (
  git_worktree_dir_name,
  git_worktree_prune,
  git_worktree_remove,
)
from src.core.models import TaskType, ThreadMetadata, ThreadStatus
from src.core.notifications import send_telegram
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.verify_trailer import (
  read_verify_final_report,
  verify_result_trailer_error,
)

log = structlog.get_logger()


class _WorkerRunOutcome(NamedTuple):
  """A worker run's terminal disposition: process exit code, quota-exhaustion flag, setup error."""

  exit_code: int
  quota_exhausted: bool
  error: str

  @property
  def failed(self) -> bool:
    """A non-zero exit outside the quota branch — the FAILED outcome the liveness gate and
    the exit-code/error backfills negotiate over; exit 0 and quota exhaustion take their
    own recorded paths."""
    return self.exit_code != 0 and not self.quota_exhausted


# The quota flag, not the error text, drives the finalize chain's quota branch.
_QUOTA_EXHAUSTED_OUTCOME = _WorkerRunOutcome(exit_code=-1, quota_exhausted=True, error="")


async def _stream_worker_events(
    worker: Worker,
    session_id: str,
    thread: ThreadMetadata,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
) -> _WorkerRunOutcome:
  """Mark worker running, broadcast start event, run worker, and report its outcome."""
  thread.status = ThreadStatus.RUNNING
  thread.started_at = datetime.now(timezone.utc)
  await thread_mgr.save_metadata(thread)
  log.info("worker_running", thread_id=thread.id, session=session_id)

  await session_mgr.persist_and_broadcast(
      session_id, spawner_events._thread_worker_event(thread, 'running', '', content=None))

  try:
    return _WorkerRunOutcome(exit_code=await worker.run(), quota_exhausted=False, error="")
  except QuotaExhaustedException:
    await worker.terminate()
    log.warning("worker_quota_exhausted", thread_id=thread.id)
    return _QUOTA_EXHAUSTED_OUTCOME
  except Exception as e:
    await worker.terminate()
    log.error("worker_failed", thread_id=thread.id, error=str(e), traceback=traceback.format_exc())
    return _WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error=str(e))


async def _maybe_override_exit_code_from_result(
    exit_code: int,
    session_id: str,
    thread: ThreadMetadata,
    thread_mgr: ThreadManager,
) -> int:
  """Override a non-zero exit_code to 0 when the last result event in events.jsonl is a success.

  Workers killed by SIGTERM (exit 143) after emitting a success result event should not be
  treated as failures. Any failure reading events.jsonl is logged and the original exit_code
  is returned -- this path must never raise.
  """
  if exit_code == 0:
    return exit_code
  try:
    events = await spawner_events._read_thread_events(session_id, thread.id, thread_mgr)
  except Exception as e:
    log.warning("worker_exit_override_read_failed", thread_id=thread.id, error=str(e))
    return exit_code

  for ev in reversed(events):
    if ev.get("type") != ET.RESULT:
      continue
    if ev.get("subtype") == "success" and ev.get("is_error") in (False, None):
      log.warning(
          "worker_exit_overridden_by_result_subtype",
          thread_id=thread.id,
          original_exit_code=exit_code,
      )
      return 0
    break
  return exit_code


async def _cleanup_worker_directory(thread: ThreadMetadata, skip_cleanup: bool, worktree_parent: Path) -> str | None:
  """Remove the worker's worktree after a successful run.

  Returns an error message when the (success-path) cleanup fails so the caller can
  surface it; returns None on success or when there is nothing to clean.
  """
  if skip_cleanup:
    return None

  if not thread.worktree_path or not thread.repo_path:
    return None
  wt = Path(thread.worktree_path)
  if not wt.exists():
    return None
  if not thread.branch_name:
    raise RuntimeError(f"thread {thread.id} has worktree_path but no branch_name")
  try:
    removed = await git_worktree_remove(
        thread.repo_path,
        wt,
        thread.id,
        allowed_parent=worktree_parent,
        expected_residue_name=git_worktree_dir_name(thread.branch_name),
    )
  except Exception as wt_err:
    log.error("worktree_cleanup_error", thread_id=thread.id, worktree=str(wt), error=str(wt_err), exc_info=True)
    return f"Worktree cleanup failed for {wt}: {wt_err}"
  if not removed:
    log.error("worktree_cleanup_remove_failed", thread_id=thread.id, worktree=str(wt))
    return f"Worktree cleanup failed for {wt}: git worktree remove reported failure"
  await git_worktree_prune(thread.repo_path, thread.id)
  return None


def _should_skip_worktree_cleanup(thread: ThreadMetadata, exit_code: int) -> bool:
  """Decide whether the worker's worktree must survive past this exit.

  Survives for: keep_worktree pin, explicit skip_cleanup, reviewer chain, a non-zero
  exit (failures keep their worktree so the debug state is not destroyed), or the
  reviewer-handoff handled on the zero-exit success path.
  """
  if thread.keep_worktree:
    return True
  if thread.skip_cleanup:
    return True
  if thread.review_of:
    return True
  if exit_code != 0:
    return True
  can_spawn_reviewer = all([thread.repo_path, thread.branch_name, thread.worktree_path])
  return thread.require_review and can_spawn_reviewer


async def _run_finalize_effects(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    outcome: _WorkerRunOutcome,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    skip_notify: bool,
    verify_report: str | None,
) -> None:
  """Run a finalized thread's side effects — worktree cleanup, then the notify chain.

  Holds no status write: the caller owns that. Every effect behind this call is
  judgment-idempotent (src/core/finalize_effects), so repetition converges to a no-op.
  """
  skip_cleanup = _should_skip_worktree_cleanup(thread, outcome.exit_code)
  cleanup_error = await _cleanup_worker_directory(thread, skip_cleanup, Path(cfg.worktree_dir))
  if cleanup_error:
    await session_mgr.persist_and_broadcast(session_id, {"type": ET.ERROR, "content": cleanup_error})

  if skip_notify:
    return
  await _notify_completion(
      session_id,
      description,
      thread,
      outcome,
      thread_mgr,
      session_mgr,
      cfg,
      verify_report=verify_report)


async def _thread_cancelled(thread_mgr: ThreadManager, session_id: str, thread_id: str) -> bool:
  """Re-read the thread from disk: the cancel endpoint may have already set CANCELLED."""
  current = await thread_mgr.get_thread(session_id, thread_id)
  return current is not None and current.status == ThreadStatus.CANCELLED


def _exit_status_label(exit_code: int) -> str:
  """The worker_summary status label for a run's exit code."""
  return "completed" if exit_code == 0 else "failed"


async def _verify_report_for_task(
    task_type: TaskType,
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager,
) -> str | None:
  """The run's verifier final report when the task is VERIFY; None for every other task type."""
  if task_type != TaskType.VERIFY:
    return None
  return await read_verify_final_report(session_id, thread_id, thread_mgr)


async def _finalize_worker(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    outcome: _WorkerRunOutcome,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    skip_notify: bool,
    task_type: TaskType,
    completed_at: datetime | None,
) -> None:
  """Update thread status and notify completion.

  ``completed_at`` overrides the terminal-status timestamp when given.
  """
  cancelled = await _thread_cancelled(thread_mgr, session_id, thread.id)
  # One read of events.jsonl per finalize pass: the trailer gate below and the
  # notify chain's summary must judge and quote the identical string.
  verify_report = await _verify_report_for_task(task_type, session_id, thread.id, thread_mgr)
  if verify_report is not None and not cancelled and not outcome.quota_exhausted:
    trailer_error = verify_result_trailer_error(verify_report)
    if trailer_error:
      outcome = outcome._replace(
          exit_code=-1, error=f"{outcome.error}; {trailer_error}" if outcome.error else trailer_error)

  if cancelled:
    # Cancel endpoint already set the status; don't overwrite.
    log.info("worker_already_cancelled", thread_id=thread.id)
  elif outcome.quota_exhausted:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, completed_at=completed_at)
  elif outcome.error:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=-1, completed_at=completed_at)
  elif outcome.exit_code == 0:
    await thread_mgr.update_status(
        session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0, completed_at=completed_at)
    log.info("worker_completed", thread_id=thread.id)
  else:
    await thread_mgr.update_status(
        session_id, thread.id, ThreadStatus.FAILED, exit_code=outcome.exit_code, completed_at=completed_at)
    log.warning("worker_failed_nonzero", thread_id=thread.id, exit_code=outcome.exit_code)

  await _run_finalize_effects(
      session_id,
      description,
      thread,
      outcome,
      thread_mgr,
      session_mgr,
      cfg,
      skip_notify=skip_notify,
      verify_report=verify_report)


async def _finalize_worker_safely(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    outcome: _WorkerRunOutcome,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    skip_notify: bool,
    task_type: TaskType,
) -> None:
  """Finalize a worker thread; on failure, log and best-effort-broadcast a session ERROR event."""
  try:
    # completed_at is the run's true end — its raw log's final mtime — so server
    # downtime before finalization never shifts the recorded end.
    thread_dir = thread_mgr.thread_dir(session_id, thread.id)
    completed_at = runs.raw_completion_time(runs.raw_log_path(thread_dir))
    await _finalize_worker(
        session_id,
        description,
        thread,
        outcome,
        thread_mgr,
        session_mgr,
        cfg,
        skip_notify=skip_notify,
        task_type=task_type,
        completed_at=completed_at)
  except Exception as e:
    log.error("spawn_worker_finalize_failed", session=session_id, traceback=traceback.format_exc())
    try:
      await session_mgr.persist_and_broadcast(
          session_id, {
              "type": ET.ERROR,
              "content": f"Worker finalization failed: {e}"
          })
    except Exception:
      log.warning("spawn_worker_finalize_broadcast_failed", session=session_id, exc_info=True)


async def recomplete_finalize_effects(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    task_type: TaskType,
) -> None:
  """Re-run ONLY the side effects of _finalize_worker — never the status write.

  Startup-reconcile entry point for threads already marked terminal when a
  crash interrupted their notify chain. Every effect behind this call is
  judgment-idempotent (src/core/finalize_effects), so repetition converges to
  a no-op; the status/completed_at fields are left exactly as recorded. The
  original quota/error outcome state is not persisted, so a re-run summarizes
  from exit_code alone.
  """
  verify_report = await _verify_report_for_task(task_type, session_id, thread.id, thread_mgr)
  await _run_finalize_effects(
      session_id,
      description,
      thread,
      _WorkerRunOutcome(exit_code=exit_code, quota_exhausted=False, error=""),
      thread_mgr,
      session_mgr,
      cfg,
      skip_notify=False,
      verify_report=verify_report)


async def _persist_worker_summary_once(
    session_id: str,
    thread_id: str,
    event: dict,
    session_mgr: SessionManager,
    *,
    fallback: bool,
) -> None:
  """Persist and broadcast a worker_summary event unless the session already carries one.

  Idempotency judgment: a rerun of the finalize chain (e.g. startup reconcile
  completing a crashed finalize) never duplicates the summary. ``fallback`` marks
  the degraded summary written when the notify chain itself failed.
  """
  if finalize_effects.terminal_summary_present(session_mgr.load_chat_events_sync(session_id), thread_id):
    log.info("worker_summary_skip_duplicate", session=session_id, thread=thread_id, fallback=fallback)
    return
  await session_mgr.mark_unread(session_id)
  await session_mgr.persist_and_broadcast(session_id, event)
  log.info("worker_summary_sent", session=session_id, thread=thread_id, fallback=fallback)


async def _broadcast_completion(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    outcome: _WorkerRunOutcome,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    *,
    verify_report: str | None,
) -> tuple[str, str]:
  """Build and broadcast the worker_summary event. Returns (events_summary, full_summary).

  A non-None ``verify_report`` marks the run as VERIFY: _verify_report_for_task returns
  None for every other task type.
  """
  if verify_report is not None:
    events_summary = verify_report
  else:
    events_summary = await spawner_events.read_events_summary(session_id, thread.id, thread_mgr)

  cancelled = await _thread_cancelled(thread_mgr, session_id, thread.id)

  status = 'cancelled' if cancelled else _exit_status_label(outcome.exit_code)
  if verify_report is not None:
    report = events_summary or "(no verifier final report)"
    full_summary = f"**Verifier completion: thread `{thread.id}`**\n\n{report}"
  else:
    full_summary = f"**Worker finished: {description}**\n\n{events_summary}"

  suffix = ""
  if cancelled:
    suffix = "\n\n*Cancelled by user.*"
  elif outcome.quota_exhausted:
    suffix = "\n\n*Worker stopped: API quota exhausted.*"
  elif outcome.error:
    if verify_report is not None:
      suffix = f"\n\n*Verifier completion failed: {outcome.error}*"
    else:
      suffix = f"\n\n*Worker error: {outcome.error}*"
  elif outcome.exit_code != 0:
    suffix = f"\n\n*Worker exited with code {outcome.exit_code}.*"
  full_summary += suffix

  worker_event = spawner_events._thread_worker_event(thread, status, full_content=full_summary, content=None)
  await _persist_worker_summary_once(session_id, thread.id, worker_event, session_mgr, fallback=False)
  return events_summary, full_summary


async def _notify_completion(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    outcome: _WorkerRunOutcome,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    verify_report: str | None,
) -> None:
  """Broadcast worker_summary event to the session WebSocket and trigger master agent."""
  try:
    # Fetch session metadata exactly once for the whole notify chain; an
    # unscheduled session gives the Telegram notify nothing to look up.
    scheduled_task_name = None
    session_meta = await session_mgr.get_session(session_id)
    if session_meta is not None and session_meta.scheduled_task:
      session_meta.last_run_status = "success" if outcome.exit_code == 0 else "failed"
      session_meta.updated_at = datetime.now(timezone.utc)
      await session_mgr.save_metadata(session_meta)
      scheduled_task_name = session_meta.scheduled_task
    events_summary, full_summary = await _broadcast_completion(
        session_id,
        description,
        thread,
        outcome,
        thread_mgr,
        session_mgr,
        verify_report=verify_report)

    if scheduled_task_name:
      try:
        for task in get_scheduled_tasks():
          if task.name == scheduled_task_name and task.notify == 'telegram':
            await send_telegram(events_summary, cfg)
            break
      except Exception as tg_err:
        log.warning("telegram_notify_failed", session=session_id, error=str(tg_err))

    await review.maybe_spawn_reviewer(
        session_id, thread, outcome.exit_code, events_summary, full_summary, thread_mgr, session_mgr, cfg)
  except Exception as e:
    log.error("notify_completion_failed", thread_id=thread.id, error=str(e))
    try:
      status = _exit_status_label(outcome.exit_code)
      fallback = spawner_events._worker_locator_summary(thread.id, status, spawner_events._worker_summary_timestamp())
      fallback_event = spawner_events._thread_worker_event(
          thread, status, full_content=f"{fallback}\n\n*(summary unavailable: {e})*", content=fallback)
      await _persist_worker_summary_once(session_id, thread.id, fallback_event, session_mgr, fallback=True)
    except Exception as inner:
      log.error("fallback_notify_failed", thread_id=thread.id, error=str(inner), traceback=traceback.format_exc())
