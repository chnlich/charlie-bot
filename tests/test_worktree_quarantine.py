"""Tests for startup worktree quarantine: git helper, sweep selection, and trash listing."""

import shutil
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import (
  OPUS_BACKEND_ID,
  RECOVERY_TASK_PREFIXES,
  SPAWNER_RESUME_WORKER_PATCH_TARGET,
  await_recovery_tasks,
)

from src.core import git as git_module
from src.core import init as init_module
from src.core import init_worker_recovery as worker_recovery_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, utc_now
from src.core.sessions import SessionManager


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      worktree_dir=str(tmp_path / "worktrees"),
  )


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
    repo_path: str = "/tmp/repo",
    session_id: str = "s1",
    description: str = "test task",
    completed_at: Any = "__auto__",
) -> dict:
  if completed_at == "__auto__":
    completed_at = (utc_now() - timedelta(days=age_days)).isoformat()
  return {
      "id": thread_id,
      "session_id": session_id,
      "description": description,
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
# _remove_local_worktree_artifacts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remove_local_artifacts_skips_symlink_and_keeps_target(
    tmp_path: Path) -> None:
  """A `.venv` symlink into an outside checkout survives cleanup without raising.

  The symlink (one cheap directory entry `git worktree remove --force` deletes on
  its own) is skipped and logged, never deleted, and its target is never touched;
  a real regenerable artifact directory in the same tree is still removed.
  """
  wt = tmp_path / "worktrees" / "charliebot-task-sym"
  wt.mkdir(parents=True)
  outside = tmp_path / "venv-target"
  outside.mkdir()
  (outside / "site.py").write_text("real", encoding="utf-8")
  (wt / ".venv").symlink_to(outside, target_is_directory=True)
  (wt / ".pixi").mkdir()
  (wt / ".pixi" / "cached").write_text("regenerable", encoding="utf-8")

  await git_module._remove_local_worktree_artifacts(wt, "thread-sym")

  # The symlink survives cleanup (the remove --force deletes it), its target is
  # untouched, and the real artifact dir is still removed.
  assert (wt / ".venv").is_symlink()
  assert (wt / ".venv").readlink() == outside
  assert (outside / "site.py").exists()
  assert not (wt / ".pixi").exists()


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

  assert not quarantined
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

  assert not quarantined
  assert no_ts.exists()
  assert bad_ts.exists()


@pytest.mark.asyncio
async def test_sweep_survives_non_string_completed_at_and_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  malformed = _make_worktree(parent, "charliebot-task-number-ts")
  old = _make_worktree(parent, "charliebot-task-valid")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg,
      [
          _thread(
              thread_id="number-ts",
              status="failed",
              worktree_path=malformed,
              branch_name="charliebot/task-number-ts",
              completed_at=123),
          _thread(
              thread_id="valid",
              status="failed",
              worktree_path=old,
              branch_name="charliebot/task-valid",
              age_days=20.0),
      ],
  )

  assert quarantined == [str(old)]
  assert malformed.exists()
  assert not old.exists()


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

  assert not quarantined
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

  monkeypatch.setattr(worker_recovery_module, "git_quarantine_worktree", boom)

  # Must not propagate out of startup.
  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=20.0)])
  assert wt.exists()


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


async def _await_recovery_tasks() -> None:
  await await_recovery_tasks(RECOVERY_TASK_PREFIXES)


