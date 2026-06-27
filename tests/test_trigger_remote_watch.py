"""Unit tests for remote PID watching via ssh polling (Tool 2)."""
from __future__ import annotations

import asyncio
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from pydantic import ValidationError

from src.cli import schedule_trigger as cli_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, LocalPid, RemotePid, ScheduleTriggerRequest, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import (
    RemoteVerifyError,
    TriggerManager,
    _migrate_legacy_watch_pids,
)


# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


class _FakeProc:
  """Stand-in for asyncio.subprocess.Process."""

  def __init__(self, stdout: bytes, stderr: bytes = b"", returncode: int = 0) -> None:
    self._stdout = stdout
    self._stderr = stderr
    self.returncode = returncode
    self.kill_called = False

  async def communicate(self) -> tuple[bytes, bytes]:
    return self._stdout, self._stderr

  def kill(self) -> None:
    self.kill_called = True

  async def wait(self) -> int:
    return self.returncode


def _mk_subprocess_mock(scripted: dict[tuple[str, int], list[str]]) -> AsyncMock:
  """Build a mock for ``asyncio.create_subprocess_exec``.

  ``scripted`` maps (host, pid) -> list of statuses ("ALIVE" / "DEAD"). Each
  call pops the next entry; the last entry is repeated indefinitely.
  """

  async def _factory(*args, **kwargs):  # noqa: ARG001
    # Extract host and `kill -0 PID 2>&1 ...` payload from cmd.
    # Layout: ssh -o BatchMode=yes -o ConnectTimeout=10 HOST "kill -0 PID ..."
    host = args[5]
    payload = args[6]
    pid = int(payload.split()[2])
    queue = scripted[(host, pid)]
    status = queue[0] if len(queue) == 1 else queue.pop(0)
    return _FakeProc(stdout=(status + "\n").encode())

  return AsyncMock(side_effect=_factory)


async def _make_mgr(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, str]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Remote watch"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  return cfg, session_mgr, trigger_mgr, session.id


# ---------------------------------------------------------------------------
# Verify-on-create
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_create_alive_persists(tmp_path: Path) -> None:
  cfg, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  scripted = {("neptune", 1234): ["ALIVE"]}

  with (
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)),
      patch("src.core.triggers.trigger_master", new=AsyncMock()),
      patch.object(TriggerManager, "_start_task", lambda self, t: None),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="remote watch",
        watch_targets=[RemotePid(host="neptune", pid=1234)],
    )

  # File on disk has the new schema.
  raw = (cfg.sessions_dir / session_id / "triggers" / f"{trigger.id}.json").read_text("utf-8")
  data = json.loads(raw)
  assert data["watch_targets"] == [{"kind": "remote_pid", "host": "neptune", "pid": 1234}]
  assert "watch_pids" not in data


@pytest.mark.asyncio
async def test_remote_create_dead_rejects(tmp_path: Path) -> None:
  cfg, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  scripted = {("neptune", 1234): ["DEAD"]}

  with patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)):
    with pytest.raises(RemoteVerifyError) as excinfo:
      await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=30,
          message="dead remote",
          watch_targets=[RemotePid(host="neptune", pid=1234)],
      )

  assert "neptune:1234" in str(excinfo.value)
  # Nothing was persisted.
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  assert not triggers_dir.exists() or not list(triggers_dir.glob("*.json"))


@pytest.mark.asyncio
async def test_remote_create_one_dead_among_many_rejects(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  scripted = {
      ("neptune", 1): ["ALIVE"],
      ("neptune", 2): ["DEAD"],
      ("noire", 3): ["ALIVE"],
  }
  with patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)):
    with pytest.raises(RemoteVerifyError) as excinfo:
      await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=30,
          message="dead remote",
          watch_targets=[
              RemotePid(host="neptune", pid=1),
              RemotePid(host="neptune", pid=2),
              RemotePid(host="noire", pid=3),
          ],
      )
  msg = str(excinfo.value)
  assert "neptune:2" in msg
  assert "DEAD" in msg


