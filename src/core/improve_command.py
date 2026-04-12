"""Iterative /improve loop orchestrator."""

import asyncio
import json
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import structlog
from pydantic import BaseModel

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
from src.core.models import ThreadStatus
from src.core.tasks import create_logged_task
from src.core.timeouts import IMPROVE_QUOTA_POLL_INTERVAL

log = structlog.get_logger()

_QUOTA_POLL_INTERVAL = IMPROVE_QUOTA_POLL_INTERVAL

# ---------------------------------------------------------------------------
# State models
# ---------------------------------------------------------------------------


class ImproveState(BaseModel):
  goal: str
  max_iterations: int = 5
  status: str = "running"  # running, stopped, completed


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def _state_path(session_id: str, cfg: CharlieBotConfig) -> Path:
  return cfg.sessions_dir / session_id / "improve_state.json"


def load_improve_state(session_id: str, cfg: CharlieBotConfig) -> Optional[ImproveState]:
  """Read the improve state file, returning None if missing or corrupted."""
  path = _state_path(session_id, cfg)
  if not path.exists():
    return None
  try:
    return ImproveState.model_validate_json(path.read_text())
  except Exception:
    log.warning("improve_state_corrupted", session=session_id, path=str(path), exc_info=True)
    return None


def save_improve_state(session_id: str, state: ImproveState, cfg: CharlieBotConfig) -> None:
  """Write the improve state file."""
  path = _state_path(session_id, cfg)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(state.model_dump_json(indent=2))


# ---------------------------------------------------------------------------
# Stop signal
# ---------------------------------------------------------------------------


async def stop_improve_loop(session_id: str, cfg: CharlieBotConfig) -> bool:
  """Set improve loop status to stopped. Returns True if there was an active loop."""
  state = load_improve_state(session_id, cfg)
  if state is None or state.status != "running":
    return False
  state.status = "stopped"
  save_improve_state(session_id, state, cfg)
  return True


# ---------------------------------------------------------------------------
# Master prompt building
# ---------------------------------------------------------------------------


def build_improve_master_prompt(
    session_id: str,
    goal: str,
    max_iterations: int,
    cfg: CharlieBotConfig,
) -> str:
  """Build a prompt for master CC to orchestrate the improve loop."""
  state_path = cfg.sessions_dir / session_id / "improve_state.json"
  return f"""## Iterative Improvement Loop

You are starting an iterative improvement loop.

**Goal:** {goal}
**Max iterations:** {max_iterations}

### Instructions
1. Determine the target repository and base branch from the session context.
2. Confirm with the user: show the repo path, restate the final goal, and capture only the constraints or success criteria that must be preserved. Keep the prompt final-goal driven: the user should provide the destination and constraints, not a roadmap, per-iteration plan, or implementation recipe.
3. Do NOT propose per-iteration methods, milestones, or a detailed plan. Workers are fully autonomous and choose their own approach for each iteration.
4. If the user requests a specific backend, it must be a configured `backend_options.id` from `~/.charliebot/config.yaml`; include `--backend <id>` in the command only when explicitly requested.
5. After approval, choose a descriptive `--work-branch` based on the goal (e.g. `improve/fix-precision`, `improve/optimize-step-time`). If the user mentions a Linear ticket, include it (e.g. `ALG-865/chaoli/20260326/fix-precision`). Then run:
   ```
   python -m src.cli.improve --session {session_id} --repo <repo> --base-branch <base-branch> --iterations {max_iterations} --goal '<the goal above>' --work-branch '<your chosen branch>'
   ```
   All iterations commit to the single work branch in a shared worktree.
   Add `--merge-back` if the user wants the work branch merged back into base_branch after all iterations complete.
6. The CLI returns immediately after launching the server-side loop. You will receive a summary message when all iterations complete. Do NOT wait or poll — just let the user know the loop has started.

The improve state file is at: {state_path}"""


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


# ---------------------------------------------------------------------------
# Quota-aware retry helpers
# ---------------------------------------------------------------------------


