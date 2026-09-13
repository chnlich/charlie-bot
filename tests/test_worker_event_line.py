"""Worker event-line serialization: the persisted log line parses back to the event.

Worker._event_line is the single serialization home for every events-log append;
orjson renders compact UTF-8 where the stdlib form emitted spaces and \\uXXXX
escapes, and every reader JSON-parses the log per line.
"""

import json
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import make_worker, process_worker_event

from src.agents import worker as worker_module
from src.agents.worker import _event_line
from src.core import event_types as ET


def test_event_line_round_trips_the_event() -> None:
  event = {
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "概要 — 100% done"
          }],
          "usage": {
              "input_tokens": 10
          }
      },
      "timestamp": "2026-09-11T00:00:00+00:00",
  }
  assert json.loads(_event_line(event)) == event


@pytest.mark.asyncio
async def test_process_event_persists_a_line_that_parses_back(tmp_path: Path, monkeypatch) -> None:
  worker = make_worker(tmp_path, "event-line")
  event = {"type": "user", "message": {"content": [{"type": "text", "text": "café ✓"}]}}
  lines = (await process_worker_event(worker, tmp_path, event, monkeypatch)).splitlines()
  assert len(lines) == 1 and json.loads(lines[0]) == event


@pytest.mark.asyncio
async def test_process_event_never_persists_the_session_attach_signal(tmp_path: Path, monkeypatch) -> None:
  """The run-start adoption signal carries no renderable content: it appends no
  worker-log line and broadcasts nothing, so no read of the log ever pays the
  WorkerEvent validation failure a type-less line forces."""
  worker = make_worker(tmp_path, "attach-signal")
  broadcast = AsyncMock()
  monkeypatch.setattr(worker_module.streaming_manager, "broadcast", broadcast)
  event = {"type": ET.SESSION_ATTACHED, "session_id": "oc-s-1"}
  lines = (await process_worker_event(worker, tmp_path, event, monkeypatch)).splitlines()
  assert lines == []
  broadcast.assert_not_called()