# ---------------------------------------------------------------------------
# Wait loop with remote probe — ALL-die fire across hosts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_multi_host_all_die_fires(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  # All ALIVE on first poll (verify + first iter), then all DEAD on second iter.
  scripted = {
      ("neptune", 1): ["ALIVE", "ALIVE", "DEAD"],
      ("neptune", 2): ["ALIVE", "ALIVE", "DEAD"],
      ("noire", 3): ["ALIVE", "ALIVE", "DEAD"],
  }

  # Make sleeps instant so the test runs fast.
  async def _no_sleep(_seconds: float) -> None:
    return None

  with (
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)),
      patch("src.core.triggers.asyncio.sleep", new=_no_sleep),
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="multi host",
        watch_targets=[
            RemotePid(host="neptune", pid=1),
            RemotePid(host="neptune", pid=2),
            RemotePid(host="noire", pid=3),
        ],
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert "neptune:1" in msg
  assert "neptune:2" in msg
  assert "noire:3" in msg


@pytest.mark.asyncio
async def test_remote_timeout_with_alive_pids(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  # Verify says ALIVE; subsequent probes also ALIVE → must timeout.
  scripted = {("neptune", 1): ["ALIVE"]}

  async def _no_sleep(_seconds: float) -> None:
    return None

  with (
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)),
      patch("src.core.triggers.asyncio.sleep", new=_no_sleep),
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,  # fire_at already in the past after verify
        message="too late",
        watch_targets=[RemotePid(host="neptune", pid=1)],
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "timeout"
  msg = mock_master.await_args.args[1]
  assert "still alive: neptune:1" in msg


# ---------------------------------------------------------------------------
# Backoff schedule
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_backoff_intervals_and_plateau(tmp_path: Path) -> None:
  """Each iteration sleeps for [base, base+10] where base follows the backoff schedule."""
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  recorded_sleeps: list[float] = []
  real_sleep = asyncio.sleep

  async def _record_sleep(seconds: float) -> None:
    # The probe loop only ever sleeps with values >= 10s (backoff base) up to
    # max-wait. Forward true zero-sleeps (test-yield helpers) to the real
    # asyncio.sleep so the patch doesn't leak globally and the test can yield.
    if seconds <= 0:
      await real_sleep(seconds)
      return
    recorded_sleeps.append(seconds)
    # Don't actually sleep — return immediately so iterations advance fast.

  scripted = {("neptune", 1): ["ALIVE"]}  # always alive — never exits

  with (
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=_mk_subprocess_mock(scripted)),
      patch("src.core.triggers.asyncio.sleep", new=_record_sleep),
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=10_000,
        message="long wait",
        watch_targets=[RemotePid(host="neptune", pid=1)],
    )
    task = trigger_mgr._tasks[trigger.id]
    for _ in range(200):
      if len(recorded_sleeps) >= 8:
        break
      await real_sleep(0)
    task.cancel()
    try:
      await task
    except (asyncio.CancelledError, BaseException):
      pass

  expected_bases = [10, 20, 40, 80, 160, 320, 600, 600]
  assert len(recorded_sleeps) >= len(expected_bases), (
      f"only recorded {len(recorded_sleeps)} sleeps: {recorded_sleeps}"
  )
  for i, base in enumerate(expected_bases):
    assert base <= recorded_sleeps[i] <= base + 10, (
        f"sleep[{i}]={recorded_sleeps[i]} not in [{base}, {base + 10}]"
    )


# ---------------------------------------------------------------------------
# Migration: legacy `watch_pids` JSON file -> rewritten in new schema
# ---------------------------------------------------------------------------


