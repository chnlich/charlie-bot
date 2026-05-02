"""Iterative /improve loop orchestrator."""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path
from typing import TYPE_CHECKING, Optional

import structlog
from pydantic import BaseModel

if TYPE_CHECKING:
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

from src.api.ext_usage import CcOpusProvider
from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.git import (
    git_create_worktree,
    git_merge_ff_only,
    git_pull_ff_only,
    git_push_branch,
    git_push_refspec,
    git_worktree_prune,
    git_worktree_remove,
)
from src.core.master_trigger import trigger_master
from src.core.models import SpawnRequest, ThreadStatus, parse_utc_datetime, utc_now
from src.core.tasks import create_logged_task
from src.core.timeouts import IMPROVE_QUOTA_POLL_INTERVAL

log = structlog.get_logger()

_QUOTA_POLL_INTERVAL = IMPROVE_QUOTA_POLL_INTERVAL

# ---------------------------------------------------------------------------
# State models
# ---------------------------------------------------------------------------


class ImproveState(BaseModel):
  loop_id: int
  goal: str
  max_iterations: int = 5
  status: str = "running"  # running | stopped | completed | failed
  work_branch: str
  base_branch: Optional[str] = None
  repo_path: str
  merge_back: bool = False
  backend: Optional[str] = None
  model: Optional[str] = None
  created_at: str
  iterations_completed: int = 0


class ImproveLoopAlreadyRunningError(RuntimeError):
  """Raised when a session already has an active improve loop."""

  def __init__(self, loop_id: Optional[int]) -> None:
    self.loop_id = loop_id
    if loop_id is None:
      super().__init__("An improve loop is already running for this session. Use /stop-improve first.")
      return
    super().__init__(f"Loop {loop_id} is already running for this session. Use /stop-improve first.")


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def _loops_dir(session_id: str, cfg: CharlieBotConfig) -> Path:
  return cfg.sessions_dir / session_id / "loops"


def _active_loop_path(session_id: str, cfg: CharlieBotConfig) -> Path:
  return _loops_dir(session_id, cfg) / "active.lock"


