"""Iterative /improve loop orchestrator."""

import json
from pathlib import Path
from typing import Optional

import structlog
from pydantic import BaseModel

from src.api.message_utils import extract_text_from_message
from src.core.config import CharlieBotConfig

log = structlog.get_logger()

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
    log.warning("improve_state_corrupted", session=session_id, path=str(path))
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
1. Determine the target repository from the session context.
2. Confirm with the user: show the repo path and goal summary, and ask if they are ready to start. Do NOT propose per-iteration methods or a detailed plan — workers are fully autonomous and decide their own approach.
3. After approval, run the following command:
   ```
   python -m src.cli.improve --session {session_id} --repo <repo> --iterations {max_iterations} --goal '<the goal above>'
   ```
4. The CLI returns immediately after launching the server-side loop. You will receive a summary message when all iterations complete. Do NOT wait or poll — just let the user know the loop has started.

The improve state file is at: {state_path}"""


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _extract_iteration_summary(events: list[dict], iteration: int, status: str) -> str:
  """Extract a summary from worker events, preferring the result event."""
  for event in reversed(events):
    if event.get("type") == "result" and event.get("result"):
      return event["result"][:500]
    if event.get("type") == "assistant":
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
) -> None:
  """Run the iterative improvement loop as a server-side async task.

  For each iteration, creates a worker thread, awaits its completion,
  extracts a summary, and broadcasts progress. When done (or stopped),
  triggers the master CC with the combined summary.
  """
  from src.core.ndjson import parse_ndjson_file
  from src.core.spawner import _trigger_master, resolve_session_subagent_backend_model, spawn_worker

  previous_summaries: list[str] = []

  try:
    for i in range(1, iterations + 1):
      # Check if stopped
      state = load_improve_state(session_id, cfg)
      if state and state.status == "stopped":
        log.info("improve_loop_stopped", session=session_id, iteration=i)
        break

      # Build iteration description
      desc_parts = [f"Iterative improvement — iteration {i}/{iterations}", f"Goal: {goal}"]
      if previous_summaries:
        desc_parts.append("Previous iterations:")
        for idx, summary in enumerate(previous_summaries, 1):
          desc_parts.append(f"  Iteration {idx}: {summary}")
      description = "\n".join(desc_parts)

      # Get session metadata
      meta = await session_mgr.get_session(session_id)
      if not meta:
        log.error("improve_loop_session_missing", session=session_id)
        break

      # Create thread and spawn worker
      thread = await thread_mgr.create_thread(meta, description, require_review=False)
      resolved_backend, resolved_model = await resolve_session_subagent_backend_model(session_id, cfg, session_mgr)
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
      )

      # Extract summary
      thread_meta = await thread_mgr.get_thread(session_id, thread.id)
      status = thread_meta.status.value if thread_meta else "unknown"
      events_path = await thread_mgr.get_events_log_path(session_id, thread.id)
      events = parse_ndjson_file(events_path)
      summary = _extract_iteration_summary(events, i, status)
      previous_summaries.append(summary)
      log.info("improve_iteration_completed", session=session_id, iteration=i, status=status)

      # Broadcast progress event
      await session_mgr.persist_and_broadcast(
          session_id, {
              "type": "improve_iteration_completed",
              "iteration": i,
              "total_iterations": iterations,
              "status": status,
              "summary": summary[:200],
          })

    # Broadcast completion and trigger master CC
    payload = _build_summary_payload("improve_completed", goal, previous_summaries)
    await session_mgr.persist_and_broadcast(session_id, payload)
    await _trigger_master(session_id, json.dumps(payload, indent=2), cfg, session_mgr)

  except Exception:
    log.error("improve_loop_failed", session=session_id, exc_info=True)
    try:
      failure_payload = _build_summary_payload("improve_failed", goal, previous_summaries)
      await _trigger_master(session_id, json.dumps(failure_payload, indent=2), cfg, session_mgr)
    except Exception:
      log.error("improve_loop_trigger_master_on_failure_failed", session=session_id, exc_info=True)
