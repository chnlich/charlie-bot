"""Tests for startup worktree quarantine: git helper, sweep selection, and trash listing."""

import shutil
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest

from src.core import git as git_module
from src.core import init as init_module
from src.core.config import CharlieBotConfig
from src.core.models import utc_now


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      worktree_dir=str(tmp_path / "worktrees"),
  )


def _make_worktree(parent: Path, name: str) -> Path:
  """Create a realistic worktree dir with caches, a .git marker, and preserved content."""
  wt = parent / name
  (wt / ".pixi").mkdir(parents=True)
  (wt / ".pixi" / "cached").write_text("regenerable", encoding="utf-8")
  (wt / ".local").mkdir()
  (wt / ".local" / "bin").write_text("regenerable", encoding="utf-8")
  (wt / "src.py").write_text("print('keep me')", encoding="utf-8")
  (wt / "diff.txt").write_text("the diff", encoding="utf-8")
  (wt / ".git").write_text("gitdir: ../repo/.git/worktrees/x\n", encoding="utf-8")
  return wt


def _thread(
    *,
    thread_id: str,
    status: str,
    worktree_path: Path,
    branch_name: str = "charliebot/task-x",
    age_days: float = 30.0,
    keep_worktree: bool = False,
    repo_path: str = "/tmp/repo",
    completed_at: Any = "__auto__",
) -> dict:
  if completed_at == "__auto__":
    completed_at = (utc_now() - timedelta(days=age_days)).isoformat()
  return {
      "id": thread_id,
      "status": status,
      "branch_name": branch_name,
      "repo_path": repo_path,
      "worktree_path": str(worktree_path),
      "keep_worktree": keep_worktree,
      "completed_at": completed_at,
  }


# ---------------------------------------------------------------------------
# git_quarantine_worktree helper
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_quarantine_strips_caches_moves_remainder_and_prunes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  worktree_parent = tmp_path / "worktrees"
  trash = worktree_parent / ".trash"
  wt = _make_worktree(worktree_parent, "charliebot-task-q1")

  prune_calls: list[tuple[str, str]] = []

  async def fake_prune(repo_path: str, thread_id: str) -> None:
    prune_calls.append((repo_path, thread_id))

  monkeypatch.setattr(git_module, "git_worktree_prune", fake_prune)

  dest = await git_module.git_quarantine_worktree(
      str(tmp_path / "repo"),
      wt,
      "thread-q1",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-q1",
      trash_dir=trash,
  )

  # Original moved away; remainder lives in trash.
  assert not wt.exists()
  assert dest == trash / "charliebot-task-q1"
  assert dest.is_dir()
  # Regenerable caches stripped before the move.
  assert not (dest / ".pixi").exists()
  assert not (dest / ".local").exists()
  # All non-cache content preserved (code, diff, git marker).
  assert (dest / "src.py").read_text(encoding="utf-8") == "print('keep me')"
  assert (dest / "diff.txt").read_text(encoding="utf-8") == "the diff"
  assert (dest / ".git").exists()
  assert prune_calls == [(str(tmp_path / "repo"), "thread-q1")]


@pytest.mark.asyncio
async def test_quarantine_resolves_trash_name_collision_with_thread_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  worktree_parent = tmp_path / "worktrees"
  trash = worktree_parent / ".trash"
  # A previously quarantined worktree of the same branch name already occupies the slot.
  (trash / "charliebot-task-q1").mkdir(parents=True)
  (trash / "charliebot-task-q1" / "old").write_text("old", encoding="utf-8")
  wt = _make_worktree(worktree_parent, "charliebot-task-q1")

  async def fake_prune(repo_path: str, thread_id: str) -> None:
    del repo_path, thread_id

  monkeypatch.setattr(git_module, "git_worktree_prune", fake_prune)

  dest = await git_module.git_quarantine_worktree(
      str(tmp_path / "repo"),
      wt,
      "thread-collide",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-q1",
      trash_dir=trash,
  )

  assert dest == trash / "charliebot-task-q1-thread-collide"
  assert (dest / "src.py").exists()
  # Pre-existing entry untouched.
  assert (trash / "charliebot-task-q1" / "old").read_text(encoding="utf-8") == "old"


@pytest.mark.asyncio
async def test_quarantine_rejects_unexpected_residue_name(tmp_path: Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt = worktree_parent / "unrelated-dir"
  wt.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="does not match expected"):
    await git_module.git_quarantine_worktree(
        str(tmp_path / "repo"),
        wt,
        "thread-bad",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-expected",
        trash_dir=worktree_parent / ".trash",
    )

  assert wt.exists()


# ---------------------------------------------------------------------------
# _quarantine_stale_failed_worktrees sweep
# ---------------------------------------------------------------------------


