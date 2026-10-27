"""Disk sweep candidate safety, archiving and reporting."""

import asyncio
import datetime
import json
import os
import pathlib
import subprocess
import sys
import tarfile

import pytest

from src.features.disk_sweep import sweep
from src.infra import config, models
from src.runtime.hooks import scheduled_handlers


def _cfg(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """Build a profile whose worktrees and scratch directory stay under tmp_path."""
  worktree_dir = tmp_path / "worktrees"
  scratch_dir = tmp_path / "scripts"
  worktree_dir.mkdir()
  scratch_dir.mkdir()
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home", paths={
          "worktree_dir": str(worktree_dir),
          "scratch_dir": str(scratch_dir),
      })


def _git(repo_path: pathlib.Path, *args: str) -> str:
  """Run a successful git command in a temporary repository."""
  proc = subprocess.run(
      ["git", *args], cwd=repo_path, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False)
  assert proc.returncode == 0, proc.stderr
  return proc.stdout.strip()


def _create_repo(repo_path: pathlib.Path) -> pathlib.Path:
  """Create a committed repository with one tracked file."""
  repo_path.mkdir(parents=True)
  _git(repo_path, "init", "-b", "main")
  _git(repo_path, "config", "user.name", "Disk Sweep Test")
  _git(repo_path, "config", "user.email", "disk-sweep@example.test")
  (repo_path / "tracked.txt").write_text("original\n", encoding="utf-8")
  _git(repo_path, "add", "tracked.txt")
  _git(repo_path, "commit", "-m", "initial")
  return repo_path


def _create_worktree(
    cfg: config.CharlieBotConfig,
    repo_path: pathlib.Path,
    name: str,
) -> tuple[pathlib.Path, str]:
  """Create a CharlieBot-named worktree under the configured root."""
  branch = f"charliebot/{name}"
  worktree_path = pathlib.Path(cfg.paths.worktree_dir) / branch.replace("/", "-")
  _git(repo_path, "worktree", "add", "-b", branch, str(worktree_path), "HEAD")
  return worktree_path, branch


