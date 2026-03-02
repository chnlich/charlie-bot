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
async def test_spawn_worker_creates_worktree_and_uses_worktree_cwd(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  repo_path = (tmp_path / "repo").resolve()
  repo_path.mkdir(parents=True, exist_ok=True)
  events_log = tmp_path / "events.jsonl"
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Do work",
  )
  captures: dict[str, Any] = {}

  class FakeSessionManager:
    async def save_chat_event(self, session_id: str, event: dict[str, Any]) -> None:
      captures["chat_event"] = event

  class FakeThreadManager:
    async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
      return thread

    async def _save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return events_log

    async def update_status(
        self,
        session_id: str,
        thread_id: str,
        status: Any,
        pid: Optional[int] = None,
        exit_code: Optional[int] = None,
    ) -> None:
      captures["status"] = status
      captures["exit_code"] = exit_code

  async def fake_git_current_branch(repo: Path) -> str:
    assert repo == repo_path
    return "main"

  async def fake_git_create_worktree(repo: Path, base_branch: str, branch_name: str, wt_path: Path) -> None:
    captures["git_create_worktree"] = {
        "repo": repo,
        "base_branch": base_branch,
        "branch_name": branch_name,
        "wt_path": wt_path,
    }

  class FakeWorker:
    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["worker_backend"] = backend_option

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  async def fake_broadcast_and_persist(session_id: str, event: dict[str, Any], session_mgr: Any) -> None:
    captures["broadcast_event"] = event

  async def fake_notify_completion(
      session_id: str,
      description: str,
      thread_meta: ThreadMetadata,
      exit_code: int,
      thread_mgr: Any,
      session_mgr: Any,
      notify_cfg: CharlieBotConfig,
      quota_exhausted: bool = False,
      error: str = "",
  ) -> None:
    captures["notify_exit_code"] = exit_code

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner, "_git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "_git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(spawner, "Worker", FakeWorker)
  monkeypatch.setattr(spawner, "broadcast_and_persist", fake_broadcast_and_persist)
  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)

  await spawner.spawn_worker(
      session_id="session-id",
      description="Do work",
      thread_id="thread-1",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      repo_path=str(repo_path),
      resolved_backend="codex-o3",
      resolved_model="o3-pro",
  )
  monkeypatch.undo()

  assert "git_create_worktree" in captures
  assert captures["worker_dir"] == captures["git_create_worktree"]["wt_path"].resolve()
  assert captures["worker_dir"] != repo_path
  assert thread.worktree_path == str(captures["git_create_worktree"]["wt_path"])


@pytest.mark.asyncio
async def test_spawn_review_worker_propagates_backend_model(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg()
  captured: dict[str, Any] = {}
  saved_review_thread: dict[str, ThreadMetadata] = {}

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

    async def _save_metadata(self, meta: ThreadMetadata) -> None:
      saved_review_thread["meta"] = meta

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
      worktree_path="/tmp/worktrees/charliebot-task-1",
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
  assert saved_review_thread["meta"].worktree_path == "/tmp/worktrees/charliebot-task-1"


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
      worktree_path="/tmp/worktrees/charliebot-task-1",
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
