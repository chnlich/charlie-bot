"""Iterative /improve loop orchestrator."""

from __future__ import annotations

import asyncio
import json
import re
import time
from pathlib import Path
from typing import TYPE_CHECKING, Optional

import structlog
from pydantic import BaseModel

if TYPE_CHECKING:
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.git import (
    _git_rev_parse,
    _git_stdout,
    git_create_worktree,
    git_current_branch,
    git_push_branch,
    git_push_refspec,
    git_worktree_dir_name,
    git_worktree_prune,
    git_worktree_remove,
)
from src.core.master_trigger import trigger_master
from src.core.models import SpawnRequest, TaskType, ThreadStatus, utc_now
from src.core.tasks import create_logged_task
from src.core.timeouts import SUBPROCESS_GIT_READ_TIMEOUT_ASYNC

log = structlog.get_logger()

_QUOTA_BLOCKER_TEXT_PATTERNS = (
    "quota exhausted",
    "quota exceeded",
    "insufficient quota",
    "over quota",
    "rate limit exceeded",
    "rate limit reached",
    "rate limit rejected",
    "rate-limit exceeded",
    "rate-limit reached",
    "rate-limit rejected",
    "rate_limited",
    "rate_limit_error",
    "rate limited",
    "too many requests",
    "resource_exhausted",
    "429",
    "out of token",
    "out-of-token",
    "out of tokens",
    "insufficient tokens",
    "tokens exhausted",
)

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


class _ImproveLoopBlockedError(RuntimeError):
  """Raised when an improve-loop worker hit a provider quota/token/rate-limit blocker."""

  def __init__(self, iteration: int, reason: str, summary: str) -> None:
    self.iteration = iteration
    self.reason = reason
    self.summary = summary
    super().__init__(f"Improve loop blocked on iteration {iteration}: {reason}")


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def _loops_dir(session_id: str, cfg: CharlieBotConfig) -> Path:
  return cfg.sessions_dir / session_id / "loops"


def _active_loop_path(session_id: str, cfg: CharlieBotConfig) -> Path:
  return _loops_dir(session_id, cfg) / "active.lock"


