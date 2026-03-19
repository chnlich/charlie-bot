"""Iterative /improve loop orchestrator."""

from pathlib import Path
from typing import Optional

import structlog
from pydantic import BaseModel, Field

from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager

log = structlog.get_logger()

# ---------------------------------------------------------------------------
# State models
# ---------------------------------------------------------------------------


class ImproveIteration(BaseModel):
  iteration: int
  worker_thread_id: str
  review_thread_id: Optional[str] = None
  summary: str = ""
  status: str = "pending"  # pending, completed, failed


class ImproveState(BaseModel):
  goal: str
  max_iterations: int = 5
  current_iteration: int = 0
  repo_path: Optional[str] = None
  status: str = "running"  # running, stopped, completed
  iterations: list[ImproveIteration] = Field(default_factory=list)


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
# Prompt building
# ---------------------------------------------------------------------------


def build_improve_worker_prompt(state: ImproveState) -> str:
  """Build the worker prompt with goal and accumulated summaries."""
  n = state.current_iteration
  max_iter = state.max_iterations

  prev_lines = ""
  # Cap at last 5 iterations to avoid prompt bloat
  recent = state.iterations[-5:] if state.iterations else []
  for it in recent:
    if it.summary:
      prev_lines += f"**Iteration {it.iteration}:** {it.summary}\n"

  previous_section = ""
  if prev_lines:
    previous_section = f"""### Previous Iterations
{prev_lines}"""

  return f"""## Iterative Improvement — Iteration {n}/{max_iter}

### Goal
{state.goal}

{previous_section}
### Instructions
You are part of an iterative improvement loop. Build on previous iterations' findings.
Focus on making meaningful, targeted improvements toward the goal.
After making changes, write a brief summary (2-3 sentences) of what you changed and why as your final message."""


# ---------------------------------------------------------------------------
# Core loop functions
# ---------------------------------------------------------------------------


async def start_improve_loop(
    session_id: str,
    goal: str,
    max_iterations: int,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    repo_path: Optional[str] = None,
) -> None:
  """Create improve state and spawn the first worker."""
  state = ImproveState(
      goal=goal,
      max_iterations=max_iterations,
      current_iteration=1,
      repo_path=repo_path,
  )
  save_improve_state(session_id, state, cfg)

  await session_mgr.persist_and_broadcast(
      session_id, {
          "type": "improve_started",
          "goal": goal,
          "max_iterations": max_iterations,
      })

  await _spawn_improve_worker(session_id, state, cfg, session_mgr, thread_mgr)


async def continue_improve_loop(
    session_id: str,
    thread,
    events_summary: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
) -> None:
  """Called when a reviewer completes for an improve-loop thread. Spawn next iteration or finish."""
  state = load_improve_state(session_id, cfg)
  if state is None:
    log.warning("improve_loop_state_missing", session=session_id)
    return

  # Record the completed iteration summary
  for it in state.iterations:
    if it.worker_thread_id == thread.id:
      it.summary = events_summary[:500]  # cap summary length
      it.status = "completed"
      break

  if state.status == "stopped":
    save_improve_state(session_id, state, cfg)
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": "improve_stopped",
            "iterations_completed": state.current_iteration,
        })
    return

  if state.current_iteration >= state.max_iterations:
    state.status = "completed"
    save_improve_state(session_id, state, cfg)
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": "improve_completed",
            "iterations_completed": state.current_iteration,
            "goal": state.goal,
        })
    # Trigger master with final summary
    from src.core.spawner import _trigger_master
    summaries = "\n".join(f"**Iteration {it.iteration}:** {it.summary}" for it in state.iterations if it.summary)
    final = f"**Improve loop completed** ({state.current_iteration} iterations)\n\nGoal: {state.goal}\n\n{summaries}"
    await _trigger_master(session_id, final, cfg, session_mgr)
    return

  # Spawn next iteration
  state.current_iteration += 1
  save_improve_state(session_id, state, cfg)
  await _spawn_improve_worker(session_id, state, cfg, session_mgr, thread_mgr)


async def stop_improve_loop(session_id: str, cfg: CharlieBotConfig) -> bool:
  """Set improve loop status to stopped. Returns True if there was an active loop."""
  state = load_improve_state(session_id, cfg)
  if state is None or state.status != "running":
    return False
  state.status = "stopped"
  save_improve_state(session_id, state, cfg)
  return True


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


async def _spawn_improve_worker(
    session_id: str,
    state: ImproveState,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
) -> None:
  """Spawn a worker for the current iteration."""
  from src.core.spawner import resolve_session_subagent_backend_model, spawn_worker

  prompt = build_improve_worker_prompt(state)
  description = f"Improve iteration {state.current_iteration}/{state.max_iterations}: {state.goal[:80]}"

  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    log.error("improve_loop_session_not_found", session=session_id)
    return

  thread = await thread_mgr.create_thread(
      session_meta,
      description,
      require_review=True,
      improve_loop=True,
  )

  resolved_backend, resolved_model = await resolve_session_subagent_backend_model(session_id, cfg, session_mgr)

  # Track iteration in state
  iteration = ImproveIteration(
      iteration=state.current_iteration,
      worker_thread_id=thread.id,
  )
  state.iterations.append(iteration)
  save_improve_state(session_id, state, cfg)

  await session_mgr.persist_and_broadcast(
      session_id, {
          "type": "improve_iteration",
          "iteration": state.current_iteration,
          "max_iterations": state.max_iterations,
          "thread_id": thread.id,
      })

  create_logged_task(
      spawn_worker(
          session_id,
          description,
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          repo_path=state.repo_path,
          prompt_override=prompt,
          resolved_backend=resolved_backend,
          resolved_model=resolved_model,
      ),
      name=f"improve_worker_{session_id[:8]}_iter{state.current_iteration}",
  )
