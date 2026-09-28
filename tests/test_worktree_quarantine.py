"""Tests for startup worktree quarantine: git helper, sweep selection, and trash listing."""

import shutil
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    build_worktree_cfg,
    spy_on_load_json_meta,
)

from src.core import git as git_module
from src.core import init as init_module
from src.core import init_worker_recovery as worker_recovery_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, utc_now
from src.core.sessions import SessionManager


async def _make_session(cfg: CharlieBotConfig, session_id: str) -> None:
  """Create a real session with metadata so crash recovery's deliver_to_successor
  can resolve it; without a successor the report is written into the session itself."""
  mgr = SessionManager(cfg)
  session = await mgr.create_session(
      CreateSessionRequest(name=session_id, session_id=session_id), backend=OPUS_BACKEND_ID)
  assert session.id == session_id


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
    completed_at: Any = "__auto__",
) -> dict:
  if completed_at == "__auto__":
    completed_at = (utc_now() - timedelta(days=age_days)).isoformat()
  return {
      "id": thread_id,
      "session_id": "s1",
      "description": "test task",
      "status": status,
      "branch_name": branch_name,
      "repo_path": "/tmp/repo",
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
# _remove_local_worktree_artifacts

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

  monkeypatch.setattr(worker_recovery_module, "git_quarantine_worktree", fake_quarantine)
  return quarantined


@pytest.mark.asyncio
async def test_sweep_quarantines_old_failed_worktree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = build_worktree_cfg(tmp_path)
  parent = Path(cfg.paths.worktree_dir)
  wt = _make_worktree(parent, "charliebot-task-old")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=8.0)])

  assert quarantined == [str(wt)]
  assert not wt.exists()
  assert (parent / ".trash" / "charliebot-task-old").exists()


# ---------------------------------------------------------------------------
# run_crash_recovery: interrupted-run reconcile (never kills) + sweep
# ---------------------------------------------------------------------------


def _write_thread_meta(cfg: CharlieBotConfig, session_id: str, meta: dict) -> Path:
  import json

  meta.setdefault("session_id", session_id)
  meta.setdefault("description", "test task")
  thread_dir = cfg.sessions_dir / session_id / "threads" / meta["id"]
  thread_dir.mkdir(parents=True, exist_ok=True)
  meta_path = thread_dir / "metadata.json"
  meta_path.write_text(json.dumps(meta), encoding="utf-8")
  return meta_path