@pytest.mark.asyncio
async def test_maybe_respawn_verify_task_cross_models_backend_when_omitted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A never-started VERIFY delegation with no explicit --backend must still resolve
  cross-model via model_preference on respawn (mirrors _authorize_spawn_request's VERIFY
  branch in api/internal.py) — not silently fall back to the session's own backend, which
  would defeat verify's "checked by a different model" invariant.
  """
  from src.core import spawner as spawner_module
  from src.core.models import BackendOption, SessionMetadata, TaskType

  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="session-backend", label="S", type="cc-claude", model="s-model"),
          BackendOption(id="other-backend", label="O", type="cc-claude", model="o-model"),
      ],
      model_preference=["other-backend"],
  )

  class FakeSessionManager:

    async def get_session(self, session_id: str) -> SessionMetadata:
      return SessionMetadata(id=session_id, name="s", backend="session-backend")

  captured: dict[str, Any] = {}

  async def fake_spawn_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr, request=None):
    captured["request"] = request

  monkeypatch.setattr(spawner_module, "spawn_worker", fake_spawn_worker)

  meta = {"id": "t1", "session_id": "s1", "description": "verify task", "status": "running", "pid": None}
  item = init_module._InterruptedRun(session_id="s1", thread_dir=tmp_path / "thread", meta=meta)
  chat_events = [
      {
          "type": "task_delegated",
          "thread_id": "t1",
          "delegate_invocation": {
              "task_type": TaskType.VERIFY.value,
              "backend": None,
          },
      },
  ]

  respawned = await init_module._maybe_respawn(
      cfg, FakeSessionManager(), thread_mgr=None, spawner=spawner_module, item=item, chat_events=chat_events)
  await _await_recovery_tasks()

  assert respawned is True
  assert captured["request"].resolved_backend == "other-backend"
  assert captured["request"].resolved_model == "o-model"


@pytest.mark.asyncio
async def test_run_crash_recovery_recovers_and_sweeps(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A never-started pre-boot thread drain-finalizes to failed; the aged failed worktree is swept."""
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

  # boot_time in the future: the no-started_at running thread falls back to its
  # (just-created) dir ctime, which predates boot_time, so it is reconciled.
  # pid None + no raw log -> NEVER_STARTED; with no task_delegated invocation in
  # the chat log it cannot respawn, so it drain-finalizes as failed.
  recovered = await init_module.run_crash_recovery(cfg, utc_now() + timedelta(hours=1))
  await _await_recovery_tasks()

  assert recovered == 1
  meta = json.loads(running_meta.read_text(encoding="utf-8"))
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  # A failed run keeps its worktree (debug state preserved).
  assert running_wt.exists()
  # The aged failed worktree was quarantined.
  assert quarantined == [str(old_wt)]
  assert not old_wt.exists()


def test_scan_skips_post_boot_running_thread(tmp_path: Path) -> None:
  """A worker spawned during the recovery window (started_at > boot_time) is not interrupted."""
  import json

  cfg = _cfg(tmp_path)
  boot_time = utc_now()
  meta_path = _write_thread_meta(
      cfg, "s1", {
          "id": "post-boot",
          "status": "running",
          "pid": 4242,
          "started_at": (boot_time + timedelta(seconds=30)).isoformat(),
      })

  interrupted, threads = init_module._scan_interrupted_runs(cfg, boot_time)

  assert not interrupted
  assert [m["id"] for m in threads] == ["post-boot"]
  # The scan writes nothing: the thread stays running.
  assert json.loads(meta_path.read_text(encoding="utf-8"))["status"] == "running"


@pytest.mark.asyncio
async def test_scan_skips_archived_session_threads(tmp_path: Path) -> None:
  """An archived session's pre-boot thread is not reconciled, yet still feeds the quarantine list."""
  cfg = _cfg(tmp_path)
  await _make_session(cfg, "live")
  await _make_session(cfg, "done")
  started_at = utc_now().isoformat()
  for session_id, thread_id in (("live", "live-thread"), ("done", "archived-thread")):
    _write_thread_meta(cfg, session_id, {"id": thread_id, "status": "running", "pid": 4242, "started_at": started_at})
  await SessionManager(cfg).archive_session("done")

  interrupted, threads = init_module._scan_interrupted_runs(cfg, utc_now() + timedelta(hours=1))

  assert [item.meta["id"] for item in interrupted] == ["live-thread"]
  # Quarantine still sees both: archiving says nothing about reclaiming worktree disk.
  assert sorted(m["id"] for m in threads) == ["archived-thread", "live-thread"]


