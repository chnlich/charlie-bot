"""Worker event-line serialization: the persisted log line parses back to the event.

Worker._event_line is the single serialization home for every events-log append;
orjson renders compact UTF-8 where the stdlib form emitted spaces and \\uXXXX
escapes, and every reader JSON-parses the log per line.
"""

import json
from pathlib import Path

import pytest
from conftest import make_worker, process_worker_event

from src.agents.worker import _event_line


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
