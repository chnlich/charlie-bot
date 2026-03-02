from pathlib import Path
from typing import Any, Optional

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata
from src.core import spawner


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner.resolve_backend_option(cfg, "codex-o3", "o3-pro")
  assert opt.id == "codex-o3"
  assert opt.model == "o3-pro"

  with pytest.raises(ValueError, match="not configured"):
    spawner.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner.resolve_backend_option(cfg, "codex-o3", "")


@pytest.mark.asyncio
async def test_spawn_review_worker_propagates_backend_model(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg()
  captured: dict[str, Any] = {}

  class FakeSessionManager:
    async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
      return SessionMetadata(id=session_id, name="Scheduled: nightly", backend="claude-opus-4.6")

  class FakeThreadManager:
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

  async def fake_git_current_branch(repo_path: Path) -> str:
    return "main"

  async def fake_spawn_worker(
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

  def fake_create_task(coro: Any, name: Optional[str] = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    class DummyTask:
      pass
    return DummyTask()

  monkeypatch.setattr(spawner, "_git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", fake_create_task)

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      backend="codex-o3",
      model="o3-pro",
  )

  await spawner._spawn_review_worker(
      "session-id",
      original,
      cfg,
      FakeSessionManager(),
      FakeThreadManager(),
  )

  assert captured.get("resolved_backend") == "codex-o3"
  assert captured.get("resolved_model") == "o3-pro"


@pytest.mark.asyncio
async def test_spawn_review_worker_fails_if_backend_model_missing() -> None:
  cfg = _build_cfg()

  class FakeSessionManager:
    async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
      return SessionMetadata(id=session_id, name="Scheduled: nightly", backend="claude-opus-4.6")

  class FakeThreadManager:
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

  async def fake_git_current_branch(repo_path: Path) -> str:
    return "main"

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner, "_git_current_branch", fake_git_current_branch)

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      backend="codex-o3",
      model=None,
  )

  with pytest.raises(ValueError, match="missing model metadata"):
    await spawner._spawn_review_worker(
        "session-id",
        original,
        cfg,
        FakeSessionManager(),
        FakeThreadManager(),
    )
  monkeypatch.undo()