def _loop_state_path(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> Path:
  return _loops_dir(session_id, cfg) / str(loop_id) / "state.json"


def _next_loop_id_sync(loops_dir: Path) -> int:
  if not loops_dir.exists():
    return 1

  max_loop_id = 0
  for child in loops_dir.iterdir():
    if not child.is_dir():
      continue
    try:
      loop_id = int(child.name)
    except ValueError:
      continue
    max_loop_id = max(max_loop_id, loop_id)
  return max_loop_id + 1 if max_loop_id else 1


def _find_state_loop_ids_sync(loops_dir: Path) -> list[int]:
  if not loops_dir.exists():
    return []

  state_loop_ids: list[int] = []
  for child in loops_dir.iterdir():
    if not child.is_dir():
      continue
    try:
      loop_id = int(child.name)
    except ValueError:
      continue
    if (child / "state.json").exists():
      state_loop_ids.append(loop_id)
  return state_loop_ids


def _create_empty_file_exclusive(path: Path) -> None:
  with path.open("x"):
    pass


async def load_loop_state(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> Optional[ImproveState]:
  """Read the loop state file, returning None if it is missing."""
  path = _loop_state_path(session_id, loop_id, cfg)
  if not await asyncio.to_thread(path.exists):
    return None
  return ImproveState.model_validate_json(await asyncio.to_thread(path.read_text))


async def save_loop_state(session_id: str, state: ImproveState, cfg: CharlieBotConfig) -> None:
  """Write the loop state file."""
  path = _loop_state_path(session_id, state.loop_id, cfg)
  await asyncio.to_thread(path.parent.mkdir, parents=True, exist_ok=True)
  await asyncio.to_thread(path.write_text, state.model_dump_json(indent=2))


async def clear_active_loop_lock(session_id: str, cfg: CharlieBotConfig) -> None:
  """Remove the session's active loop lock file, if present."""
  active_path = _active_loop_path(session_id, cfg)
  if await asyncio.to_thread(active_path.exists):
    await asyncio.to_thread(active_path.unlink)


async def next_loop_id(session_id: str, cfg: CharlieBotConfig) -> int:
  """Return the next sequential loop id for a session."""
  loops_dir = _loops_dir(session_id, cfg)
  return await asyncio.to_thread(_next_loop_id_sync, loops_dir)


async def _reserve_loop_dir(session_id: str, cfg: CharlieBotConfig) -> tuple[int, Path]:
  """Create a unique per-loop directory for this session."""
  loops_dir = _loops_dir(session_id, cfg)
  await asyncio.to_thread(loops_dir.mkdir, parents=True, exist_ok=True)

  while True:
    loop_id = await next_loop_id(session_id, cfg)
    loop_dir = loops_dir / str(loop_id)
    try:
      await asyncio.to_thread(loop_dir.mkdir)
    except FileExistsError:
      continue
    return loop_id, loop_dir


async def find_running_loop(session_id: str, cfg: CharlieBotConfig) -> Optional[ImproveState]:
  """Return the running loop for a session, if any."""
  loops_dir = _loops_dir(session_id, cfg)
  state_loop_ids = await asyncio.to_thread(_find_state_loop_ids_sync, loops_dir)
  for loop_id in sorted(state_loop_ids):
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is not None and state.status == "running":
      return state
  return None


# ---------------------------------------------------------------------------
# Stop signal
# ---------------------------------------------------------------------------


async def stop_improve_loop(session_id: str, cfg: CharlieBotConfig) -> bool:
  """Set improve loop status to stopped. Returns True if there was an active loop."""
  state = await find_running_loop(session_id, cfg)
  if state is None:
    return False
  state.status = "stopped"
  await save_loop_state(session_id, state, cfg)
  return True


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _extract_iteration_summary(events: list[dict], iteration: int, status: str) -> str:
  """Extract a summary from worker events, preferring the result event."""
  for event in reversed(events):
    if event.get("type") == ET.RESULT and event.get("result"):
      return event["result"][:500]
    if event.get("type") == ET.ASSISTANT:
      text = extract_text_from_message(event.get("message"))
      if text:
        return text[:500]
  return f"Iteration {iteration} {status} (no summary available)."


def _build_summary_payload(payload_type: str, goal: str, summaries: list[str]) -> dict:
  """Build the shared JSON structure for completion/failure broadcasts."""
  return {
      "type": payload_type,
      "goal": goal,
      "iterations_completed": len(summaries),
      "summaries": summaries,
  }


async def reserve_loop_state(
    session_id: str,
    goal: str,
    iterations: int,
    work_branch: str,
    repo_path: str,
    cfg: CharlieBotConfig,
    *,
    base_branch: Optional[str] = None,
    merge_back: bool = False,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> ImproveState:
  """Reserve a unique loop id and persist running state before background work begins."""
  loops_dir = _loops_dir(session_id, cfg)
  await asyncio.to_thread(loops_dir.mkdir, parents=True, exist_ok=True)
  active_path = _active_loop_path(session_id, cfg)

  try:
    await asyncio.to_thread(_create_empty_file_exclusive, active_path)
  except FileExistsError as exc:
    running = await find_running_loop(session_id, cfg)
    raise ImproveLoopAlreadyRunningError(running.loop_id if running else None) from exc

  try:
    loop_id, _ = await _reserve_loop_dir(session_id, cfg)
    state = ImproveState(
        loop_id=loop_id,
        goal=goal,
        max_iterations=iterations,
        status="running",
        work_branch=work_branch,
        base_branch=base_branch,
        repo_path=str(Path(repo_path).resolve()),
        merge_back=merge_back,
        backend=resolved_backend or None,
        model=resolved_model or None,
        created_at=utc_now().isoformat(),
        iterations_completed=0,
    )
    await save_loop_state(session_id, state, cfg)
    await asyncio.to_thread(active_path.write_text, f"{loop_id}\n")
    return state
  except Exception:
    await clear_active_loop_lock(session_id, cfg)
    raise


# ---------------------------------------------------------------------------
# Quota-aware retry helpers
# ---------------------------------------------------------------------------


async def is_quota_failure(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> bool:
  """Check if a failed thread was due to quota exhaustion."""
  from src.core.ndjson import parse_ndjson_file

  thread = await thread_mgr.get_thread(session_id, thread_id)
  if not thread or thread.status != ThreadStatus.FAILED:
    return False
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)
  for ev in reversed(events):
    if ev.get('type') == 'rate_limit_event':
      rli = ev.get('rate_limit_info', {})
      if rli.get('overageStatus') == 'rejected':
        return True
    text = ev.get('result', '') or ''
    if 'quota exhausted' in text.lower():
      return True
  return False


async def _wait_for_quota_recovery(
    session_id: str,
    loop_id: int,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> bool:
  """Wait for API quota to recover. Returns True if recovered, False if stopped by user."""
  provider = CcOpusProvider()

  while True:
    # Check stop signal
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is None:
      raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
    if state.status == "stopped":
      return False

    usage = None
    try:
      usage = await provider.fetch()
    except Exception:
      log.warning("quota_recovery_fetch_failed", session=session_id, exc_info=True)

    if usage is not None:
      five_hour = usage.get("five_hour", {})
      utilization = five_hour.get("utilization", 0.0)
      if utilization < 0.8:
        log.info("quota_recovered", session=session_id, utilization=utilization)
        return True

      resets_at_str = five_hour.get("resets_at", "")
      if resets_at_str:
        try:
          resets_at = parse_utc_datetime(resets_at_str)
          now = utc_now()
          wait_seconds = (resets_at - now).total_seconds() + 60  # 60s buffer
          if wait_seconds > 0:
            msg = f"Quota exhausted, waiting until {resets_at_str} (+60s buffer)..."
            log.info("quota_waiting", session=session_id, resets_at=resets_at_str, wait_seconds=wait_seconds)
            await session_mgr.persist_and_broadcast(
                session_id, {
                    "type": ET.IMPROVE_ITERATION_COMPLETED,
                    "status": "quota_waiting",
                    "summary": msg
                })
            wait_seconds = min(wait_seconds, _QUOTA_POLL_INTERVAL)  # cap individual sleeps to check stop signal
            await asyncio.sleep(wait_seconds)
            continue
        except (ValueError, TypeError):
          log.warning("quota_recovery_parse_resets_at_failed", resets_at=resets_at_str)
      log.info("quota_polling_fallback", session=session_id)
    else:
      log.info("quota_polling_no_usage_data", session=session_id)

    # Fallback: either usage API unavailable, or high utilization with no usable resets_at.
    msg = "Quota exhausted, retrying in 10 minutes..."
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.IMPROVE_ITERATION_COMPLETED,
            "status": "quota_waiting",
            "summary": msg
        })
    await asyncio.sleep(_QUOTA_POLL_INTERVAL)


# ---------------------------------------------------------------------------
# Merge-back helper
# ---------------------------------------------------------------------------


async def _merge_back_to_base(
    repo_path: Path,
    base_branch: str,
    work_branch: str,
    cfg: CharlieBotConfig,
    session_id: str,
) -> tuple[bool, str]:
  """Merge work_branch into base_branch and push. Returns (success, error_message).

  Primary: create an ephemeral worktree on base_branch, pull, merge --ff-only, push.
  Fallback: if base_branch is already checked out, use refspec push.
  """
  tmp_path = Path(cfg.worktree_dir) / f"merge-{work_branch.replace('/', '-')}-{int(time.time())}"

  try:
    # Step 1: create ephemeral worktree on base_branch
    try:
      # Use a temp branch name for the worktree (we'll operate on base_branch directly)
      tmp_branch = f"_merge-tmp-{int(time.time())}"
      await git_create_worktree(repo_path, base_branch, tmp_branch, tmp_path)
    except RuntimeError as e:
      err_str = str(e)
      if 'already checked out' in err_str or 'is already checked out' in err_str:
        # Fallback: refspec push
        log.info("merge_back_fallback_refspec", session=session_id, reason=err_str)
        ok, push_err = await git_push_refspec(repo_path, work_branch, base_branch)
        if ok:
          return True, ""
        return False, f"refspec push failed: {push_err}"
      return False, f"worktree creation failed: {err_str}"

    # Step 2: pull latest base_branch
    ok, err = await git_pull_ff_only(tmp_path, base_branch)
    if not ok:
      log.warning("merge_back_pull_failed", session=session_id, error=err)
      # Non-fatal: the worktree was just created from base_branch, it should be up to date

    # Step 3: merge --ff-only work_branch
    ok, err = await git_merge_ff_only(tmp_path, work_branch)
    if not ok:
      return False, f"merge --ff-only failed: {err}"

    # Step 4: push the merged result (on tmp_branch) to remote base_branch
    ok, err = await git_push_refspec(tmp_path, "HEAD", base_branch)
    if not ok:
      return False, f"push failed: {err}"

    return True, ""
  finally:
    # Step 5: clean up ephemeral worktree
    if tmp_path.exists():
      await git_worktree_remove(str(repo_path), tmp_path, session_id)
      await git_worktree_prune(str(repo_path), session_id)


# ---------------------------------------------------------------------------
# Single-iteration helper
# ---------------------------------------------------------------------------


async def _run_single_iteration(
    i: int,
    iterations: int,
    goal: str,
    session_id: str,
    loop_id: int,
    repo_path: str,
    work_branch: str,
    wt_path: Path,
    loop_dir: Path,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    resolved_backend: str,
    resolved_model: str,
    previous_summaries: list[str],
    base_branch: Optional[str],
) -> Optional[str]:
  """Run a single iteration of the improve loop, with quota-failure retry.

  Returns the iteration summary on success, None if stopped by user.
  Appends the summary to previous_summaries on success.
  """
  from src.core.ndjson import parse_ndjson_file
  from src.core.spawner import spawn_worker

  while True:  # Retry loop for quota failures
    # Check if stopped
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is None:
      raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
    if state.status == "stopped":
      log.info("improve_loop_stopped", session=session_id, iteration=i)
      return None

    # Build iteration description
    desc_parts = [f"Iterative improvement — iteration {i}/{iterations}", f"Goal: {goal}"]
    description = "\n".join(desc_parts)

    # Get session metadata
    meta = await session_mgr.get_session(session_id)
    if not meta:
      log.error("improve_loop_session_missing", session=session_id)
      return None

    # Create thread and spawn worker
    thread = await thread_mgr.create_thread(meta, description, require_review=False)
    await spawn_worker(
        session_id,
        description,
        thread.id,
        cfg,
        session_mgr,
        thread_mgr,
        request=SpawnRequest(
            repo_path=repo_path,
            resolved_backend=resolved_backend,
            resolved_model=resolved_model,
            base_branch=base_branch,
            branch_name_override=work_branch,
            worktree_path_override=str(wt_path),
            skip_cleanup=True,
            skip_notify=True,
            loop_dir=str(loop_dir),
            iteration_number=i,
            is_continuation=(i > 1),
        ),
    )

    # Check for quota failure
    if await is_quota_failure(session_id, thread.id, thread_mgr):
      log.warning("improve_iteration_quota_failure", session=session_id, iteration=i)
      recovered = await _wait_for_quota_recovery(session_id, loop_id, cfg, session_mgr)
      if not recovered:
        log.info("improve_loop_stopped_during_quota_wait", session=session_id, iteration=i)
        return None
      # Retry the same iteration — do NOT increment i or append summary
      continue

    # Successful (or non-quota failure) — extract summary and proceed
    thread_meta = await thread_mgr.get_thread(session_id, thread.id)
    status = thread_meta.status.value if thread_meta else "unknown"
    events_path = await thread_mgr.get_events_log_path(session_id, thread.id)
    events = await asyncio.to_thread(parse_ndjson_file, events_path)
    summary = _extract_iteration_summary(events, i, status)
    previous_summaries.append(summary)

    # Write fallback report if the worker didn't write one
    report_path = loop_dir / f'iter_{i:04d}.md'
    if not await asyncio.to_thread(report_path.exists):
      await asyncio.to_thread(report_path.write_text, summary)

    log.info("improve_iteration_completed", session=session_id, iteration=i, status=status)

    # Broadcast progress event
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.IMPROVE_ITERATION_COMPLETED,
            "iteration": i,
            "total_iterations": iterations,
            "status": status,
            "summary": summary[:200],
        })

    # Trigger master after each iteration so it sees progress
    iter_trigger_payload = {
        'type': ET.IMPROVE_ITERATION_COMPLETED,
        'goal': goal,
        'iteration': i,
        'total_iterations': iterations,
        'status': status,
        'summary': summary[:200],
        'work_branch': work_branch,
        'instructions': "Report iteration progress with commit identifier and link. Briefly assess and recommend.",
    }
    create_logged_task(
        trigger_master(session_id, json.dumps(iter_trigger_payload, indent=2), cfg, session_mgr),
        name=f"improve-iter-trigger-{session_id[:8]}-{i}",
    )

    return summary


