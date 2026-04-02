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
from src.core.models import ThreadStatus
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
2. Confirm with the user: show the repo path and goal summary, and ask if they are ready to start. Do NOT propose per-iteration methods or a detailed plan — workers are fully autonomous and decide their own approach.
3. If the user requests a specific backend, it must be a configured `backend_options.id` from `~/.charliebot/config.yaml`; include `--backend <id>` in the command only when explicitly requested.
4. After approval, choose a descriptive `--branch-prefix` based on the goal (e.g. `improve/fix-precision`, `improve/optimize-step-time`). If the user mentions a Linear ticket, include it (e.g. `ALG-865/chaoli/20260326/fix-precision`). Then run:
   ```
   python -m src.cli.improve --session {session_id} --repo <repo> --base-branch <base-branch> --iterations {max_iterations} --goal '<the goal above>' --branch-prefix '<your chosen prefix>'
   ```
   Each iteration creates `<prefix>/iter1`, `<prefix>/iter2`, etc., automatically chaining on the previous iteration's code.
5. The CLI returns immediately after launching the server-side loop. You will receive a summary message when all iterations complete. Do NOT wait or poll — just let the user know the loop has started.

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
  events = parse_ndjson_file(events_path)
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
    branch_prefix: Optional[str] = None,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> None:
  """Run the iterative improvement loop as a server-side async task.

  For each iteration, creates a worker thread, awaits its completion,
  extracts a summary, and broadcasts progress. When done (or stopped),
  triggers the master CC with the combined summary.
  """
  from src.core.ndjson import parse_ndjson_file
  from src.core.spawner import _trigger_master, spawn_worker

  previous_summaries: list[str] = []
  prev_branch: Optional[str] = base_branch
  meta = None  # session metadata; assigned each iteration but needed after the inner loop

  improve_id = branch_prefix.replace('/', '-') if branch_prefix else f'improve-{int(time.time())}'
  improve_dir = cfg.sessions_dir / session_id / 'improve' / improve_id
  improve_dir.mkdir(parents=True, exist_ok=True)
  log.info(
      "improve_loop_backend_pinned",
      session=session_id,
      backend=resolved_backend,
      model=resolved_model,
  )

  try:
    for i in range(1, iterations + 1):
      while True:  # Retry loop for quota failures
        # Check if stopped
        state = load_improve_state(session_id, cfg)
        if state and state.status == "stopped":
          log.info("improve_loop_stopped", session=session_id, iteration=i)
          break

        # Build iteration description
        desc_parts = [f"Iterative improvement — iteration {i}/{iterations}", f"Goal: {goal}"]
        description = "\n".join(desc_parts)

        # Get session metadata
        meta = await session_mgr.get_session(session_id)
        if not meta:
          log.error("improve_loop_session_missing", session=session_id)
          break

        # Create thread and spawn worker
        thread = await thread_mgr.create_thread(meta, description, require_review=False)
        branch_name_override = f"{branch_prefix}/iter{i}" if branch_prefix else None
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
            base_branch=prev_branch,
            branch_name_override=branch_name_override,
            improve_dir=str(improve_dir),
            iteration_number=i,
        )

        # Check for quota failure
        if await is_quota_failure(session_id, thread.id, thread_mgr):
          log.warning("improve_iteration_quota_failure", session=session_id, iteration=i)
          recovered = await _wait_for_quota_recovery(session_id, cfg, session_mgr)
          if not recovered:
            log.info("improve_loop_stopped_during_quota_wait", session=session_id, iteration=i)
            break
          # Retry the same iteration — do NOT increment i or append summary
          continue

        # Successful (or non-quota failure) — extract summary and proceed
        thread_meta = await thread_mgr.get_thread(session_id, thread.id)
        status = thread_meta.status.value if thread_meta else "unknown"
        if thread_meta and thread_meta.branch_name:
          prev_branch = thread_meta.branch_name
        events_path = await thread_mgr.get_events_log_path(session_id, thread.id)
        events = parse_ndjson_file(events_path)
        summary = _extract_iteration_summary(events, i, status)
        previous_summaries.append(summary)

        # Write fallback report if the worker didn't write one
        report_path = improve_dir / f'iter_{i:04d}.md'
        if not report_path.exists():
          report_path.write_text(summary)

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
        break  # Move to next iteration

      # If we broke out of the while loop, check why
      state = load_improve_state(session_id, cfg)
      if (state and state.status == "stopped") or not meta:
        break

    # Broadcast completion and trigger master CC
    payload = _build_summary_payload(ET.IMPROVE_COMPLETED, goal, previous_summaries)
    await session_mgr.persist_and_broadcast(session_id, payload)
    await _trigger_master(session_id, json.dumps(payload, indent=2), cfg, session_mgr)

  except Exception:
    log.error("improve_loop_failed", session=session_id, exc_info=True)
    try:
      failure_payload = _build_summary_payload(ET.IMPROVE_FAILED, goal, previous_summaries)
      await _trigger_master(session_id, json.dumps(failure_payload, indent=2), cfg, session_mgr)
    except Exception:
      log.error("improve_loop_trigger_master_on_failure_failed", session=session_id, exc_info=True)
