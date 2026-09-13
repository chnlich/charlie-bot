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
async def test_process_event_persists_the_attach_signal_without_broadcasting_it(tmp_path: Path, monkeypatch) -> None:
  """The typed adoption signal is the worker log's session-id record (the token
  tally's codex reconciliation reads it from the raw line) but no thread
  subscriber reads it and the projection skips it, so it appends exactly one
  typed line and broadcasts nothing."""
  worker = make_worker(tmp_path, "attach-signal")
  monkeypatch.setattr(worker_module.streaming_manager, "broadcast", AsyncMock())
  event = {"type": ET.SESSION_ATTACHED, "session_id": "oc-s-1"}
  lines = (await process_worker_event(worker, tmp_path, event, monkeypatch)).splitlines()
  assert len(lines) == 1
  persisted = json.loads(lines[0])
  assert persisted["type"] == ET.SESSION_ATTACHED
  assert persisted["session_id"] == "oc-s-1"
  assert persisted["timestamp"]
  # process_worker_event installs its own broadcast seam mock; the funnel must
  # have left it uncalled (the live mock, not the test's pre-installed one).
  assert worker_module.streaming_manager.broadcast.await_count == 0
