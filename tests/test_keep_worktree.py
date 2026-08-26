"""Tests for the --keep-worktree / keep_worktree delegation feature.

Covers the full delegate → worker → reviewer merge chain: when keep_worktree=True
the worktree directory must remain on disk after both _cleanup_worker_directory
(post-worker) and finalize_review_chain (post-reviewer merge).
"""

from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import (
  CODEX_BACKEND_OPTION,
  CapturingThreadManager,
  JudgmentShim,
  recording_notify_completion,
)

from src.core import review, spawner, spawner_finalize, spawner_launch
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
          CODEX_BACKEND_OPTION,
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
      loop_dir=None,
      iteration_number=None,
      is_continuation=False,
      keep_worktree=True,
      start_point=None,
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
      loop_dir=None,
      iteration_number=None,
      is_continuation=False,
      keep_worktree=False,
      start_point=None,
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

  async def fail_git_worktree_remove(*args: Any, **kwargs: Any) -> bool:
    raise AssertionError("git_worktree_remove must not be called when keep_worktree=True")

  monkeypatch.setattr(spawner_finalize, "_notify_completion", recording_notify_completion(captures))
  monkeypatch.setattr(spawner_finalize, "git_worktree_remove", fail_git_worktree_remove)

  await spawner._finalize_worker(
      session_id="session-id",
      description="slurm benchmark",
      thread=thread,
      outcome=spawner._WorkerRunOutcome(exit_code=0, quota_exhausted=False, error=""),
      thread_mgr=CapturingThreadManager(thread, captures),
      session_mgr=object(),
      cfg=cfg,
      skip_notify=False,
      task_type=TaskType.IMPLEMENT,
      completed_at=None,
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
        backend_option: BackendOption | None = None,
        on_spawned: Callable | None = None,
    ) -> None:
      captures["prompt"] = task_description

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner_launch, "git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(spawner_launch, "Worker", FakeWorker)
  monkeypatch.setattr(spawner_finalize, "_notify_completion", recording_notify_completion(captures))

  await spawner.spawn_worker(
      session_id="session-id",
      description="Run SLURM benchmark",
      thread_id="thread-1",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=CapturingThreadManager(thread, captures, events_log),
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
  assert captures["notify_thread"].keep_worktree is True
