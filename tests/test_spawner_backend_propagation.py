from pathlib import Path
from typing import Any

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    CODEX_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    CapturingThreadManager,
    JudgmentShim,
    backend_option,
    build_option_worktree_cfg,
    capturing_worker,
    make_fake_git_create_worktree,
    run_worktree_spawn,
    stage_worktree_spawn,
)

from src.core import spawner, spawner_launch
from src.core.config import CharlieBotConfig
from src.core.models import (
    SessionMetadata,
    SpawnRequest,
    TaskType,
    ThreadMetadata,
)


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  backend_option(
                      id=OPUS_BACKEND_ID,
                      label="Opus",
                      type="cc-claude",
                      model="claude-opus-4-6",
                      effort="max",
                      cli_binary="claude-sub",
                  ),
                  CODEX_BACKEND_OPTION,
              ]
      },
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner.resolve_backend_option(cfg, OPUS_BACKEND_ID, "claude-opus-4-6")
  assert opt.id == OPUS_BACKEND_ID
  assert opt.model == "claude-opus-4-6"
  assert opt.effort == "max"
  assert opt.cli_binary == "claude-sub"

  with pytest.raises(ValueError, match=r"is not in backends.options"):
    spawner.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner.resolve_backend_option(cfg, "codex-o3", "")


@pytest.mark.asyncio
async def test_spawn_worker_creates_worktree_and_uses_worktree_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  rig = stage_worktree_spawn(tmp_path, monkeypatch, description="Do work")

  await run_worktree_spawn(rig, resolved_model="o3-pro", keep_worktree=False)
  monkeypatch.undo()

  assert "git_create_worktree" in rig.captures
  assert rig.captures["worker_dir"] == rig.captures["git_create_worktree"]["wt_path"].resolve()
  assert rig.captures["worker_dir"] != rig.repo_path
  assert rig.thread.worktree_path == str(rig.captures["git_create_worktree"]["wt_path"])
  assert rig.thread.base_branch == "main"


@pytest.mark.asyncio
@pytest.mark.parametrize("reuse_worktree", [False, True], ids=["fresh_worktree", "worktree_override"])
async def test_create_worktree_and_process_raises_when_session_missing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    reuse_worktree: bool,
) -> None:
  """Both worktree-provisioning paths refuse to spawn when the session is gone."""
  cfg = build_option_worktree_cfg(tmp_path, CODEX_BACKEND_OPTION)
  repo_path = (tmp_path / "repo").resolve()
  repo_path.mkdir(parents=True, exist_ok=True)
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Do work",
  )

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata | None:
      return None

  if reuse_worktree:
    request = SpawnRequest(
        repo_path=str(repo_path),
        base_branch="main",
        worktree_path_override=str(tmp_path / "worktrees" / "reused"),
    )
  else:
    # The fresh-worktree path provisions the worktree before the session check,
    # so the real git_create_worktree must stay faked out even though the call
    # ends in the session-missing raise.
    monkeypatch.setattr(spawner_launch, "git_create_worktree", make_fake_git_create_worktree())
    request = SpawnRequest(repo_path=str(repo_path), base_branch="main")

  with pytest.raises(ValueError, match="session 'session-id' not found"):
    await spawner._create_worktree_and_process(
        "session-id",
        thread,
        "Do work",
        cfg,
        FakeSessionManager(),
        JudgmentShim(),
        repo_path,
        request,
    )


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_create_repoless_non_verify_profiles_propagate_antigravity_and_keep_prompt(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    task_type: TaskType,
) -> None:
  cfg = build_option_worktree_cfg(tmp_path, AGY_BACKEND_OPTION)
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Prompt task",
  )
  captures: dict[str, Any] = {}

  monkeypatch.setattr(spawner_launch, "Worker", capturing_worker(captures))

  await spawner._create_repoless_process(
      "session-id",
      thread,
      "Prompt task",
      cfg,
      CapturingThreadManager(thread, captures, tmp_path / "events.jsonl"),
      SpawnRequest(resolved_backend="agy", task_type=task_type),
  )

  assert thread.backend == "agy"
  assert thread.model is None
  assert captures["worker_backend"].id == "agy"
  assert captures["worker_backend"].model is None
  assert captures["task_description"] == "Prompt task"