@pytest.mark.asyncio
async def test_reconcile_pre_boot_run_without_raw_log_kept_alive_when_death_unverifiable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Raw log missing with pid recorded but pid_start absent: death cannot be
  proven, so the run is effective-alive (raw-missing-alive) — reported once,
  never killed, never finalized failed; pid reuse must never finalize an
  innocent run."""
  import json

  from src.core import runs

  cfg = _cfg(tmp_path)
  killed: list[int] = []
  monkeypatch.setattr(worker_recovery_module, "kill_process_group", lambda pid, sig: killed.append(pid))
  boot_time = utc_now()
  await _make_session(cfg, "s1")
  meta_path = _write_thread_meta(
      cfg, "s1", {
          "id": "pre-boot",
          "status": "running",
          "pid": 4242,
          "started_at": (boot_time - timedelta(minutes=5)).isoformat(),
      })

  recovered = await init_module.run_crash_recovery(cfg, boot_time)
  await _await_recovery_tasks()

  assert recovered == 1
  assert not killed
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  assert meta["status"] == "running"
  chat_path = cfg.sessions_dir / "s1" / "data" / "chat_events.jsonl"
  chat_events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  reports = [e for e in chat_events if e.get("source") == "crash_recovery"]
  assert len(reports) == 1
  assert runs.RAW_MISSING_ALIVE_REASON in reports[0]["content"]


@pytest.mark.asyncio
async def test_reconcile_pre_boot_run_without_raw_log_and_verifiable_death_drains_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The unchanged真死回收: raw log missing with the full liveness identity
  recorded and the process proven dead is DIED — drain-finalized failed,
  never killed.

  Reconciliation resolves truth from disk and NEVER kills the recorded pid —
  only leftover descendants holding the raw-log fd are killed, and a missing
  raw log has none.
  """
  import json

  cfg = _cfg(tmp_path)
  killed: list[int] = []
  monkeypatch.setattr(worker_recovery_module, "kill_process_group", lambda pid, sig: killed.append(pid))
  boot_time = utc_now()
  meta_path = _write_thread_meta(
      cfg, "s1", {
          "id": "pre-boot",
          "status": "running",
          "pid": 4242,
          "pid_start": "1",
          "started_at": (boot_time - timedelta(minutes=5)).isoformat(),
      })

  recovered = await init_module.run_crash_recovery(cfg, boot_time)
  await _await_recovery_tasks()

  assert recovered == 1
  assert not killed
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1


@pytest.mark.asyncio
async def test_reconcile_stalled_run_reattaches_reports_and_sends_no_signal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """STALLED (outcome row 4) at the init layer: a run whose recorded pid is alive but
  whose raw log has been silent beyond NO_OUTPUT_REPORT_THRESHOLD is re-attached, never
  killed, and produces a suspected-hang report on the session chat stream.
  """
  import json
  import os
  import subprocess
  import time

  from src.core import runs

  cfg = _cfg(tmp_path)
  boot_time = utc_now() + timedelta(hours=1)  # forces the fresh thread to be treated pre-boot

  await _make_session(cfg, "s1")

  monkeypatch.setattr(runs, "NO_OUTPUT_REPORT_THRESHOLD", 1)
  monkeypatch.setattr(worker_recovery_module, "NO_OUTPUT_REPORT_THRESHOLD", 1)

  killed: list[int] = []
  monkeypatch.setattr(worker_recovery_module, "kill_process_group", lambda pid, sig: killed.append(pid))

  resume_calls: list[bool] = []

  async def fake_resume_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr, *, is_alive,
                               interrupt_reason="", on_silence=None):
    resume_calls.append(is_alive())

  monkeypatch.setattr(SPAWNER_RESUME_WORKER_PATCH_TARGET, fake_resume_worker)

  proc = subprocess.Popen(["sleep", "30"])
  try:
    pid_start, _ = runs.read_pid_stat(proc.pid)  # type: ignore[misc]
    meta_path = _write_thread_meta(
        cfg, "s1", {
            "id": "stalled",
            "status": "running",
            "pid": proc.pid,
            "pid_start": pid_start,
            "started_at": utc_now().isoformat(),
        })
    raw = meta_path.parent / "data" / runs.RAW_LOG_NAME
    raw.parent.mkdir(parents=True, exist_ok=True)
    raw.write_text(
        '{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}}\n',
        encoding="utf-8")
    old_ts = time.time() - 5
    os.utime(raw, (old_ts, old_ts))

    recovered = await init_module.run_crash_recovery(cfg, boot_time)
    await _await_recovery_tasks()

    assert recovered == 1
    assert resume_calls == [True]  # re-attached, following a genuinely alive process
    assert not killed  # never signaled
    assert proc.poll() is None  # still alive after the reconcile returns

    meta = json.loads(meta_path.read_text(encoding="utf-8"))
    assert meta["status"] == "running"  # STALLED is reported, never forced terminal

    chat_path = cfg.sessions_dir / "s1" / "data" / "chat_events.jsonl"
    chat_events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    hang_reports = [
        e for e in chat_events
        if e.get("source") == "crash_recovery" and "Suspected hung" in e.get("content", "")
    ]
    assert len(hang_reports) == 1
  finally:
    proc.kill()
    proc.wait()