# ---------------------------------------------------------------------------
# Server-side improve loop
# ---------------------------------------------------------------------------


async def run_improve_loop(
    session_id: str,
    repo_path: str,
    iterations: int,
    goal: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    base_branch: Optional[str] = None,
    work_branch: Optional[str] = None,
    merge_back: bool = False,
    resolved_backend: str = "",
    resolved_model: str = "",
    loop_id: Optional[int] = None,
) -> None:
  """Run the iterative improvement loop as a server-side async task.

  All iterations commit to a single work_branch in a single shared worktree.
  When done (or stopped), optionally merges work_branch back to base_branch,
  then triggers the master CC with the combined summary.
  """
  previous_summaries: list[str] = []

  work_branch = work_branch or f'improve/{int(time.time())}'
  resolved_repo = Path(repo_path).resolve()
  if loop_id is None:
    state = await reserve_loop_state(
        session_id,
        goal,
        iterations,
        work_branch,
        str(resolved_repo),
        cfg,
        base_branch=base_branch,
        merge_back=merge_back,
        resolved_backend=resolved_backend,
        resolved_model=resolved_model,
    )
    loop_id = state.loop_id
  else:
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is None:
      raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
    resolved_repo = Path(state.repo_path)
    work_branch = state.work_branch
    base_branch = state.base_branch
    merge_back = state.merge_back
    resolved_backend = state.backend or resolved_backend
    resolved_model = state.model or resolved_model
  loop_dir = cfg.sessions_dir / session_id / 'loops' / str(loop_id)
  await asyncio.to_thread(loop_dir.mkdir, parents=True, exist_ok=True)

  log.info(
      "improve_loop_backend_pinned",
      session=session_id,
      loop_id=loop_id,
      backend=resolved_backend,
      model=resolved_model,
      work_branch=work_branch,
      merge_back=merge_back,
  )

  # Create the single worktree for all iterations
  wt_path = Path(cfg.worktree_dir) / work_branch.replace('/', '-')
  Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)
  try:
    await git_create_worktree(resolved_repo, base_branch, work_branch, wt_path)
  except Exception as e:
    state.status = 'failed'
    await save_loop_state(session_id, state, cfg)
    log.error("improve_loop_worktree_failed", session=session_id, error=str(e))
    failure_payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, [])
    failure_payload['error'] = f"Failed to create worktree: {e}"
    await session_mgr.persist_and_broadcast(session_id, failure_payload)
    await trigger_master(session_id, json.dumps(failure_payload, indent=2), cfg, session_mgr)
    return

  try:
    for i in range(1, iterations + 1):
      summary = await _run_single_iteration(
          i,
          iterations,
          goal,
          session_id,
          loop_id,
          repo_path,
          work_branch,
          wt_path,
          loop_dir,
          cfg,
          session_mgr,
          thread_mgr,
          resolved_backend,
          resolved_model,
          previous_summaries,
          base_branch,
      )
      if summary is None:
        break  # Stopped by user or session missing
      state = await load_loop_state(session_id, loop_id, cfg)
      if state is None:
        raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
      state.iterations_completed += 1
      await save_loop_state(session_id, state, cfg)

    # Check if we exited because the user stopped the loop
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is None:
      raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
    stopped_by_user = state.status == 'stopped'

    if stopped_by_user:
      payload = _build_summary_payload(ET.IMPROVE_STOPPED, goal, previous_summaries)
      payload['reason'] = 'Stopped by user'
    else:
      payload = _build_summary_payload(ET.IMPROVE_COMPLETED, goal, previous_summaries)

    state.status = 'stopped' if stopped_by_user else 'completed'
    await save_loop_state(session_id, state, cfg)

    payload['work_branch'] = work_branch
    payload['base_branch'] = base_branch

    # Post-loop: merge back or push work branch
    if merge_back and not stopped_by_user and previous_summaries:
      success, error = await _merge_back_to_base(resolved_repo, base_branch, work_branch, cfg, session_id)
      if success:
        payload['merge_result'] = {'merged': True, 'base_branch': base_branch}
      else:
        payload['merge_result'] = {'merged': False, 'error': error}
    else:
      # Best-effort push work_branch to remote
      ok, push_err = await git_push_branch(resolved_repo, work_branch)
      if not ok:
        log.warning("improve_loop_push_failed", session=session_id, error=push_err)

    await session_mgr.persist_and_broadcast(session_id, payload)
    final_payload = {
        **payload,
        'instructions': "Report final state and summarize what changed across iterations.",
    }
    await trigger_master(session_id, json.dumps(final_payload, indent=2), cfg, session_mgr)

  except asyncio.CancelledError:
    log.warning("improve_loop_cancelled", session=session_id)
    await session_mgr.persist_and_broadcast(session_id, {"type": ET.IMPROVE_CANCELLED, "goal": goal})
    raise
  except Exception:
    log.error("improve_loop_failed", session=session_id, exc_info=True)
    state = await load_loop_state(session_id, loop_id, cfg)
    if state:
      state.status = 'failed'
      await save_loop_state(session_id, state, cfg)
  finally:
    await clear_active_loop_lock(session_id, cfg)
    # Always clean up the shared worktree
    if wt_path.exists():
      await git_worktree_remove(str(resolved_repo), wt_path, session_id)
      await git_worktree_prune(str(resolved_repo), session_id)