def _write_owner(
    cfg: config.CharlieBotConfig,
    *,
    session_id: str,
    worktree_path: pathlib.Path,
    repo_path: pathlib.Path,
    branch: str,
    closed_at: datetime.datetime | None,
    keep_worktree: bool = False,
    reopened: bool = False,
    outcome: str = "archived",
) -> pathlib.Path:
  """Write task metadata, lifecycle facts and a worktree Run record."""
  session_dir = cfg.sessions_dir / session_id
  run_id = f"run-{session_id}"
  run_dir = session_dir / "data" / "runs" / run_id
  run_dir.mkdir(parents=True)
  metadata = {"task": {"goal": "test task", "keep_worktree": keep_worktree}}
  (session_dir / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
  events = []
  if closed_at is not None:
    events.append({"type": "task_closed", "timestamp": closed_at.isoformat(), "outcome": outcome})
  if reopened:
    events.append({"type": "task_reopened", "timestamp": closed_at.isoformat() if closed_at else None})
  event_path = session_dir / "data" / "chat_events.jsonl"
  event_path.parent.mkdir(parents=True, exist_ok=True)
  event_path.write_text("".join(json.dumps(event) + "\n" for event in events), encoding="utf-8")
  record = models.RunRecord(
      id=run_id,
      session_id=session_id,
      repo_path=str(repo_path),
      branch_name=branch,
      worktree_path=str(worktree_path.resolve()),
  )
  (run_dir / "metadata.json").write_text(record.model_dump_json(), encoding="utf-8")
  return run_dir


def _run_sweep(
    cfg: config.CharlieBotConfig,
    *,
    apply: bool = False,
    now: datetime.datetime,
    proc_root: pathlib.Path,
) -> sweep.SweepReport:
  """Drive the sweep coroutine with an explicit clock and process tree."""
  return asyncio.run(sweep.run_sweep(cfg, apply=apply, now=now, proc_root=proc_root))


def _set_tree_mtime(path: pathlib.Path, timestamp: float) -> None:
  """Make every entry in a tree old enough for a scratch-age check."""
  for root, directories, filenames in os.walk(path, followlinks=False):
    for name in [*directories, *filenames]:
      os.utime(pathlib.Path(root) / name, (timestamp, timestamp), follow_symlinks=False)
  os.utime(path, (timestamp, timestamp), follow_symlinks=False)


def _empty_proc_root(tmp_path: pathlib.Path) -> pathlib.Path:
  """Create an empty process tree for deterministic process checks."""
  proc_root = tmp_path / "proc"
  proc_root.mkdir()
  return proc_root


def _fake_process(
    proc_root: pathlib.Path,
    *,
    pid: int,
    cwd: pathlib.Path,
    cmdline: bytes,
    exe: pathlib.Path,
    fd_target: pathlib.Path,
    root: pathlib.Path = pathlib.Path("/"),
    maps: str = "",
) -> pathlib.Path:
  """Write readable proc path files for one same-user process."""
  proc_dir = proc_root / str(pid)
  proc_dir.mkdir()
  (proc_dir / "cwd").symlink_to(cwd)
  (proc_dir / "root").symlink_to(root)
  (proc_dir / "exe").symlink_to(exe)
  fd_dir = proc_dir / "fd"
  fd_dir.mkdir()
  (fd_dir / "3").symlink_to(fd_target)
  (proc_dir / "maps").write_text(maps, encoding="utf-8")
  (proc_dir / "cmdline").write_bytes(cmdline)
  return proc_dir


def test_worktree_age_reopen_keep_and_unknown_owner_rules(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  proc_root = _empty_proc_root(tmp_path)
  now = datetime.datetime(2026, 5, 20, tzinfo=datetime.UTC)

  due_path, due_branch = _create_worktree(cfg, repo_path, "due")
  _write_owner(
      cfg,
      session_id="due",
      worktree_path=due_path,
      repo_path=repo_path,
      branch=due_branch,
      closed_at=now - datetime.timedelta(days=3),
      outcome="succeeded",
  )
  young_path, young_branch = _create_worktree(cfg, repo_path, "young")
  _write_owner(
      cfg,
      session_id="young",
      worktree_path=young_path,
      repo_path=repo_path,
      branch=young_branch,
      closed_at=now - datetime.timedelta(days=2, hours=23),
  )
  kept_path, kept_branch = _create_worktree(cfg, repo_path, "kept")
  _write_owner(
      cfg,
      session_id="kept",
      worktree_path=kept_path,
      repo_path=repo_path,
      branch=kept_branch,
      closed_at=now - datetime.timedelta(days=14),
      keep_worktree=True,
  )
  kept_young_path, kept_young_branch = _create_worktree(cfg, repo_path, "kept-young")
  _write_owner(
      cfg,
      session_id="kept-young",
      worktree_path=kept_young_path,
      repo_path=repo_path,
      branch=kept_young_branch,
      closed_at=now - datetime.timedelta(days=13),
      keep_worktree=True,
  )
  reopened_path, reopened_branch = _create_worktree(cfg, repo_path, "reopened")
  _write_owner(
      cfg,
      session_id="reopened",
      worktree_path=reopened_path,
      repo_path=repo_path,
      branch=reopened_branch,
      closed_at=now - datetime.timedelta(days=30),
      reopened=True,
  )
  unknown_path, unknown_branch = _create_worktree(cfg, repo_path, "unknown")
  unknown_run_dir = cfg.sessions_dir / "missing-owner" / "data" / "runs" / "run-missing-owner"
  unknown_run_dir.mkdir(parents=True)
  unknown_record = models.RunRecord(
      id="run-missing-owner",
      session_id="missing-owner",
      repo_path=str(repo_path),
      branch_name=unknown_branch,
      worktree_path=str(unknown_path.resolve()),
  )
  (unknown_run_dir / "metadata.json").write_text(unknown_record.model_dump_json(), encoding="utf-8")

  report = _run_sweep(cfg, now=now, proc_root=proc_root)
  by_path = {row.path: row for row in report.rows}

  assert by_path[due_path.resolve()].action == "delete"
  assert by_path[kept_path.resolve()].action == "delete"
  assert by_path[reopened_path.resolve()].reason == "task-open"
  assert by_path[unknown_path.resolve()].reason == "unknown-owner"
  assert young_path.resolve() not in by_path
  assert kept_young_path.resolve() not in by_path


def test_shared_worktree_uses_newest_owner_close_and_any_keep_flag(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  proc_root = _empty_proc_root(tmp_path)
  now = datetime.datetime(2026, 5, 20, tzinfo=datetime.UTC)

  recently_owned_path, recently_owned_branch = _create_worktree(cfg, repo_path, "recent-owner")
  _write_owner(
      cfg,
      session_id="old-owner",
      worktree_path=recently_owned_path,
      repo_path=repo_path,
      branch=recently_owned_branch,
      closed_at=now - datetime.timedelta(days=30),
  )
  _write_owner(
      cfg,
      session_id="recent-owner",
      worktree_path=recently_owned_path,
      repo_path=repo_path,
      branch=recently_owned_branch,
      closed_at=now - datetime.timedelta(days=2),
      outcome="failed",
  )

  kept_path, kept_branch = _create_worktree(cfg, repo_path, "shared-kept")
  _write_owner(
      cfg,
      session_id="shared-ordinary-owner",
      worktree_path=kept_path,
      repo_path=repo_path,
      branch=kept_branch,
      closed_at=now - datetime.timedelta(days=15),
  )
  _write_owner(
      cfg,
      session_id="shared-kept-owner",
      worktree_path=kept_path,
      repo_path=repo_path,
      branch=kept_branch,
      closed_at=now - datetime.timedelta(days=15),
      keep_worktree=True,
  )

  report = _run_sweep(cfg, now=now, proc_root=proc_root)
  by_path = {row.path: row for row in report.rows}

  assert recently_owned_path.resolve() not in by_path
  assert by_path[kept_path.resolve()].action == "delete"


@pytest.mark.parametrize("change", ["tracked", "untracked"])
def test_apply_archives_worktree_changes_before_removing_it(
    tmp_path: pathlib.Path,
    change: str,
) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  worktree_path, branch = _create_worktree(cfg, repo_path, change)
  run_dir = _write_owner(
      cfg,
      session_id=change,
      worktree_path=worktree_path,
      repo_path=repo_path,
      branch=branch,
      closed_at=datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC),
  )
  if change == "tracked":
    (worktree_path / "tracked.txt").write_text("changed\n", encoding="utf-8")
  else:
    (worktree_path / "new.txt").write_text("untracked contents\n", encoding="utf-8")

  report = _run_sweep(
      cfg, apply=True, now=datetime.datetime(2026, 1, 20, tzinfo=datetime.UTC), proc_root=_empty_proc_root(tmp_path))
  row = next(row for row in report.rows if row.path == worktree_path.resolve())
  archive_path = run_dir / "worktree-changes.tar"

  assert row.action == "archive+delete"
  assert not worktree_path.exists()
  assert archive_path.is_file()
  with tarfile.open(archive_path) as archive:
    names = set(archive.getnames())
    assert "tracked.patch" in names
    patch = archive.extractfile("tracked.patch")
    assert patch is not None
    if change == "tracked":
      assert b"changed" in patch.read()
      assert "untracked/new.txt" not in names
    else:
      assert patch.read() == b""
      assert "untracked/new.txt" in names
      untracked = archive.extractfile("untracked/new.txt")
      assert untracked is not None
      assert untracked.read() == b"untracked contents\n"


def test_large_untracked_and_dirty_submodule_worktrees_are_skipped(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  submodule_source = _create_repo(tmp_path / "submodule-source")
  _git(repo_path, "-c", "protocol.file.allow=always", "submodule", "add", str(submodule_source), "deps/lib")
  _git(repo_path, "commit", "-m", "add submodule")
  large_path, large_branch = _create_worktree(cfg, repo_path, "large")
  dirty_submodule_path, submodule_branch = _create_worktree(cfg, repo_path, "dirty-submodule")
  _git(dirty_submodule_path, "-c", "protocol.file.allow=always", "submodule", "update", "--init")
  _write_owner(
      cfg,
      session_id="large",
      worktree_path=large_path,
      repo_path=repo_path,
      branch=large_branch,
      closed_at=datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC),
  )
  _write_owner(
      cfg,
      session_id="dirty-submodule",
      worktree_path=dirty_submodule_path,
      repo_path=repo_path,
      branch=submodule_branch,
      closed_at=datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC),
  )
  (large_path / "too-large.bin").write_bytes(b"x" * (10 * 1024 * 1024 + 1))
  (dirty_submodule_path / "deps" / "lib" / "tracked.txt").write_text("submodule edit\n", encoding="utf-8")

  report = _run_sweep(
      cfg, now=datetime.datetime(2026, 1, 20, tzinfo=datetime.UTC), proc_root=_empty_proc_root(tmp_path))
  by_path = {row.path: row for row in report.rows}

  assert by_path[large_path.resolve()].reason == "large-untracked"
  assert by_path[dirty_submodule_path.resolve()].reason == "dirty-submodule"
  assert large_path.is_dir()
  assert dirty_submodule_path.is_dir()


@pytest.mark.parametrize("source", ["cwd", "cmdline"])
def test_process_cwd_and_cmdline_paths_protect_a_worktree(
    tmp_path: pathlib.Path,
    source: str,
) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  worktree_path, branch = _create_worktree(cfg, repo_path, source)
  _write_owner(
      cfg,
      session_id=source,
      worktree_path=worktree_path,
      repo_path=repo_path,
      branch=branch,
      closed_at=datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC),
  )
  proc_root = _empty_proc_root(tmp_path)
  outside = tmp_path / "outside"
  outside.write_text("open file\n", encoding="utf-8")
  cwd = worktree_path if source == "cwd" else tmp_path
  cmd_target = worktree_path / "tracked.txt" if source == "cmdline" else outside
  _fake_process(
      proc_root,
      pid=12345,
      cwd=cwd,
      cmdline=b"python\0" + os.fsencode(cmd_target) + b"\0",
      exe=pathlib.Path(sys.executable).resolve(),
      fd_target=outside,
  )

  report = _run_sweep(cfg, now=datetime.datetime(2026, 1, 20, tzinfo=datetime.UTC), proc_root=proc_root)
  row = next(row for row in report.rows if row.path == worktree_path.resolve())

  assert row.reason == "in-use"
  assert row.action == "skip"
  assert worktree_path.is_dir()


