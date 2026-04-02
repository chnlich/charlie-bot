"""Tests for the /improve slash command orchestrator."""

import json
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from src.core import improve_command, spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata, ThreadStatus
from src.core.improve_command import (
    ImproveState,
    build_improve_master_prompt,
    load_improve_state,
    run_improve_loop,
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
  assert "--base-branch <base-branch>" in prompt


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


@pytest.mark.asyncio
async def test_run_improve_loop_pins_resolved_backend_across_iterations(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  thread_mgr = None
  spawn_calls: list[dict[str, str]] = []
  broadcasts: list[dict] = []
  triggered: dict[str, object] = {}

  class FakeSessionManager:
    def __init__(self) -> None:
      self._calls = 0

    async def get_session(self, session_id: str) -> SessionMetadata:
      self._calls += 1
      backend = "claude-opus-4.6" if self._calls == 1 else "codex-o3"
      return SessionMetadata(id=session_id, name="Test Session", backend=backend)

    async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
      del session_id
      broadcasts.append(event)

  class FakeThreadManager:
    def __init__(self) -> None:
      self.threads: dict[str, ThreadMetadata] = {}
      self.events_log_paths: dict[str, Path] = {}

    async def create_thread(
        self,
        session_meta: SessionMetadata,
        description: str,
        require_review: bool = True,
    ) -> ThreadMetadata:
      del require_review
      thread_id = f"thread-{len(self.threads) + 1}"
      thread = ThreadMetadata(id=thread_id, session_id=session_meta.id, description=description)
      self.threads[thread_id] = thread
      events_log_path = cfg.sessions_dir / session_meta.id / "threads" / thread_id / "data" / "events.jsonl"
      events_log_path.parent.mkdir(parents=True, exist_ok=True)
      events_log_path.write_text("", encoding="utf-8")
      self.events_log_paths[thread_id] = events_log_path
      return thread

    async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
      del session_id
      return self.threads[thread_id]

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      del session_id
      return self.events_log_paths[thread_id]

  thread_mgr = FakeThreadManager()
  session_mgr = FakeSessionManager()

  async def fake_spawn_worker(
      session_id: str,
      description: str,
      thread_id: str,
      cfg: CharlieBotConfig,
      session_mgr,
      thread_mgr,
      repo_path: str | None = None,
      context: str | None = None,
      prompt_override: str | None = None,
      resolved_backend: str = "",
      resolved_model: str = "",
      base_branch: str | None = None,
      branch_name_override: str | None = None,
      improve_dir: str | None = None,
      iteration_number: int | None = None,
      require_takeoff: bool = False,
  ) -> None:
    del session_id, description, cfg, session_mgr, repo_path, context, prompt_override, base_branch, improve_dir, require_takeoff
    spawn_calls.append({
        "thread_id": thread_id,
        "resolved_backend": resolved_backend,
        "resolved_model": resolved_model,
        "branch_name_override": branch_name_override or "",
        "iteration_number": str(iteration_number),
    })
    thread = thread_mgr.threads[thread_id]
    thread.status = ThreadStatus.COMPLETED
    thread.branch_name = branch_name_override or f"branch-{iteration_number}"

  async def fake_trigger_master(
      session_id: str,
      payload: str,
      cfg: CharlieBotConfig,
      session_mgr,
  ) -> None:
    del cfg, session_mgr
    triggered["session_id"] = session_id
    triggered["payload"] = json.loads(payload)

  monkeypatch.setattr(spawner, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(spawner, "_trigger_master", fake_trigger_master)

  await run_improve_loop(
      session_id="session-id",
      repo_path="/tmp/repo",
      iterations=2,
      goal="Optimize step time",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      branch_prefix="improve/perf",
      resolved_backend="claude-opus-4.6",
      resolved_model="claude-opus-4-6",
  )

  assert [call["resolved_backend"] for call in spawn_calls] == ["claude-opus-4.6", "claude-opus-4.6"]
  assert [call["resolved_model"] for call in spawn_calls] == ["claude-opus-4-6", "claude-opus-4-6"]
  assert [call["branch_name_override"] for call in spawn_calls] == ["improve/perf/iter1", "improve/perf/iter2"]
  assert triggered["session_id"] == "session-id"
  assert triggered["payload"]["type"] == improve_command.ET.IMPROVE_COMPLETED
  assert triggered["payload"]["iterations_completed"] == 2
  assert broadcasts[-1]["type"] == improve_command.ET.IMPROVE_COMPLETED