def test_migrate_legacy_watch_pids_helper() -> None:
  legacy = json.dumps(
      {
          "id": "leg-1",
          "session_id": "s1",
          "fire_at": "2030-01-01T00:00:00+00:00",
          "message": "hi",
          "created_at": "2030-01-01T00:00:00+00:00",
          "status": "pending",
          "fired_at": None,
          "watch_pids": [123, 456],
      }
  )
  trigger, migrated = _migrate_legacy_watch_pids(legacy)
  assert migrated is True
  assert trigger.watch_targets == [LocalPid(pid=123), LocalPid(pid=456)]


def test_migrate_legacy_watch_pids_none_value() -> None:
  legacy = json.dumps(
      {
          "id": "leg-2",
          "session_id": "s1",
          "fire_at": "2030-01-01T00:00:00+00:00",
          "message": "hi",
          "created_at": "2030-01-01T00:00:00+00:00",
          "status": "pending",
          "fired_at": None,
          "watch_pids": None,
      }
  )
  trigger, migrated = _migrate_legacy_watch_pids(legacy)
  assert migrated is True
  assert trigger.watch_targets == []


def test_migrate_legacy_no_op_when_already_new_schema() -> None:
  modern = json.dumps(
      {
          "id": "new-1",
          "session_id": "s1",
          "fire_at": "2030-01-01T00:00:00+00:00",
          "message": "hi",
          "created_at": "2030-01-01T00:00:00+00:00",
          "status": "pending",
          "fired_at": None,
          "watch_targets": [{"kind": "remote_pid", "host": "neptune", "pid": 7}],
      }
  )
  trigger, migrated = _migrate_legacy_watch_pids(modern)
  assert migrated is False
  assert trigger.watch_targets == [RemotePid(host="neptune", pid=7)]


def test_migrate_backfills_kind_on_kindless_targets() -> None:
  """Pre-discriminator watch_targets (no `kind`) get LOCAL/REMOTE backfilled."""
  legacy = json.dumps(
      {
          "id": "kindless-1",
          "session_id": "s1",
          "fire_at": "2030-01-01T00:00:00+00:00",
          "message": "hi",
          "created_at": "2030-01-01T00:00:00+00:00",
          "status": "pending",
          "fired_at": None,
          "watch_targets": [{"host": None, "pid": 1}, {"host": "neptune", "pid": 2}],
      }
  )
  trigger, migrated = _migrate_legacy_watch_pids(legacy)
  assert migrated is True
  assert trigger.watch_targets == [LocalPid(pid=1), RemotePid(host="neptune", pid=2)]


@pytest.mark.asyncio
async def test_recover_pending_rewrites_legacy_file(tmp_path: Path) -> None:
  cfg, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  triggers_dir.mkdir(parents=True, exist_ok=True)

  far_future = (datetime.now(timezone.utc) + timedelta(days=365)).isoformat()
  legacy_path = triggers_dir / "legacy-x.json"
  legacy_path.write_text(
      json.dumps(
          {
              "id": "legacy-x",
              "session_id": session_id,
              "fire_at": far_future,
              "message": "old style",
              "created_at": far_future,
              "status": "pending",
              "fired_at": None,
              "watch_pids": [777],
          }
      ),
      encoding="utf-8",
  )

  # Skip the actual sleep task on recovery — we only care about the rewrite.
  with patch.object(TriggerManager, "_start_task", lambda self, t: None):
    await trigger_mgr.recover_pending()

  rewritten = json.loads(legacy_path.read_text("utf-8"))
  assert "watch_pids" not in rewritten
  assert rewritten["watch_targets"] == [{"kind": "local_pid", "pid": 777}]


# ---------------------------------------------------------------------------
# CLI parsing — self-describing --watch specs (local / remote / slurm)
# ---------------------------------------------------------------------------


def test_cli_parse_local_only(monkeypatch) -> None:
  ns = cli_module._parse_watch_target("12345")
  assert ns == {"kind": "local_pid", "pid": 12345}