async def is_quota_failure(session_id: str, thread_id: str, thread_mgr: "ThreadManager") -> bool:
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
    cfg: CharlieBotConfig,
    session_mgr: "SessionManager",
) -> bool:
  """Wait for API quota to recover. Returns True if recovered, False if stopped by user."""
  provider = CcOpusProvider()

  while True:
    # Check stop signal
    state = load_improve_state(session_id, cfg)
    if state and state.status == "stopped":
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
          resets_at = datetime.fromisoformat(resets_at_str)
          now = datetime.now(timezone.utc)
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

      # High utilization but no parseable resets_at — fall through to polling
      msg = "Quota exhausted, retrying in 10 minutes..."
      await session_mgr.persist_and_broadcast(
          session_id, {
              "type": ET.IMPROVE_ITERATION_COMPLETED,
              "status": "quota_waiting",
              "summary": msg
          })
      log.info("quota_polling_fallback", session=session_id)
      await asyncio.sleep(_QUOTA_POLL_INTERVAL)
    else:
      # Usage API unavailable — poll fallback
      msg = "Quota exhausted, retrying in 10 minutes..."
      await session_mgr.persist_and_broadcast(
          session_id, {
              "type": ET.IMPROVE_ITERATION_COMPLETED,
              "status": "quota_waiting",
              "summary": msg
          })
      log.info("quota_polling_no_usage_data", session=session_id)
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
    repo_path: str,
    work_branch: str,
    wt_path: Path,
    improve_dir: Path,
    cfg: CharlieBotConfig,
    session_mgr: "SessionManager",
    thread_mgr: "ThreadManager",
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
    state = load_improve_state(session_id, cfg)
    if state and state.status == "stopped":
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
        repo_path=repo_path,
        resolved_backend=resolved_backend,
        resolved_model=resolved_model,
        base_branch=base_branch,
        branch_name_override=work_branch,
        worktree_path_override=str(wt_path),
        skip_cleanup=True,
        skip_notify=True,
        improve_dir=str(improve_dir),
        iteration_number=i,
        is_continuation=(i > 1),
    )

    # Check for quota failure
    if await is_quota_failure(session_id, thread.id, thread_mgr):
      log.warning("improve_iteration_quota_failure", session=session_id, iteration=i)
      recovered = await _wait_for_quota_recovery(session_id, cfg, session_mgr)
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
    report_path = improve_dir / f'iter_{i:04d}.md'
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
    session_mgr: "SessionManager",
    thread_mgr: "ThreadManager",
    base_branch: Optional[str] = None,
    branch_prefix: Optional[str] = None,  # deprecated, use work_branch
    work_branch: Optional[str] = None,
    merge_back: bool = False,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> None:
  """Run the iterative improvement loop as a server-side async task.

  All iterations commit to a single work_branch in a single shared worktree.
  When done (or stopped), optionally merges work_branch back to base_branch,
  then triggers the master CC with the combined summary.
  """
  previous_summaries: list[str] = []

  # Compat: work_branch = work_branch or branch_prefix or auto-generated
  work_branch = work_branch or branch_prefix or f'improve/{int(time.time())}'
  resolved_repo = Path(repo_path).resolve()

  improve_id = work_branch.replace('/', '-')
  improve_dir = cfg.sessions_dir / session_id / 'improve' / improve_id
  await asyncio.to_thread(improve_dir.mkdir, parents=True, exist_ok=True)
  log.info(
      "improve_loop_backend_pinned",
      session=session_id,
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
    log.error("improve_loop_worktree_failed", session=session_id, error=str(e))
    failure_payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, [])
    failure_payload['error'] = f"Failed to create worktree: {e}"
    await session_mgr.persist_and_broadcast(session_id, failure_payload)
    await trigger_master(session_id, json.dumps(failure_payload, indent=2), cfg, session_mgr)
    return

  try:
    for i in range(1, iterations + 1):
      summary = await _run_single_iteration(
          i, iterations, goal, session_id, repo_path, work_branch, wt_path, improve_dir, cfg, session_mgr, thread_mgr,
          resolved_backend, resolved_model, previous_summaries, base_branch)
      if summary is None:
        break  # Stopped by user or session missing

    # Check if we exited because the user stopped the loop
    state = load_improve_state(session_id, cfg)
    stopped_by_user = state and state.status == 'stopped'

    if stopped_by_user:
      payload = _build_summary_payload(ET.IMPROVE_STOPPED, goal, previous_summaries)
      payload['reason'] = 'Stopped by user'
    else:
      payload = _build_summary_payload(ET.IMPROVE_COMPLETED, goal, previous_summaries)

    # Persist final status to improve_state.json
    state = load_improve_state(session_id, cfg) or ImproveState(goal=goal)
    state.status = 'stopped' if stopped_by_user else 'completed'
    save_improve_state(session_id, state, cfg)

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

  except asyncio.CancelledError:
    log.warning("improve_loop_cancelled", session=session_id)
    await session_mgr.persist_and_broadcast(session_id, {"type": ET.IMPROVE_CANCELLED, "goal": goal})
    raise
  except Exception:
    log.error("improve_loop_failed", session=session_id, exc_info=True)
    state = load_improve_state(session_id, cfg)
    if state:
      state.status = 'failed'
      save_improve_state(session_id, state, cfg)
  finally:
    # Always clean up the shared worktree
    if wt_path.exists():
      await git_worktree_remove(str(resolved_repo), wt_path, session_id)
      await git_worktree_prune(str(resolved_repo), session_id)
