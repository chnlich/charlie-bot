"""Tests for the /improve slash command orchestrator."""

from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.core.improve_command import (
    ImproveIteration,
    ImproveState,
    build_improve_worker_prompt,
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

  state = ImproveState(
      goal="optimize performance",
      max_iterations=3,
      current_iteration=1,
      status="running",
  )
  save_improve_state(session_id, state, cfg)

  loaded = load_improve_state(session_id, cfg)
  assert loaded is not None
  assert loaded.goal == "optimize performance"
  assert loaded.max_iterations == 3
  assert loaded.current_iteration == 1
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


def test_state_with_iterations(tmp_path: Path):
  """State with iterations round-trips correctly."""
  cfg = _make_cfg(tmp_path)
  session_id = "iter-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(
      goal="refactor auth",
      max_iterations=5,
      current_iteration=2,
      iterations=[
          ImproveIteration(iteration=1, worker_thread_id="t1", summary="Fixed login flow", status="completed"),
          ImproveIteration(iteration=2, worker_thread_id="t2", status="pending"),
      ],
  )
  save_improve_state(session_id, state, cfg)

  loaded = load_improve_state(session_id, cfg)
  assert loaded is not None
  assert len(loaded.iterations) == 2
  assert loaded.iterations[0].summary == "Fixed login flow"
  assert loaded.iterations[1].status == "pending"


# ---------------------------------------------------------------------------
# Prompt building
# ---------------------------------------------------------------------------


def test_prompt_building_first_iteration():
  """First iteration has no previous summaries."""
  state = ImproveState(goal="optimize performance", max_iterations=5, current_iteration=1)
  prompt = build_improve_worker_prompt(state)

  assert "Iteration 1/5" in prompt
  assert "optimize performance" in prompt
  assert "Previous Iterations" not in prompt
  assert "iterative improvement loop" in prompt


def test_prompt_building_with_summaries():
  """Accumulated summaries appear in prompt."""
  state = ImproveState(
      goal="refactor auth",
      max_iterations=5,
      current_iteration=3,
      iterations=[
          ImproveIteration(iteration=1, worker_thread_id="t1", summary="Extracted auth middleware", status="completed"),
          ImproveIteration(iteration=2, worker_thread_id="t2", summary="Added token validation", status="completed"),
      ],
  )
  prompt = build_improve_worker_prompt(state)

  assert "Iteration 3/5" in prompt
  assert "refactor auth" in prompt
  assert "Previous Iterations" in prompt
  assert "Extracted auth middleware" in prompt
  assert "Added token validation" in prompt


def test_prompt_includes_all_summaries():
  """All iteration summaries are included, no cap."""
  iterations = [
      ImproveIteration(iteration=i, worker_thread_id=f"t{i}", summary=f"Change {i}", status="completed")
      for i in range(1, 8)
  ]
  state = ImproveState(goal="big refactor", max_iterations=10, current_iteration=8, iterations=iterations)
  prompt = build_improve_worker_prompt(state)

  for i in range(1, 8):
    assert f"Change {i}" in prompt


# ---------------------------------------------------------------------------
# Stop signal
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stop_active_loop(tmp_path: Path):
  """Stopping an active loop returns True and sets status."""
  cfg = _make_cfg(tmp_path)
  session_id = "stop-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  state = ImproveState(goal="test", max_iterations=5, current_iteration=2, status="running")
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

  state = ImproveState(goal="test", max_iterations=5, current_iteration=5, status="completed")
  save_improve_state(session_id, state, cfg)

  result = await stop_improve_loop(session_id, cfg)
  assert result is False


# ---------------------------------------------------------------------------
# Spawner integration: improve loop without review
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_successful_worker_no_review_triggers_continue_improve_loop():
  """A successful improve-loop worker without review should call continue_improve_loop, not _trigger_master."""
  from src.core.spawner import _maybe_spawn_reviewer

  session_id = "improve-session"
  thread = MagicMock()
  thread.id = "thread-1"

  thread_meta = MagicMock()
  thread_meta.id = "thread-1"
  thread_meta.review_of = None
  thread_meta.require_review = False
  thread_meta.improve_loop = True

  thread_mgr = AsyncMock()
  thread_mgr.get_thread = AsyncMock(return_value=thread_meta)

  session_mgr = AsyncMock()
  cfg = MagicMock()

  with patch("src.core.improve_command.continue_improve_loop", new_callable=AsyncMock) as mock_continue, \
       patch("src.core.spawner._trigger_master", new_callable=AsyncMock) as mock_master:
    await _maybe_spawn_reviewer(
        session_id, thread, 0, "events summary", "full summary", thread_mgr, session_mgr, cfg
    )

    mock_continue.assert_awaited_once_with(
        session_id, thread_meta, "events summary", cfg, session_mgr, thread_mgr
    )
    mock_master.assert_not_awaited()


@pytest.mark.asyncio
async def test_successful_worker_no_review_no_improve_triggers_master():
  """A successful worker without review and without improve_loop should call _trigger_master."""
  from src.core.spawner import _maybe_spawn_reviewer

  session_id = "normal-session"
  thread = MagicMock()
  thread.id = "thread-1"

  thread_meta = MagicMock()
  thread_meta.id = "thread-1"
  thread_meta.review_of = None
  thread_meta.require_review = False
  thread_meta.improve_loop = False

  thread_mgr = AsyncMock()
  thread_mgr.get_thread = AsyncMock(return_value=thread_meta)

  session_mgr = AsyncMock()
  cfg = MagicMock()

  with patch("src.core.spawner._trigger_master", new_callable=AsyncMock) as mock_master, \
       patch("src.core.improve_command.continue_improve_loop", new_callable=AsyncMock) as mock_continue:
    await _maybe_spawn_reviewer(
        session_id, thread, 0, "events summary", "full summary", thread_mgr, session_mgr, cfg
    )

    mock_master.assert_awaited_once_with(session_id, "full summary", cfg, session_mgr)
    mock_continue.assert_not_awaited()