def _loop_state_path(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> Path:
  return _loops_dir(session_id, cfg) / str(loop_id) / "state.json"


def _goal_file_path(loop_dir: Path) -> Path:
  return loop_dir / "goal.md"


def _plan_file_path(loop_dir: Path) -> Path:
  return loop_dir / "plan.md"


def loop_goal_path(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> Path:
  """Path to the live goal file for a loop (the editable per-iteration goal)."""
  return _goal_file_path(_loops_dir(session_id, cfg) / str(loop_id))


def loop_plan_path(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> Path:
  """Path to the optional live plan file for a loop."""
  return _plan_file_path(_loops_dir(session_id, cfg) / str(loop_id))


async def read_loop_goal(loop_dir: Path) -> str:
  """Read the live goal for a loop, failing loudly if goal.md is missing.

  The goal file is re-read at the start of every iteration so the user can steer
  a running loop by editing it. A missing file mid-loop is a hard failure — there
  is deliberately no fallback to the state.json startup snapshot.
  """
  goal_path = _goal_file_path(loop_dir)
  if not await asyncio.to_thread(goal_path.exists):
    raise RuntimeError(f"improve loop goal file missing: {goal_path}")
  return await asyncio.to_thread(goal_path.read_text)


async def read_loop_plan(loop_dir: Path) -> Optional[str]:
  """Read the optional live plan for a loop, returning None when absent."""
  plan_path = _plan_file_path(loop_dir)
  if not await asyncio.to_thread(plan_path.exists):
    return None
  return await asyncio.to_thread(plan_path.read_text)


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


async def require_loop_state(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> ImproveState:
  """Read the loop state file, raising RuntimeError if it is missing."""
  state = await load_loop_state(session_id, loop_id, cfg)
  if state is None:
    raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
  return state


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


def _commits_section_first_none(text: str) -> bool:
  """Return True when the report's `### Commits` section lists `none` first."""
  lines = text.splitlines()
  for idx, line in enumerate(lines):
    if line.strip().startswith("### Commits"):
      for entry in lines[idx + 1:]:
        stripped = entry.strip()
        if not stripped:
          continue
        if stripped.startswith("- "):
          return re.match(r"^none\b", stripped[2:].strip()) is not None
        break
      break
  return False


async def _iter_report_validity(
    report_path: Path,
    iteration: int,
    commits_added: int,
) -> tuple[bool, Optional[str]]:
  """Mechanically judge an iteration's report validity.

  Valid ⇔ the report exists, contains a `## Iter <n>` heading (em dash or
  hyphen tolerated), and either commits were added or the report carries an
  explicit zero-progress verdict (`### Commits` with `- none — <verdict>`).
  Returns (valid, invalid_reason), invalid_reason in
  {missing_report, malformed_report, no_commit_no_verdict} or None when valid.
  """
  if not await asyncio.to_thread(report_path.exists):
    return False, "missing_report"
  text = await asyncio.to_thread(report_path.read_text)
  if not re.search(rf"^## Iter {iteration}\b", text, flags=re.MULTILINE):
    return False, "malformed_report"
  if commits_added > 0 or _commits_section_first_none(text):
    return True, None
  return False, "no_commit_no_verdict"


def _invalid_iteration_summary(
    iteration: int,
    reason: str,
    commits_added: int,
    tip_before: str,
    tip_after: str,
    report_path: Path,
) -> str:
  """Build the fixed single-line placeholder for an invalid iteration."""
  return (
      f"Iteration {iteration} INVALID ({reason}); commits_added={commits_added}; "
      f"tip {tip_before}..{tip_after}; report: {report_path}")


async def _worktree_commit_delta(wt_path: Path, tip_before: str) -> tuple[str, int, str]:
  """Compute (tip_after, commits_added, diffstat) for the iteration's commits."""
  tip_after = await _git_rev_parse(wt_path, "HEAD") or ""
  commits_added = 0
  diffstat = ""
  if tip_before and tip_after:
    ok, out, _ = await _git_stdout(
        wt_path,
        "rev-list",
        "--count",
        f"{tip_before}..{tip_after}",
        timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
        timeout_label="git rev-list --count",
    )
    if ok and out.isdigit():
      commits_added = int(out)
    if commits_added > 0:
      ok, ds, _ = await _git_stdout(
          wt_path,
          "diff",
          "--shortstat",
          f"{tip_before}..{tip_after}",
          timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
          timeout_label="git diff --shortstat",
      )
      if ok:
        diffstat = ds
  return tip_after, commits_added, diffstat


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
    plan: Optional[str] = None,
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
    loop_id, loop_dir = await _reserve_loop_dir(session_id, cfg)
    # Write the live goal exactly once at reservation. Iterations re-read this
    # file; state.json's goal stays as the startup snapshot for display/summary.
    await asyncio.to_thread(_goal_file_path(loop_dir).write_text, goal)
    if plan is not None:
      await asyncio.to_thread(_plan_file_path(loop_dir).write_text, plan)
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
# Quota/token/rate-limit blocker helpers
# ---------------------------------------------------------------------------


def _event_text(event: dict) -> str:
  parts: list[str] = []
  for key in ("message", "content", "result", "error", "api_error_status"):
    value = event.get(key)
    if value is None:
      continue
    if key == "message" and isinstance(value, dict):
      text = extract_text_from_message(value)
      if text:
        parts.append(text)
      continue
    if isinstance(value, (dict, list)):
      parts.append(json.dumps(value, default=str))
    else:
      parts.append(str(value))
  return "\n".join(parts)


def _quota_blocker_reason(events: list[dict]) -> Optional[str]:
  for ev in reversed(events):
    event_type = ev.get('type')
    if event_type == ET.RATE_LIMIT_EVENT:
      rli = ev.get('rate_limit_info', {})
      status = str(rli.get('status', '')).lower()
      overage_status = str(rli.get('overageStatus', '')).lower()
      if status == 'rejected' or overage_status == 'rejected':
        rate_type = rli.get('rateLimitType') or 'rate limit'
        return f"rate-limit rejection ({rate_type})"

    if event_type not in (ET.ERROR, ET.ASSISTANT_ERROR, ET.RESULT):
      continue
    if (event_type == ET.RESULT and ev.get('is_error') is not True and ev.get('api_error_status') is None and
        'error' not in str(ev.get('subtype', '')).lower()):
      continue

    text = _event_text(ev).lower()
    for pattern in _QUOTA_BLOCKER_TEXT_PATTERNS:
      if pattern in text:
        return f"provider quota/token/rate-limit rejection ({pattern})"
  return None


async def is_quota_failure(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> bool:
  """Check if a failed thread was due to provider quota/token/rate-limit rejection."""
  from src.core.ndjson import parse_ndjson_file

  thread = await thread_mgr.get_thread(session_id, thread_id)
  if not thread or thread.status != ThreadStatus.FAILED:
    return False
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)
  return _quota_blocker_reason(events) is not None


# ---------------------------------------------------------------------------
# Single-iteration helper
# ---------------------------------------------------------------------------


async def _run_single_iteration(
    i: int,
    iterations: int,
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
  """Run a single iteration of the improve loop.

  Returns the iteration summary on success, None if stopped by user.
  Appends the summary to previous_summaries on success.
  """
  from src.core.ndjson import parse_ndjson_file
  from src.core.spawner import spawn_worker

  # Check if stopped
  state = await require_loop_state(session_id, loop_id, cfg)
  if state.status == "stopped":
    log.info("improve_loop_stopped", session=session_id, iteration=i)
    return None

  # Re-read the live goal so mid-loop edits to goal.md steer this iteration.
  # A missing goal.md fails the loop loudly (read_loop_goal raises) — there is no
  # fallback to the state.json startup snapshot.
  goal = await read_loop_goal(loop_dir)
  plan = await read_loop_plan(loop_dir)

  # Build iteration description
  desc_parts = [f"Iterative improvement — iteration {i}/{iterations}", f"Goal: {goal}"]
  if plan is not None:
    desc_parts.append(f"Plan:\n{plan}")
  if previous_summaries:
    desc_parts.append("Previous iteration summaries:\n" + "\n\n".join(previous_summaries))
  description = "\n".join(desc_parts)

  # Get session metadata
  meta = await session_mgr.get_session(session_id)
  if not meta:
    log.error("improve_loop_session_missing", session=session_id)
    raise ValueError(f"session '{session_id}' not found")

  # Create thread and spawn worker
  tip_before = await _git_rev_parse(wt_path, "HEAD") or ""
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
          task_type=TaskType.IMPLEMENT,
      ),
  )

  # Successful (or non-quota failure) — mechanically judge validity from the
  # report file and git state, then source the summary from the report head.
  thread_meta = await thread_mgr.get_thread(session_id, thread.id)
  status = thread_meta.status.value if thread_meta else "unknown"
  events_path = await thread_mgr.get_events_log_path(session_id, thread.id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)
  if thread_meta and thread_meta.status == ThreadStatus.FAILED:
    blocker_reason = _quota_blocker_reason(events)
    if blocker_reason:
      summary = _extract_iteration_summary(events, i, status)
      log.warning("improve_iteration_blocked", session=session_id, iteration=i, reason=blocker_reason)
      raise _ImproveLoopBlockedError(i, blocker_reason, summary)

  report_path = loop_dir / f'iter_{i:04d}.md'
  tip_after, commits_added, diffstat = await _worktree_commit_delta(wt_path, tip_before)
  report_valid, invalid_reason = await _iter_report_validity(report_path, i, commits_added)

  if report_valid:
    summary = (await asyncio.to_thread(report_path.read_text))[:500]
  else:
    summary = _invalid_iteration_summary(i, invalid_reason, commits_added, tip_before, tip_after, report_path)
  previous_summaries.append(summary)

  # Write a fallback report if the worker wrote none. Validity was already
  # decided as missing_report before this write, so the fallback never flips the
  # verdict. The event-extracted summary is used only here, never as loop context.
  if not await asyncio.to_thread(report_path.exists):
    fallback_body = _extract_iteration_summary(events, i, status)
    await asyncio.to_thread(
        report_path.write_text, "<!-- runner fallback: worker wrote no report -->\n" + fallback_body)

  log.info(
      "improve_iteration_completed",
      session=session_id,
      iteration=i,
      status=status,
      report_valid=report_valid,
      invalid_reason=invalid_reason,
      commits_added=commits_added,
  )

  # Broadcast progress event, sourced from the report head/placeholder like the
  # master wake payload.
  await session_mgr.deliver_to_successor(
      session_id, {
          "type": ET.IMPROVE_ITERATION_COMPLETED,
          "iteration": i,
          "total_iterations": iterations,
          "status": status,
          "summary": summary[:200],
          "report_path": str(report_path),
          "report_valid": report_valid,
          "invalid_reason": invalid_reason,
          "tip": tip_after,
          "commits_added": commits_added,
          "diffstat": diffstat,
      })

  # Trigger master after each iteration so it sees progress
  iter_trigger_payload = {
      'type':
          ET.IMPROVE_ITERATION_COMPLETED,
      'goal':
          goal,
      'iteration':
          i,
      'total_iterations':
          iterations,
      'status':
          status,
      'summary':
          summary[:200],
      'work_branch':
          work_branch,
      'report_path':
          str(report_path),
      'report_valid':
          report_valid,
      'invalid_reason':
          invalid_reason,
      'tip':
          tip_after,
      'commits_added':
          commits_added,
      'diffstat':
          diffstat,
      'instructions':
          (
              "Audit this iteration before reporting: read the report file at report_path and check its "
              "verdict and KPI readings against the live goal. Report iteration progress with commit "
              "identifier and link. If drift signals are present (report_valid is false, or the report "
              "omits a goal-declared KPI or verdict), apply a bounded live-goal edit per the "
              "improve-goal skill and quote the edit in full in your chat report."),
  }
  create_logged_task(
      trigger_master(session_id, json.dumps(iter_trigger_payload, indent=2), cfg, session_mgr),
      name=f"improve-iter-trigger-{session_id[:8]}-{i}",
  )

  return summary


# ---------------------------------------------------------------------------
# Server-side improve loop
# ---------------------------------------------------------------------------


async def _land_work_branch_after_loop(
    resolved_repo: Path,
    work_branch: str,
    base_branch: Optional[str],
    merge_back: bool,
    stopped_by_user: bool,
    previous_summaries: list[str],
    session_id: str,
) -> Optional[dict]:
  """Decide how to land the work branch after the iteration loop finishes.

  Fast-forwards work_branch onto base_branch when merge_back is set and the loop
  produced summaries without being stopped. On FF push failure, falls back to
  pushing the bare work branch so the master agent can land it manually (PR,
  manual rebase, etc.). Returns the merge_result dict to attach to the payload,
  or None when no landing decision was recorded (best-effort branch push only).
  """
  # Worktree was created from origin/<base>, so the FF push only fails if origin/<base>
  # advanced during the loop. On failure, hand the work branch back to the master agent
  # via the trigger payload — no rebase-retry, no fallback subagent — so it can decide
  # how to land (e.g. open a PR, manual rebase).
  if merge_back and not stopped_by_user and previous_summaries:
    ok, push_err = await git_push_refspec(resolved_repo, work_branch, base_branch)
    if ok:
      return {'merged': True, 'base_branch': base_branch}
    log.warning("improve_loop_landing_ff_push_failed", session=session_id, error=push_err)
    # Keep the work branch on origin so the master agent can act on it.
    ok_push, push_branch_err = await git_push_branch(resolved_repo, work_branch)
    if not ok_push:
      log.warning("improve_loop_work_branch_push_failed", session=session_id, error=push_branch_err)
    return {
        'merged': False,
        'error': push_err,
        'work_branch': work_branch,
        'base_branch': base_branch,
    }
  # Best-effort push work_branch to remote
  ok, push_err = await git_push_branch(resolved_repo, work_branch)
  if not ok:
    log.warning("improve_loop_push_failed", session=session_id, error=push_err)
  return None


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
    plan: Optional[str] = None,
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
        plan=plan,
        base_branch=base_branch,
        merge_back=merge_back,
        resolved_backend=resolved_backend,
        resolved_model=resolved_model,
    )
    loop_id = state.loop_id
  else:
    state = await require_loop_state(session_id, loop_id, cfg)
    resolved_repo = Path(state.repo_path)
    work_branch = state.work_branch
    base_branch = state.base_branch
    merge_back = state.merge_back
    resolved_backend = state.backend or resolved_backend
    resolved_model = state.model or resolved_model
  loop_dir = cfg.sessions_dir / session_id / 'loops' / str(loop_id)
  await asyncio.to_thread(loop_dir.mkdir, parents=True, exist_ok=True)

  # Read the live goal written at reservation. The resume path (loop_id given)
  # reads it too so an edit made before a restart takes effect. state.json's goal
  # field stays the startup snapshot used only for display/summary payloads.
  goal = await read_loop_goal(loop_dir)

  log.info(
      "improve_loop_backend_pinned",
      session=session_id,
      loop_id=loop_id,
      backend=resolved_backend,
      model=resolved_model,
      work_branch=work_branch,
      merge_back=merge_back,
  )

  # Create the single worktree for all iterations.
  # resolve_base_branch (inside git_create_worktree) fetches the remote tip and
  # hard-fails on ambiguity (stale/mismatched local base), so iterations always
  # start from a fresh, unambiguous base. Persist the canonical bare branch name
  # so merge-back pushes and reviewer instructions never see an origin/ form.
  wt_path = Path(cfg.worktree_dir) / work_branch.replace('/', '-')
  Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)
  try:
    resolution = await git_create_worktree(
        resolved_repo, base_branch or await git_current_branch(resolved_repo), work_branch, wt_path)
    state.base_branch = resolution.canonical
    await save_loop_state(session_id, state, cfg)
    base_branch = resolution.canonical
  except Exception as e:
    state.status = 'failed'
    await save_loop_state(session_id, state, cfg)
    await clear_active_loop_lock(session_id, cfg)
    log.error("improve_loop_worktree_failed", session=session_id, error=str(e))
    failure_payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, [])
    failure_payload['error'] = f"Failed to create worktree: {e}"
    await session_mgr.deliver_to_successor(session_id, failure_payload)
    await trigger_master(session_id, json.dumps(failure_payload, indent=2), cfg, session_mgr)
    return

  blocked_error: Optional[_ImproveLoopBlockedError] = None
  failure_iteration = 0

  try:
    for i in range(1, iterations + 1):
      failure_iteration = i
      try:
        summary = await _run_single_iteration(
            i,
            iterations,
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
      except _ImproveLoopBlockedError as exc:
        blocked_error = exc
        break
      if summary is None:
        break  # Stopped by user
      state = await require_loop_state(session_id, loop_id, cfg)
      state.iterations_completed += 1
      await save_loop_state(session_id, state, cfg)

    # Check if we exited because the user stopped the loop
    state = await require_loop_state(session_id, loop_id, cfg)
    stopped_by_user = state.status == 'stopped'

    if blocked_error:
      payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, previous_summaries)
      payload['blocked_iteration'] = blocked_error.iteration
      payload['reason'] = blocked_error.reason
      payload['blocked_summary'] = blocked_error.summary[:500]
      payload['summary'] = (
          f"Improve loop blocked on iteration {blocked_error.iteration}: {blocked_error.reason}. "
          "No further iterations were spawned; decide whether to wait, switch backend, or relaunch.")
    elif stopped_by_user:
      payload = _build_summary_payload(ET.IMPROVE_STOPPED, goal, previous_summaries)
      payload['reason'] = 'Stopped by user'
    else:
      payload = _build_summary_payload(ET.IMPROVE_COMPLETED, goal, previous_summaries)

    if blocked_error:
      state.status = 'failed'
    else:
      state.status = 'stopped' if stopped_by_user else 'completed'
    await save_loop_state(session_id, state, cfg)

    payload['work_branch'] = work_branch
    payload['base_branch'] = base_branch
    if blocked_error:
      payload['worktree_path'] = str(wt_path)

    if not blocked_error:
      merge_result = await _land_work_branch_after_loop(
          resolved_repo,
          work_branch,
          base_branch,
          merge_back,
          stopped_by_user,
          previous_summaries,
          session_id,
      )
      if merge_result is not None:
        payload['merge_result'] = merge_result

    await session_mgr.deliver_to_successor(session_id, payload)
    if blocked_error:
      instructions = (
          "Report that the improve loop is blocked by provider quota/token/rate-limit rejection. "
          "Do not spawn another iteration worker automatically; ask the user to decide whether to wait, "
          "switch backend, or relaunch.")
    else:
      instructions = "Report final state and summarize what changed across iterations."
    if not blocked_error and payload.get('merge_result', {}).get('merged') is False:
      instructions += (
          " The fast-forward landing onto base_branch failed (origin/base_branch advanced"
          " during the loop). The work branch has been pushed to origin — decide how to land it"
          " (e.g. open a PR, request a manual rebase). Do NOT retry a fast-forward push.")
    final_payload = {
        **payload,
        'instructions': instructions,
    }
    await trigger_master(session_id, json.dumps(final_payload, indent=2), cfg, session_mgr)

  except asyncio.CancelledError:
    log.warning("improve_loop_cancelled", session=session_id)
    await session_mgr.deliver_to_successor(session_id, {"type": ET.IMPROVE_CANCELLED, "goal": goal})
    raise
  except Exception as exc:
    log.error("improve_loop_failed", session=session_id, exc_info=True)
    state = await load_loop_state(session_id, loop_id, cfg)
    if state:
      state.status = 'failed'
      await save_loop_state(session_id, state, cfg)
      failure_payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, previous_summaries)
      failure_payload['failed_iteration'] = failure_iteration
      failure_payload['error'] = f"Improve loop failed: {exc}"
      failure_payload['work_branch'] = work_branch
      failure_payload['base_branch'] = base_branch
      instructions = (
          "Report the improve loop failure to the user. Do not spawn another "
          "iteration worker or relaunch the loop automatically; ask the user to decide next steps.")
      try:
        await session_mgr.deliver_to_successor(session_id, failure_payload)
        final_payload = {**failure_payload, 'instructions': instructions}
        await trigger_master(session_id, json.dumps(final_payload, indent=2), cfg, session_mgr)
      except Exception as notify_error:
        log.error(
            "improve_loop_failure_notify_failed",
            session=session_id,
            error=str(notify_error),
            exc_info=True,
        )
  finally:
    await clear_active_loop_lock(session_id, cfg)
    # Keep the shared worktree when the loop failed so its in-progress state survives for
    # debugging; startup recovery marks the iteration thread failed and the quarantine
    # sweep reclaims it later. A hard process crash skips this finally and likewise keeps it.
    final_state = await load_loop_state(session_id, loop_id, cfg)
    loop_failed = final_state is not None and final_state.status == 'failed'
    if wt_path.exists() and not loop_failed:
      try:
        removed = await git_worktree_remove(
            str(resolved_repo),
            wt_path,
            session_id,
            allowed_parent=Path(cfg.worktree_dir),
            expected_residue_name=git_worktree_dir_name(work_branch),
        )
      except Exception as e:
        log.error("improve_loop_cleanup_failed", session=session_id, worktree=str(wt_path), error=str(e), exc_info=True)
        await session_mgr.deliver_to_successor(
            session_id, {
                "type": ET.ERROR,
                "content": f"Improve-loop worktree cleanup failed for {wt_path}: {e}"
            })
      else:
        if removed:
          await git_worktree_prune(str(resolved_repo), session_id)
        else:
          log.error("improve_loop_cleanup_remove_failed", session=session_id, worktree=str(wt_path))
          await session_mgr.deliver_to_successor(
              session_id, {
                  "type": ET.ERROR,
                  "content": f"Improve-loop worktree cleanup failed for {wt_path}: git worktree remove reported failure"
              })
