"""Tests for the --keep-worktree / keep_worktree delegation feature.

Covers the full delegate → worker → reviewer merge chain: when keep_worktree=True
the worktree directory must remain on disk after both _cleanup_worker_directory
(post-worker) and finalize_review_chain (post-reviewer merge).
"""

from pathlib import Path
from typing import Any, Optional

import pytest
from conftest import JudgmentShim

from src.core import review, spawner
from src.core.config import CharlieBotConfig
from src.core.git import BaseResolution
from src.core.models import (
  BackendOption,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  ThreadMetadata,
  ThreadStatus,
)


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def test_build_worker_prompt_includes_keep_worktree_note(tmp_path: Path) -> None:
  prompt = spawner._build_worker_prompt(
      description="Run SLURM benchmark",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="bench"),
      cfg=_build_cfg(tmp_path),
      task_type=TaskType.IMPLEMENT,
      keep_worktree=True,
  )
  assert "This worktree will persist after the reviewer merges." in prompt
  assert "SLURM" in prompt


def test_build_worker_prompt_omits_keep_worktree_note_by_default(tmp_path: Path) -> None:
  prompt = spawner._build_worker_prompt(
      description="Run SLURM benchmark",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="bench"),
      cfg=_build_cfg(tmp_path),
      task_type=TaskType.IMPLEMENT,
  )
  assert "This worktree will persist after the reviewer merges." not in prompt


@pytest.mark.asyncio
async def test_cleanup_worker_directory_skips_when_keep_worktree(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """_finalize_worker must leave the worktree intact when thread.keep_worktree is True."""
  cfg = _build_cfg(tmp_path)
  wt_dir = tmp_path / "worktrees" / "charliebot-task-kept"
  wt_dir.mkdir(parents=True)
  (wt_dir / "marker.txt").write_text("still-here", encoding="utf-8")

  thread = ThreadMetadata(
      id="thread-keep",
      session_id="session-id",
      description="slurm benchmark",
      repo_path=str(tmp_path / "repo"),
      branch_name="charliebot/task-kept",
      worktree_path=str(wt_dir),
      require_review=True,
      keep_worktree=True,
  )

  captures: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
      del session_id, thread_id
      return thread

    async def update_status(
        self,
        session_id: str,
        thread_id: str,
        status: Any,
        pid: Optional[int] = None,
        exit_code: Optional[int] = None,
        completed_at: Any = None,
    ) -> None:
      captures["status"] = status
      captures["exit_code"] = exit_code

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
      task_type: TaskType = TaskType.IMPLEMENT,
  ) -> None:
    captures["notified"] = True

  async def fail_git_worktree_remove(*args: Any, **kwargs: Any) -> bool:
    raise AssertionError("git_worktree_remove must not be called when keep_worktree=True")

  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)
  monkeypatch.setattr(spawner, "git_worktree_remove", fail_git_worktree_remove)

  await spawner._finalize_worker(
      session_id="session-id",
      description="slurm benchmark",
      thread=thread,
      exit_code=0,
      thread_mgr=FakeThreadManager(),
      session_mgr=object(),
      cfg=cfg,
  )

  assert captures["status"] == ThreadStatus.COMPLETED
  assert captures["notified"] is True
  assert wt_dir.exists()
  assert (wt_dir / "marker.txt").read_text(encoding="utf-8") == "still-here"


@pytest.mark.asyncio
async def test_finalize_review_chain_skips_when_keep_worktree(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """finalize_review_chain must NOT remove the worktree when the original thread is flagged keep_worktree."""
  wt_dir = tmp_path / "worktrees" / "charliebot-task-kept"
  wt_dir.mkdir(parents=True)
  (wt_dir / "slurm.sh").write_text("#!/bin/bash\necho hi\n", encoding="utf-8")

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="slurm benchmark",
      branch_name="charliebot/task-kept",
      base_branch="main",
      repo_path=str(tmp_path / "repo"),
      worktree_path=str(wt_dir),
      keep_worktree=True,
  )

  async def fail_git_worktree_remove(*args: Any, **kwargs: Any) -> bool:
    raise AssertionError("git_worktree_remove must not be called when keep_worktree=True")

  monkeypatch.setattr(review, "git_worktree_remove", fail_git_worktree_remove)

  await review.finalize_review_chain(
      "session-id", original, thread_mgr=object(), worktree_parent=tmp_path / "worktrees")

  assert wt_dir.exists()
  assert (wt_dir / "slurm.sh").exists()