def _install_recording_quarantine(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Replace the git quarantine helper with a recorder that simulates the move."""
  quarantined: list[str] = []

  async def fake_quarantine(
      repo_path: str,
      wt_path: Path,
      thread_id: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
      trash_dir: Path,
  ) -> Path:
    del repo_path, thread_id, allowed_parent, expected_residue_name
    quarantined.append(str(wt_path))
    trash_dir.mkdir(parents=True, exist_ok=True)
    dest = trash_dir / wt_path.name
    shutil.move(str(wt_path), str(dest))
    return dest

  monkeypatch.setattr(init_module, "git_quarantine_worktree", fake_quarantine)
  return quarantined


@pytest.mark.asyncio
async def test_sweep_quarantines_old_failed_worktree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  wt = _make_worktree(parent, "charliebot-task-old")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=8.0)])

  assert quarantined == [str(wt)]
  assert not wt.exists()
  assert (parent / ".trash" / "charliebot-task-old").exists()


@pytest.mark.asyncio
async def test_sweep_age_threshold_boundary(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  recent = _make_worktree(parent, "charliebot-task-recent")
  old = _make_worktree(parent, "charliebot-task-aged")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg,
      [
          _thread(
              thread_id="recent",
              status="failed",
              worktree_path=recent,
              branch_name="charliebot/task-recent",
              age_days=init_module.FAILED_WORKTREE_QUARANTINE_DAYS - 1),
          _thread(
              thread_id="aged",
              status="failed",
              worktree_path=old,
              branch_name="charliebot/task-aged",
              age_days=init_module.FAILED_WORKTREE_QUARANTINE_DAYS + 1),
      ],
  )

  assert quarantined == [str(old)]
  assert recent.exists()
  assert not old.exists()


@pytest.mark.asyncio
async def test_sweep_skips_keep_worktree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  wt = _make_worktree(Path(cfg.worktree_dir), "charliebot-task-pinned")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=99.0, keep_worktree=True)])

  assert quarantined == []
  assert wt.exists()


@pytest.mark.asyncio
async def test_sweep_skips_missing_and_unparseable_completed_at(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  no_ts = _make_worktree(parent, "charliebot-task-nots")
  bad_ts = _make_worktree(parent, "charliebot-task-badts")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg,
      [
          _thread(
              thread_id="nots",
              status="failed",
              worktree_path=no_ts,
              branch_name="charliebot/task-nots",
              completed_at=None),
          _thread(
              thread_id="badts",
              status="failed",
              worktree_path=bad_ts,
              branch_name="charliebot/task-badts",
              completed_at="not-a-real-timestamp"),
      ],
  )

  assert quarantined == []
  assert no_ts.exists()
  assert bad_ts.exists()


@pytest.mark.asyncio
async def test_sweep_skips_when_running_thread_references_worktree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  wt = _make_worktree(Path(cfg.worktree_dir), "charliebot-task-shared")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg,
      [
          _thread(thread_id="failed", status="failed", worktree_path=wt, age_days=99.0),
          _thread(thread_id="running", status="running", worktree_path=wt, age_days=0.0),
      ],
  )

  assert quarantined == []
  assert wt.exists()


@pytest.mark.asyncio
async def test_sweep_dedups_shared_worktree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  wt = _make_worktree(Path(cfg.worktree_dir), "charliebot-task-chain")
  quarantined = _install_recording_quarantine(monkeypatch)

  # Original worker + its reviewer both failed and share one worktree -> moved once.
  await init_module._quarantine_stale_failed_worktrees(
      cfg,
      [
          _thread(thread_id="worker", status="failed", worktree_path=wt, age_days=10.0),
          _thread(thread_id="reviewer", status="failed", worktree_path=wt, age_days=9.0),
      ],
  )

  assert quarantined == [str(wt)]


@pytest.mark.asyncio
async def test_sweep_is_idempotent_on_rerun(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  wt = _make_worktree(Path(cfg.worktree_dir), "charliebot-task-once")
  quarantined = _install_recording_quarantine(monkeypatch)
  threads = [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=20.0)]

  await init_module._quarantine_stale_failed_worktrees(cfg, threads)
  await init_module._quarantine_stale_failed_worktrees(cfg, threads)

  # Second run sees the worktree path gone and does nothing.
  assert quarantined == [str(wt)]


@pytest.mark.asyncio
async def test_sweep_never_raises_when_helper_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  wt = _make_worktree(Path(cfg.worktree_dir), "charliebot-task-boom")

  async def boom(*args: Any, **kwargs: Any) -> Path:
    raise RuntimeError("simulated quarantine failure")

  monkeypatch.setattr(init_module, "git_quarantine_worktree", boom)

  # Must not propagate out of startup.
  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=20.0)])
  assert wt.exists()


# ---------------------------------------------------------------------------
# _recover_orphaned_threads integration (recovery + sweep in one walk)
# ---------------------------------------------------------------------------


def _write_thread_meta(cfg: CharlieBotConfig, session_id: str, meta: dict) -> Path:
  import json

  thread_dir = cfg.sessions_dir / session_id / "threads" / meta["id"]
  thread_dir.mkdir(parents=True, exist_ok=True)
  meta_path = thread_dir / "metadata.json"
  meta_path.write_text(json.dumps(meta), encoding="utf-8")
  return meta_path


@pytest.mark.asyncio
async def test_recover_orphaned_threads_recovers_and_sweeps(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  import json

  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  old_wt = _make_worktree(parent, "charliebot-task-aged")
  running_wt = _make_worktree(parent, "charliebot-task-live")
  quarantined = _install_recording_quarantine(monkeypatch)

  running_meta = _write_thread_meta(
      cfg, "s1", {
          "id": "running",
          "status": "running",
          "pid": None,
          "branch_name": "charliebot/task-live",
          "repo_path": "/tmp/repo",
          "worktree_path": str(running_wt),
      })
  _write_thread_meta(
      cfg, "s1",
      _thread(
          thread_id="aged", status="failed", worktree_path=old_wt, branch_name="charliebot/task-aged", age_days=20.0))

  await init_module._recover_orphaned_threads(cfg)

  # Running thread recovered to failed; its just-stamped completed_at keeps it out of the sweep.
  recovered = json.loads(running_meta.read_text(encoding="utf-8"))
  assert recovered["status"] == "failed"
  assert recovered["exit_code"] == -1
  assert running_wt.exists()
  # The aged failed worktree was quarantined.
  assert quarantined == [str(old_wt)]
  assert not old_wt.exists()