@pytest.mark.parametrize("source", ["root", "exe", "fd", "maps"])
def test_process_root_exe_fd_and_maps_paths_protect_a_worktree(
    tmp_path: pathlib.Path,
    source: str,
) -> None:
  cfg = _cfg(tmp_path)
  repo_path = _create_repo(tmp_path / "repo")
  worktree_path, branch = _create_worktree(cfg, repo_path, source)
  _write_owner(
      cfg,
      session_id=source,
      worktree_path=worktree_path,
      repo_path=repo_path,
      branch=branch,
      closed_at=datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC),
  )
  proc_root = _empty_proc_root(tmp_path)
  outside = tmp_path / "outside"
  outside.write_text("open file\n", encoding="utf-8")
  target = worktree_path / "tracked.txt"
  _fake_process(
      proc_root,
      pid=12346,
      cwd=tmp_path,
      cmdline=b"python\0",
      exe=target if source == "exe" else pathlib.Path(sys.executable).resolve(),
      fd_target=target if source == "fd" else outside,
      root=worktree_path if source == "root" else pathlib.Path("/"),
      maps=f"1000-2000 r-xp 00000000 08:01 123 {target}\n" if source == "maps" else "",
  )

  report = _run_sweep(cfg, now=datetime.datetime(2026, 1, 20, tzinfo=datetime.UTC), proc_root=proc_root)
  row = next(row for row in report.rows if row.path == worktree_path.resolve())

  assert row.reason == "in-use"
  assert row.action == "skip"
  assert worktree_path.is_dir()


