"""Iterative /improve loop orchestrator."""

from pathlib import Path
from typing import Optional

import structlog
from pydantic import BaseModel

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
2. Propose a plan to the user and wait for their approval.
3. After approval, run the following command:
   ```
   python -m src.cli.improve --session {session_id} --repo <repo> --iterations {max_iterations} --goal '{goal}'
   ```
4. The CLI handles the iteration loop. Wait for it to complete, then summarize the results to the user.

The improve state file is at: {state_path}"""
