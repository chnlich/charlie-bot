"""Tests for cross-backend reviewer selection via model_preference."""

from pathlib import Path
from typing import Any, Optional

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata
from src.core import spawner


BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
    BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
    BackendOption(id="kimi-k2.5", label="Kimi", type="cc-kimi", model="kimi-k2.5"),
]


def _build_cfg(**overrides: Any) -> CharlieBotConfig:
  defaults = dict(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=BACKEND_OPTIONS,
  )
  defaults.update(overrides)
  return CharlieBotConfig(**defaults)


def _make_original_thread(
    backend: str = "codex-o3",
    model: str = "o3",
) -> ThreadMetadata:
  return ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path="/tmp/worktrees/charliebot-task-1",
      backend=backend,
      model=model,
  )


class FakeSessionManager:
  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")


class FakeThreadManager:
  def __init__(self) -> None:
    self.saved: list[ThreadMetadata] = []

  async def create_thread(
      self,
      session_meta: SessionMetadata,
      description: str,
      branch_name: Optional[str] = None,
      review_of: Optional[str] = None,
  ) -> ThreadMetadata:
    return ThreadMetadata(
        id="review-thread-id",
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
    )

  async def _save_metadata(self, meta: ThreadMetadata) -> None:
    self.saved.append(meta)


async def _fake_git_current_branch(repo_path: Path) -> str:
  return "main"


async def _fake_spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: Any,
    thread_mgr: Any,
    repo_path: Optional[str] = None,
    prompt_override: Optional[str] = None,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> None:
  return None


def _capture_create_task(captured: dict[str, Any]):
  """Return a fake asyncio.create_task that captures spawn_worker kwargs."""

  def fake_create_task(coro: Any, name: Optional[str] = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()

    class DummyTask:
      pass

    return DummyTask()

  return fake_create_task


# --- resolve_preference_option tests ---


def test_resolve_preference_option_valid() -> None:
  cfg = _build_cfg()
  opt = spawner._resolve_preference_option(cfg, "kimi-k2.5")
  assert opt.id == "kimi-k2.5"
  assert opt.model == "kimi-k2.5"


def test_resolve_preference_option_missing_id() -> None:
  cfg = _build_cfg()
  with pytest.raises(ValueError, match="not in backend_options"):
    spawner._resolve_preference_option(cfg, "nonexistent")


def test_resolve_preference_option_no_model() -> None:
  cfg = _build_cfg(backend_options=[
      BackendOption(id="no-model", label="No Model", type="cc-claude", model=None),
  ])
  with pytest.raises(ValueError, match="no default model"):
    spawner._resolve_preference_option(cfg, "no-model")


# --- _spawn_review_worker preference tests ---


@pytest.mark.asyncio
async def test_empty_preference_uses_worker_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Empty model_preference -> reviewer uses same backend as worker."""
  cfg = _build_cfg(model_preference=[])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_selects_different_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """First non-matching preference entry is selected for the reviewer."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "kimi-k2.5"
  assert captured["resolved_model"] == "kimi-k2.5"


@pytest.mark.asyncio
async def test_preference_skips_same_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Entry matching the worker's backend is skipped; next entry is used."""
  cfg = _build_cfg(model_preference=["codex-o3", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "claude-opus-4.6"
  assert captured["resolved_model"] == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_preference_skips_invalid_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid preference entries are skipped; falls back to worker backend."""
  cfg = _build_cfg(model_preference=["nonexistent-1", "nonexistent-2"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_all_same_as_worker_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """All preference entries match worker backend -> falls back."""
  cfg = _build_cfg(model_preference=["codex-o3"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_skips_invalid_then_selects_valid(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid entry skipped, next valid entry selected."""
  cfg = _build_cfg(model_preference=["nonexistent", "kimi-k2.5"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "kimi-k2.5"
  assert captured["resolved_model"] == "kimi-k2.5"
