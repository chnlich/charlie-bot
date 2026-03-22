"""Tests for the /improve slash command orchestrator."""

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from src.core.improve_command import (
    ImproveState,
    build_improve_master_prompt,
    load_improve_state,
    save_improve_state,
    stop_improve_loop,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_cfg(tmp_path: Path):
  """Create a minimal config-like object with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


# ---------------------------------------------------------------------------
# Argument parsing (max_iterations extraction)
# ---------------------------------------------------------------------------


def test_parse_args_with_max_iterations():
  """First token as digit is parsed as max_iterations."""
  args_text = "3 refactor the auth module"
  parts = args_text.split(None, 1)
  max_iterations = 5
  goal = args_text
  if parts[0].isdigit():
    max_iterations = int(parts[0])
    goal = parts[1] if len(parts) > 1 else ''
  assert max_iterations == 3
  assert goal == "refactor the auth module"


def test_parse_args_without_max_iterations():
  """No leading digit means default max_iterations=5."""
  args_text = "optimize the codebase for performance"
  parts = args_text.split(None, 1)
  max_iterations = 5
  goal = args_text
  if parts[0].isdigit():
    max_iterations = int(parts[0])
    goal = parts[1] if len(parts) > 1 else ''
  assert max_iterations == 5
  assert goal == "optimize the codebase for performance"


def test_parse_args_only_number():
  """Only a number with no goal text."""
  args_text = "5"
  parts = args_text.split(None, 1)
  max_iterations = 5
  goal = args_text
  if parts[0].isdigit():
    max_iterations = int(parts[0])
    goal = parts[1] if len(parts) > 1 else ''
  assert max_iterations == 5
  assert goal == ''


# ---------------------------------------------------------------------------
# State file creation and loading
# ---------------------------------------------------------------------------


def test_save_and_load_state(tmp_path: Path):
  """State round-trips through save/load."""
  cfg = _make_cfg(tmp_path)
  session_id = "test-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(goal="optimize performance", max_iterations=3, status="running")
  save_improve_state(session_id, state, cfg)

  loaded = load_improve_state(session_id, cfg)
  assert loaded is not None
  assert loaded.goal == "optimize performance"
  assert loaded.max_iterations == 3
  assert loaded.status == "running"


def test_load_missing_state(tmp_path: Path):
  """Missing state file returns None."""
  cfg = _make_cfg(tmp_path)
  assert load_improve_state("nonexistent", cfg) is None


def test_load_corrupted_state(tmp_path: Path):
  """Corrupted state file returns None."""
  cfg = _make_cfg(tmp_path)
  session_id = "corrupt-session"
  state_dir = cfg.sessions_dir / session_id
  state_dir.mkdir(parents=True, exist_ok=True)
  (state_dir / "improve_state.json").write_text("not valid json{{{")

  assert load_improve_state(session_id, cfg) is None


def test_simplified_state_serialization(tmp_path: Path):
  """Simplified ImproveState serializes only goal, max_iterations, status."""
  cfg = _make_cfg(tmp_path)
  session_id = "simple-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(goal="refactor auth", max_iterations=3, status="running")
  save_improve_state(session_id, state, cfg)

  loaded = load_improve_state(session_id, cfg)
  assert loaded is not None
  assert loaded.goal == "refactor auth"
  assert loaded.max_iterations == 3
  assert loaded.status == "running"
  # Verify no extra fields like current_iteration or iterations
  dumped = loaded.model_dump()
  assert set(dumped.keys()) == {"goal", "max_iterations", "status"}


# ---------------------------------------------------------------------------
# Master prompt building
# ---------------------------------------------------------------------------


def test_build_improve_master_prompt_contains_goal(tmp_path: Path):
  """Master prompt contains the goal and session_id."""
  cfg = _make_cfg(tmp_path)
  session_id = "test-session-123"
  prompt = build_improve_master_prompt(session_id, "optimize performance", 5, cfg)

  assert "optimize performance" in prompt
  assert session_id in prompt
  assert "5" in prompt
  assert "python -m src.cli.improve" in prompt


def test_build_improve_master_prompt_contains_state_path(tmp_path: Path):
  """Master prompt includes the state file path."""
  cfg = _make_cfg(tmp_path)
  session_id = "test-session-456"
  prompt = build_improve_master_prompt(session_id, "fix bugs", 3, cfg)

  expected_path = str(cfg.sessions_dir / session_id / "improve_state.json")
  assert expected_path in prompt


# ---------------------------------------------------------------------------
# Stop signal
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stop_active_loop(tmp_path: Path):
  """Stopping an active loop returns True and sets status."""
  cfg = _make_cfg(tmp_path)
  session_id = "stop-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(goal="test", max_iterations=5, status="running")
  save_improve_state(session_id, state, cfg)

  result = await stop_improve_loop(session_id, cfg)
  assert result is True

  loaded = load_improve_state(session_id, cfg)
  assert loaded.status == "stopped"


@pytest.mark.asyncio
async def test_stop_no_active_loop(tmp_path: Path):
  """Stopping when no active loop returns False."""
  cfg = _make_cfg(tmp_path)
  result = await stop_improve_loop("nonexistent", cfg)
  assert result is False


@pytest.mark.asyncio
async def test_stop_already_completed(tmp_path: Path):
  """Stopping an already-completed loop returns False."""
  cfg = _make_cfg(tmp_path)
  session_id = "done-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(goal="test", max_iterations=5, status="completed")
  save_improve_state(session_id, state, cfg)

  result = await stop_improve_loop(session_id, cfg)
  assert result is False