def test_cli_parse_remote_host_pid() -> None:
  ns = cli_module._parse_watch_target("neptune:67890")
  assert ns == {"kind": "remote_pid", "host": "neptune", "pid": 67890}


def test_cli_parse_slurm_job() -> None:
  ns = cli_module._parse_watch_target("slurm:98765")
  assert ns == {"kind": "slurm_job", "job_id": 98765}


def test_cli_parse_rejects_bad_pid() -> None:
  import argparse
  for bad in ("neptune:abc", ":123", "0", "slurm:abc", "slurm:0", "slurm:"):
    with pytest.raises(argparse.ArgumentTypeError):
      cli_module._parse_watch_target(bad)


def test_cli_accepts_mixed_kinds(monkeypatch) -> None:
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--max-wait", "60",
      "--message", "m",
      "--watch", "1234", "neptune:5678", "slurm:99",
  ]
  captured: dict = {}

  class _FakeResp:
    status_code = 200
    ok = True

    def json(self) -> dict:
      return {"trigger_id": "t1", "fire_at": "2030-01-01T00:00:00+00:00"}

  def _fake_post(url, json, headers, timeout, verify):  # noqa: A002, ARG001
    captured["payload"] = json
    return _FakeResp()

  monkeypatch.setattr(cli_module.requests, "post", _fake_post)
  with patch.object(sys, "argv", argv):
    cli_module.main()

  assert captured["payload"]["watch_targets"] == [
      {"kind": "local_pid", "pid": 1234},
      {"kind": "remote_pid", "host": "neptune", "pid": 5678},
      {"kind": "slurm_job", "job_id": 99},
  ]


def test_cli_watch_pid_flag_removed() -> None:
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--max-wait", "60",
      "--message", "m",
      "--watch-pid", "1234",
  ]
  with patch.object(sys, "argv", argv):
    with pytest.raises(SystemExit):
      cli_module.main()


def test_cli_renamed_flag_delay_no_longer_accepted() -> None:
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--delay", "60",
      "--message", "m",
  ]
  with patch.object(sys, "argv", argv):
    with pytest.raises(SystemExit):
      cli_module.main()


def test_cli_max_wait_accepted(monkeypatch) -> None:
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--max-wait", "60",
      "--message", "hello",
  ]
  captured: dict = {}

  class _FakeResp:
    status_code = 200
    ok = True

    def json(self) -> dict:
      return {"trigger_id": "t1", "fire_at": "2030-01-01T00:00:00+00:00"}

  def _fake_post(url, json, headers, timeout, verify):  # noqa: A002, ARG001
    captured["url"] = url
    captured["payload"] = json
    return _FakeResp()

  monkeypatch.setattr(cli_module.requests, "post", _fake_post)
  with patch.object(sys, "argv", argv):
    cli_module.main()

  assert captured["payload"] == {
      "session_id": "s1",
      "delay_seconds": 60,
      "message": "hello",
  }


def test_cli_remote_dead_exits_with_code_2(monkeypatch) -> None:
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--max-wait", "60",
      "--message", "hello",
      "--watch", "neptune:5678",
  ]

  class _FakeResp:
    status_code = 422
    ok = False
    text = ""

    def json(self) -> dict:
      return {"detail": "verify-on-create failed for remote watch target(s): neptune:5678 -> DEAD ('DEAD\\n')"}

  def _fake_post(url, json, headers, timeout, verify):  # noqa: A002, ARG001
    return _FakeResp()

  monkeypatch.setattr(cli_module.requests, "post", _fake_post)
  with patch.object(sys, "argv", argv):
    with pytest.raises(SystemExit) as excinfo:
      cli_module.main()

  assert excinfo.value.code == cli_module.EXIT_VERIFY_REJECTED


def test_schedule_trigger_request_rejects_legacy_watch_pids() -> None:
  with pytest.raises(ValidationError):
    ScheduleTriggerRequest(
        session_id="s1",
        delay_seconds=60,
        message="legacy",
        watch_pids=[1234],
    )