@pytest.mark.asyncio
async def test_run_crash_recovery_recovers_and_sweeps(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Startup sweeps the aged failed worktrees the thread metadata names; a
  still-running thread's worktree is never touched."""
  import json

  cfg = build_worktree_cfg(tmp_path)
  parent = Path(cfg.paths.worktree_dir)
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

  await init_module.run_crash_recovery(cfg, utc_now() + timedelta(hours=1))

  # The recovery pass writes nothing: legacy threads are read-only records.
  meta = json.loads(running_meta.read_text(encoding="utf-8"))
  assert meta["status"] == "running"
  assert running_wt.exists()
  # The aged failed worktree was quarantined.
  assert quarantined == [str(old_wt)]
  assert not old_wt.exists()


def test_scan_skips_post_boot_running_thread(tmp_path: Path) -> None:
  """A worker spawned during the recovery window (started_at > boot_time) is not interrupted."""
  import json

  cfg = build_worktree_cfg(tmp_path)
  boot_time = utc_now()
  meta_path = _write_thread_meta(
      cfg, "s1", {
          "id": "post-boot",
          "status": "running",
          "pid": 4242,
          "started_at": (boot_time + timedelta(seconds=30)).isoformat(),
      })

  threads = init_module._init_worker_recovery._scan_thread_metas(cfg)

  assert [m["id"] for m in threads] == ["post-boot"]
  # The scan writes nothing: the thread stays running.
  assert json.loads(meta_path.read_text(encoding="utf-8"))["status"] == "running"


@pytest.mark.asyncio
async def test_scan_skips_archived_session_threads(tmp_path: Path) -> None:
  """An archived session's pre-boot thread is not reconciled, yet still feeds the quarantine list."""
  cfg = build_worktree_cfg(tmp_path)
  await _make_session(cfg, "live")
  await _make_session(cfg, "done")
  started_at = utc_now().isoformat()
  for session_id, thread_id in (("live", "live-thread"), ("done", "archived-thread")):
    _write_thread_meta(cfg, session_id, {"id": thread_id, "status": "running", "pid": 4242, "started_at": started_at})
  await SessionManager(cfg).archive_session("done")

  threads = init_module._init_worker_recovery._scan_thread_metas(cfg)

  # Quarantine sees both: archiving says nothing about reclaiming worktree disk.
  assert sorted(m["id"] for m in threads) == ["archived-thread", "live-thread"]


# ---------------------------------------------------------------------------
# RUNNING_SCAN_WINDOW: stat-before-read gating
# ---------------------------------------------------------------------------


def _age_metadata_mtime(meta_path: Path, days: float) -> None:
  """Backdate a metadata.json's mtime by *days*, mimicking when it was last written."""
  import os

  ts = (utc_now() - timedelta(days=days)).timestamp()
  os.utime(meta_path, (ts, ts))


def test_scan_skips_out_of_window_thread(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A thread whose metadata mtime is outside the window is skipped unread.

  The reconcile scan must read only recently-modified thread metadata. A thread
  that still says 'running' but whose metadata.json has not been touched for
  longer than RUNNING_SCAN_WINDOW is neither read nor reconciled, while a
  recent pre-boot thread beside it still is.
  """
  cfg = build_worktree_cfg(tmp_path)
  read_paths = spy_on_load_json_meta(monkeypatch)

  boot_time = utc_now()
  recent_path = _write_thread_meta(
      cfg, "s1", {
          "id": "recent-running",
          "status": "running",
          "pid": 4242,
          "started_at": (boot_time - timedelta(minutes=5)).isoformat(),
      })
  stale_path = _write_thread_meta(
      cfg, "s1", {
          "id": "stale-running",
          "status": "running",
          "pid": 9999,
          "started_at": (boot_time - timedelta(days=200)).isoformat(),
      })
  _age_metadata_mtime(stale_path, init_module.RUNNING_SCAN_WINDOW.days + 5)

  threads = init_module._init_worker_recovery._scan_thread_metas(cfg)

  # Only the recent thread is read (the sweep's quarantine candidates live in
  # the recent window); the stale one is never touched.
  assert recent_path in read_paths
  assert stale_path not in read_paths
  assert [m["id"] for m in threads] == ["recent-running"]


@pytest.mark.asyncio
async def test_recover_window_covers_quarantine_band_and_skips_older(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The 30-day read window covers the 7-day quarantine band; older threads drop out.

  Drives the full crash-recovery path with three failed threads whose metadata mtime
  equals their completed_at (the realistic case):
    - 20 days: in window -> read -> in the 7-30d band -> quarantined;
    - 3 days:  in window -> read -> too recent (<7d) -> kept;
    - 40 days: outside window -> never read -> worktree kept (accepted negligible
      edge, only reachable if the server stayed up longer than the window).
  """
  cfg = build_worktree_cfg(tmp_path)
  parent = Path(cfg.paths.worktree_dir)
  band_wt = _make_worktree(parent, "charliebot-task-band")
  recent_wt = _make_worktree(parent, "charliebot-task-recent")
  ancient_wt = _make_worktree(parent, "charliebot-task-ancient")
  quarantined = _install_recording_quarantine(monkeypatch)

  for tid, wt, age in (("band", band_wt, 20.0), ("recent", recent_wt, 3.0), ("ancient", ancient_wt, 40.0)):
    meta_path = _write_thread_meta(
        cfg, "s1",
        _thread(thread_id=tid, status="failed", worktree_path=wt, branch_name=f"charliebot/task-{tid}", age_days=age))
    _age_metadata_mtime(meta_path, age)

  await init_module.run_crash_recovery(cfg, utc_now())

  assert quarantined == [str(band_wt)]
  assert not band_wt.exists()
  assert recent_wt.exists()
  assert ancient_wt.exists()