def test_scratch_age_and_nested_repository_safety(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  scratch_root = pathlib.Path(cfg.paths.scratch_dir)
  now = datetime.datetime(2026, 5, 20, tzinfo=datetime.UTC)
  old_timestamp = (now - datetime.timedelta(days=15)).timestamp()

  old_clean = scratch_root / "old-clean"
  old_clean.mkdir()
  (old_clean / "file.txt").write_text("data\n", encoding="utf-8")
  _set_tree_mtime(old_clean, old_timestamp)

  new_directory = scratch_root / "new"
  new_directory.mkdir()
  (new_directory / "file.txt").write_text("new\n", encoding="utf-8")

  dirty_directory = scratch_root / "dirty-repo"
  dirty_repo = _create_repo(dirty_directory / "nested")
  (dirty_repo / "tracked.txt").write_text("uncommitted\n", encoding="utf-8")
  _set_tree_mtime(dirty_directory, old_timestamp)

  unpushed_directory = scratch_root / "unpushed-repo"
  _create_repo(unpushed_directory / "nested")
  _set_tree_mtime(unpushed_directory, old_timestamp)

  detached_directory = scratch_root / "detached-repo"
  detached_repo = _create_repo(detached_directory / "nested")
  remote = tmp_path / "remote.git"
  _git(tmp_path, "init", "--bare", str(remote))
  _git(detached_repo, "remote", "add", "origin", str(remote))
  _git(detached_repo, "push", "-u", "origin", "main")
  _git(detached_repo, "checkout", "--detach")
  (detached_repo / "tracked.txt").write_text("detached commit\n", encoding="utf-8")
  _git(detached_repo, "commit", "-am", "detached head commit")
  _set_tree_mtime(detached_directory, old_timestamp)

  report = _run_sweep(cfg, now=now, proc_root=_empty_proc_root(tmp_path))
  by_path = {row.path: row for row in report.rows}

  assert by_path[old_clean.resolve()].action == "delete"
  assert by_path[dirty_directory.resolve()].reason == "has-unsaved-repo"
  assert by_path[unpushed_directory.resolve()].reason == "has-unsaved-repo"
  assert by_path[detached_directory.resolve()].reason == "has-unsaved-repo"
  assert new_directory.resolve() not in by_path


def test_scratch_tree_with_unreadable_entries_is_reported_as_skipped(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _cfg(tmp_path)
  unreadable = pathlib.Path(cfg.paths.scratch_dir) / "unreadable"
  unreadable.mkdir()

  def unreadable_tree(_path: pathlib.Path) -> float:
    raise PermissionError("denied")

  monkeypatch.setattr(sweep, "_tree_latest_mtime", unreadable_tree)

  report = _run_sweep(
      cfg,
      now=datetime.datetime(2026, 5, 20, tzinfo=datetime.UTC),
      proc_root=_empty_proc_root(tmp_path),
  )
  row = next(row for row in report.rows if row.path == unreadable.resolve())

  assert row.action == "skip"
  assert row.reason == "unreadable-path: denied"
  assert unreadable.is_dir()


def test_feature_registers_a_scheduled_handler() -> None:
  assert scheduled_handlers.handler("disk_sweep") is sweep.run_scheduled
