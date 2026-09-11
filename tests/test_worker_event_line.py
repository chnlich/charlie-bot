"""Worker event-line serialization: the persisted log line parses back to the event.

Worker._event_line is the single serialization home for every events-log append;
orjson renders compact UTF-8 where the stdlib form emitted spaces and \\uXXXX
escapes, and every reader JSON-parses the log per line.
"""

import json
import os
from pathlib import Path
from unittest.mock import AsyncMock

import pytest

import src.agents.worker as worker_mod
from src.agents.worker import Worker, _event_line
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata


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


async def _process(worker: Worker, tmp_path: Path, event: dict, monkeypatch) -> str:
  monkeypatch.setattr(worker_mod.streaming_manager, "broadcast", AsyncMock())
  fd = os.open(tmp_path / "events.jsonl", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    await worker._process_event(event, fd)
  finally:
    os.close(fd)
  return (tmp_path / "events.jsonl").read_text(encoding="utf-8")


@pytest.mark.asyncio
async def test_process_event_persists_a_line_that_parses_back(tmp_path: Path, monkeypatch) -> None:
  worker = Worker(
      ThreadMetadata.model_construct(id="event-line"),
      tmp_path,
      tmp_path / "events.jsonl",
      "",
      CharlieBotConfig(charliebot_home=tmp_path / "home"),
  )
  event = {"type": "user", "message": {"content": [{"type": "text", "text": "café ✓"}]}}
  text = await _process(worker, tmp_path, event, monkeypatch)
  lines = text.splitlines()
  assert len(lines) == 1 and json.loads(lines[0]) == event
