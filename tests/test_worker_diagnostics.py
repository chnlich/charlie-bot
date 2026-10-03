"""Tests that Worker writes hang_diagnostics.json + emits a system event."""
from __future__ import annotations

import json
import pathlib
from collections.abc import AsyncIterator
from unittest import mock

import conftest
import pytest

from src.agents.backends import base

# The tests bind a local `worker`: `from src.agents import worker` would turn
# the `worker.Worker(...)` calls below into an UnboundLocalError.
from src.agents.worker import Worker
from src.core import config, models


class _FakeBackend(base.AgentBackend):
  """In-process fake backend that pre-populates hang_diagnostics and skips subprocess work."""

  def __init__(self, *, exit_code: int = 0, hang_diagnostics: dict | None = None, **kwargs: object) -> None:
    super().__init__(**kwargs)
    self.exit_code = exit_code
    self.hang_diagnostics = hang_diagnostics
    self.stderr_text = ""

  def _build_command(self, prompt: str) -> list[str]:  # pragma: no cover - never spawned
    return ["true"]

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    if self._on_spawn is not None:
      await self._on_spawn(12345)
    if False:  # pragma: no cover - keeps method an async generator
      yield {}


@pytest.mark.asyncio
async def test_worker_writes_hang_diagnostics_and_emits_event(tmp_path: pathlib.Path) -> None:
  thread = models.ThreadMetadata(session_id="sess-1", description="test")
  events_log = tmp_path / "events.jsonl"
  fake_diag = {"captured_at": "2026-05-03T00:00:00+00:00", "pid": 12345, "process_tree": "fake-tree"}
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "cb-home")

  worker = Worker(
      thread_metadata=thread,
      working_dir=tmp_path,
      events_log_path=events_log,
      task_description="ignored",
      cfg=cfg,
  )

  fake_backend = _FakeBackend(exit_code=143, hang_diagnostics=fake_diag)

  with (
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as mock_broadcast,
      mock.patch(conftest.WORKER_BUILD_BACKEND_PATCH_TARGET, return_value=fake_backend),
      mock.patch(conftest.WORKER_CLAUDE_CODE_BACKEND_PATCH_TARGET, return_value=fake_backend),
  ):
    exit_code = await worker.run()

  assert exit_code == 143
  diag_path = events_log.parent / "hang_diagnostics.json"
  assert diag_path.exists()
  written = json.loads(diag_path.read_text(encoding="utf-8"))
  assert written == fake_diag

  events_lines = [json.loads(line) for line in events_log.read_text().splitlines() if line.strip()]
  diag_events = [e for e in events_lines if e.get("type") == "system" and e.get("subtype") == "hang_diagnostics"]
  assert len(diag_events) == 1
  diag_event = diag_events[0]
  assert diag_event["diagnostics_path"] == str(diag_path)
  assert diag_event["exit_code"] == 143
  assert "timestamp" in diag_event

  broadcast_payloads = [c.args[1] for c in mock_broadcast.await_args_list]
  assert any(e.get("type") == "system" and e.get("subtype") == "hang_diagnostics" for e in broadcast_payloads)


@pytest.mark.asyncio
async def test_worker_no_hang_diagnostics_does_not_write_file(tmp_path: pathlib.Path) -> None:
  thread = models.ThreadMetadata(session_id="sess-1", description="test")
  events_log = tmp_path / "events.jsonl"
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "cb-home")

  worker = Worker(
      thread_metadata=thread,
      working_dir=tmp_path,
      events_log_path=events_log,
      task_description="ignored",
      cfg=cfg,
  )

  fake_backend = _FakeBackend(exit_code=0, hang_diagnostics=None)

  with (
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()),
      mock.patch(conftest.WORKER_BUILD_BACKEND_PATCH_TARGET, return_value=fake_backend),
      mock.patch(conftest.WORKER_CLAUDE_CODE_BACKEND_PATCH_TARGET, return_value=fake_backend),
  ):
    exit_code = await worker.run()

  assert exit_code == 0
  assert not (events_log.parent / "hang_diagnostics.json").exists()