@pytest.mark.asyncio
async def test_finalize_review_chain_removes_worktree_by_default(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Sanity check: without keep_worktree=True, finalize_review_chain still removes the worktree."""
  wt_dir = tmp_path / "worktrees" / "charliebot-task-transient"
  wt_dir.mkdir(parents=True)

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="one-shot task",
      branch_name="charliebot/task-transient",
      base_branch="main",
      repo_path=str(tmp_path / "repo"),
      worktree_path=str(wt_dir),
  )

  remove_calls: list[tuple[str, Path, str, Path, str]] = []
  prune_calls: list[tuple[str, str]] = []

  async def fake_git_worktree_remove(
      repo_path: str,
      wt_path: Path,
      thread_id: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
  ) -> bool:
    remove_calls.append((repo_path, wt_path, thread_id, allowed_parent, expected_residue_name))
    return True

  async def fake_git_worktree_prune(repo_path: str, thread_id: str) -> None:
    prune_calls.append((repo_path, thread_id))

  monkeypatch.setattr(review, "git_worktree_remove", fake_git_worktree_remove)
  monkeypatch.setattr(review, "git_worktree_prune", fake_git_worktree_prune)

  await review.finalize_review_chain(
      "session-id", original, thread_mgr=object(), worktree_parent=tmp_path / "worktrees")

  assert len(remove_calls) == 1
  assert remove_calls[0][1] == wt_dir
  assert remove_calls[0][3] == tmp_path / "worktrees"
  assert remove_calls[0][4] == "charliebot-task-transient"
  assert len(prune_calls) == 1


@pytest.mark.asyncio
async def test_spawn_worker_persists_keep_worktree_on_thread(tmp_path: Path) -> None:
  """End-to-end-ish: SpawnRequest(keep_worktree=True) propagates to ThreadMetadata."""
  cfg = _build_cfg(tmp_path)
  repo_path = (tmp_path / "repo").resolve()
  repo_path.mkdir(parents=True, exist_ok=True)
  events_log = tmp_path / "events.jsonl"
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Run SLURM benchmark",
  )
  captures: dict[str, Any] = {}

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata:
      return SessionMetadata(id=session_id, name="Bench Session")

    async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
      captures.setdefault("broadcasts", []).append(event)

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
      return thread

    async def save_metadata(self, meta: ThreadMetadata) -> None:
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
        completed_at: Any = None,
    ) -> None:
      captures["status"] = status

  async def fake_git_create_worktree(repo: Path, base_branch: str, branch_name: str, wt_path: Path) -> BaseResolution:
    wt_path.mkdir(parents=True, exist_ok=True)
    return BaseResolution(canonical=base_branch, start_point=base_branch, detail="fake")

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
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["prompt"] = task_description

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

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
      task_type: TaskType = TaskType.IMPLEMENT,
  ) -> None:
    captures["notify_thread_keep_worktree"] = thread_meta.keep_worktree
    captures["notify_thread_worktree_path"] = thread_meta.worktree_path

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner, "git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(spawner, "Worker", FakeWorker)
  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)

  await spawner.spawn_worker(
      session_id="session-id",
      description="Run SLURM benchmark",
      thread_id="thread-1",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      request=SpawnRequest(
          repo_path=str(repo_path),
          base_branch="main",
          resolved_backend="codex-o3",
          resolved_model="o3",
          keep_worktree=True,
      ),
  )
  monkeypatch.undo()

  assert thread.keep_worktree is True
  assert "This worktree will persist after the reviewer merges." in captures["prompt"]
  wt_path = Path(thread.worktree_path)
  assert wt_path.exists()
  assert captures["notify_thread_keep_worktree"] is True
