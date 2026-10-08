"""Unit tests for remote PID watching via ssh polling (Tool 2)."""
from __future__ import annotations

import json
import pathlib
import sys
from collections.abc import Callable
from typing import Any
from unittest import mock

import conftest
import pytest

from src.infra import models
from src.runtime import triggers
from src.runtime.cli import schedule_trigger as cli_module

# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


def _mk_subprocess_mock(scripted: dict[tuple[str, int], list[str]]) -> mock.AsyncMock:
  """Build a mock for ``asyncio.create_subprocess_exec``.

  ``scripted`` maps (host, pid) -> list of statuses ("ALIVE" / "DEAD"). Each
  call pops the next entry; the last entry is repeated indefinitely.
  """

  async def _factory(*args: Any, **kwargs: Any) -> conftest.FakeAsyncProcess:
    # Extract host and `kill -0 PID 2>&1 ...` payload from cmd.
    # Layout: ssh -o <pairs...> HOST "kill -0 PID ..."; the host is the last
    # bare word before the quoted remote command.
    host = args[-2]
    payload = args[-1]
    pid = int(payload.split()[2])
    queue = scripted[(host, pid)]
    status = queue[0] if len(queue) == 1 else queue.pop(0)
    return conftest.FakeAsyncProcess(stdout=(status + "\n").encode())

  return mock.AsyncMock(side_effect=_factory)


# ---------------------------------------------------------------------------
# Verify-on-create
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_create_alive_persists(tmp_path: pathlib.Path) -> None:
  cfg, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  scripted = {("neptune", 1234): ["ALIVE"]}

  with (
      mock.patch(conftest.TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=_mk_subprocess_mock(scripted)),
      mock.patch(conftest.TRIGGER_TASK_DELIVERY_PATCH_TARGET, new=mock.AsyncMock()),
      mock.patch.object(triggers.TriggerManager, "_start_task", lambda self, t: None),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="remote watch",
        watch_targets=[models.RemotePid(host="neptune", pid=1234)],
    )

  # File on disk has the new schema.
  raw = (cfg.sessions_dir / session_id / "triggers" / f"{trigger.id}.json").read_text("utf-8")
  data = json.loads(raw)
  assert data["watch_targets"] == [{"kind": "remote_pid", "host": "neptune", "pid": 1234}]
  assert "watch_pids" not in data


@pytest.mark.asyncio
async def test_remote_create_dead_rejects(tmp_path: pathlib.Path) -> None:
  cfg, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  scripted = {("neptune", 1234): ["DEAD"]}

  with (
      mock.patch(conftest.TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=_mk_subprocess_mock(scripted)),
      pytest.raises(triggers.RemoteVerifyError) as excinfo,
  ):
    await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="dead remote",
        watch_targets=[models.RemotePid(host="neptune", pid=1234)],
    )

  assert "neptune:1234" in str(excinfo.value)
  # Nothing was persisted.
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  assert not triggers_dir.exists() or not list(triggers_dir.glob("*.json"))


# ---------------------------------------------------------------------------
# CLI parsing — self-describing --watch specs (local / remote / slurm)
# ---------------------------------------------------------------------------


def _fake_200_post(captured: dict) -> Callable[..., Any]:

  class _FakeResp:
    status_code = 200

    def json(self) -> dict:
      return {"trigger_id": "t1", "fire_at": "2030-01-01T00:00:00+00:00"}

  def _fake_post(
      url: str,
      json: dict | None = None,
      params: dict | None = None,
      headers: dict | None = None,
      timeout: float | None = None) -> _FakeResp:
    captured["url"] = url
    captured["payload"] = json
    return _FakeResp()

  return _fake_post


def test_cli_accepts_mixed_kinds(monkeypatch: pytest.MonkeyPatch) -> None:
  argv = conftest.schedule_trigger_argv("m", "--watch", "1234", "neptune:5678", "slurm:99")
  captured: dict = {}
  conftest.fake_cli_cfg(monkeypatch, pathlib.Path("/nonexistent-sessions"))

  monkeypatch.setattr(conftest.CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, _fake_200_post(captured))
  with mock.patch.object(sys, "argv", argv):
    cli_module.main()

  assert captured["payload"]["watch_targets"] == [
      {
          "kind": "local_pid",
          "pid": 1234
      },
      {
          "kind": "remote_pid",
          "host": "neptune",
          "pid": 5678
      },
      {
          "kind": "slurm_job",
          "job_id": 99
      },
  ]
