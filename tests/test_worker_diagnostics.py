"""Tests that Worker writes hang_diagnostics.json + emits a system event."""
from __future__ import annotations

import json
from collections.abc import AsyncIterator
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.agents.backends.base import AgentBackend
from src.agents.worker import Worker
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata


class _FakeBackend(AgentBackend):
  """In-process fake backend that pre-populates hang_diagnostics and skips subprocess work."""

  def __init__(self, *, exit_code: int = 0, hang_diagnostics: dict | None = None, **kwargs) -> None:
    super().__init__(**kwargs)
    self.exit_code = exit_code
    self.hang_diagnostics = hang_diagnostics
    self.stderr_text = ""

  def _build_command(self, prompt: str) -> list[str]:  # pragma: no cover - never spawned
    return ["true"]

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:  # type: ignore[override]
    if self._on_spawn is not None:
      await self._on_spawn(12345)
    if False:  # pragma: no cover - keeps method an async generator
      yield {}


@pytest.mark.asyncio
async def test_worker_writes_hang_diagnostics_and_emits_event(tmp_path: Path) -> None:
  thread = ThreadMetadata(session_id="sess-1", description="test")
  events_log = tmp_path / "events.jsonl"
  fake_diag = {"captured_at": "2026-05-03T00:00:00+00:00", "pid": 12345, "process_tree": "fake-tree"}
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "cb-home")

  worker = Worker(
      thread_metadata=thread,
      working_dir=tmp_path,
      events_log_path=events_log,
      task_description="ignored",
      cfg=cfg,
  )

  fake_backend = _FakeBackend(exit_code=143, hang_diagnostics=fake_diag)

  with (
      patch("src.agents.worker.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.agents.worker.build_backend", return_value=fake_backend),
      patch("src.agents.worker.ClaudeCodeBackend", return_value=fake_backend),
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
async def test_worker_no_hang_diagnostics_does_not_write_file(tmp_path: Path) -> None:
  thread = ThreadMetadata(session_id="sess-1", description="test")
  events_log = tmp_path / "events.jsonl"
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "cb-home")

  worker = Worker(
      thread_metadata=thread,
      working_dir=tmp_path,
      events_log_path=events_log,
      task_description="ignored",
      cfg=cfg,
  )

  fake_backend = _FakeBackend(exit_code=0, hang_diagnostics=None)

  with (
      patch("src.agents.worker.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.agents.worker.build_backend", return_value=fake_backend),
      patch("src.agents.worker.ClaudeCodeBackend", return_value=fake_backend),
  ):
    exit_code = await worker.run()

  assert exit_code == 0
  assert not (events_log.parent / "hang_diagnostics.json").exists()