def test_scan_missing_started_at_falls_back_to_ctime(tmp_path: Path) -> None:
  """Without started_at, the thread dir ctime decides; an old dir (pre-boot) is interrupted."""
  cfg = _cfg(tmp_path)
  # Thread dir created now; boot_time in the future => dir ctime < boot_time => interrupted.
  _write_thread_meta(cfg, "s1", {"id": "no-start", "status": "running", "pid": None})

  interrupted, _ = init_module._scan_interrupted_runs(cfg, utc_now() + timedelta(hours=1))

  assert [item.meta["id"] for item in interrupted] == ["no-start"]


# ---------------------------------------------------------------------------
# RUNNING_SCAN_WINDOW: stat-before-read gating
# ---------------------------------------------------------------------------


def _spy_on_load_json_meta(monkeypatch: pytest.MonkeyPatch) -> list[Path]:
  """Record every path init.iter_recent_thread_metas actually reads+parses."""
  read_paths: list[Path] = []
  real_load = worker_recovery_module.load_json_meta

  def spy(path: Path, log_event: str, **kwargs: Any) -> Any:
    read_paths.append(Path(path))
    return real_load(path, log_event, **kwargs)

  monkeypatch.setattr(worker_recovery_module, "load_json_meta", spy)
  return read_paths


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
  cfg = _cfg(tmp_path)
  read_paths = _spy_on_load_json_meta(monkeypatch)

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

  interrupted, threads = init_module._scan_interrupted_runs(cfg, boot_time)

  # Only the recent thread is read and reconciled; the stale one is never touched.
  assert recent_path in read_paths
  assert stale_path not in read_paths
  assert [item.meta["id"] for item in interrupted] == ["recent-running"]
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
  cfg = _cfg(tmp_path)
  parent = Path(cfg.worktree_dir)
  band_wt = _make_worktree(parent, "charliebot-task-band")
  recent_wt = _make_worktree(parent, "charliebot-task-recent")
  ancient_wt = _make_worktree(parent, "charliebot-task-ancient")
  quarantined = _install_recording_quarantine(monkeypatch)

  for tid, wt, age in (("band", band_wt, 20.0), ("recent", recent_wt, 3.0), ("ancient", ancient_wt, 40.0)):
    meta_path = _write_thread_meta(
        cfg, "s1",
        _thread(
            thread_id=tid, status="failed", worktree_path=wt, branch_name=f"charliebot/task-{tid}", age_days=age))
    _age_metadata_mtime(meta_path, age)

  await init_module.run_crash_recovery(cfg, utc_now())

  assert quarantined == [str(band_wt)]
  assert not band_wt.exists()
  assert recent_wt.exists()
  assert ancient_wt.exists()
