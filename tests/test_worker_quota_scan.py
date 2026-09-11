"""Worker quota scan: the payload lowercase copies ride ERROR events alone.

Worker._process_event's quota-pattern check reads an event's message and content
payloads; the copies are gated on the event type, so the streamed-turn path pays
the full-payload stringification only where the pattern set can match.
"""

import json
import os
from pathlib import Path
from unittest.mock import AsyncMock

import pytest

import src.agents.worker as worker_mod
from src.agents.worker import QuotaExhaustedException, Worker
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata


def _worker(tmp_path: Path) -> Worker:
  return Worker(
      ThreadMetadata.model_construct(id="quota-scan"),
      tmp_path,
      tmp_path / "events.jsonl",
      "",
      CharlieBotConfig(charliebot_home=tmp_path / "home"),
  )


async def _process(worker: Worker, tmp_path: Path, event: dict, monkeypatch) -> None:
  monkeypatch.setattr(worker_mod.streaming_manager, "broadcast", AsyncMock())
  fd = os.open(tmp_path / "events.jsonl", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    await worker._process_event(event, fd)
  finally:
    os.close(fd)


@pytest.mark.asyncio
async def test_error_event_with_quota_pattern_raises_and_persists(tmp_path: Path, monkeypatch) -> None:
  event = {"type": ET.ERROR, "message": "API Error: quota exceeded for project", "content": ""}
  with pytest.raises(QuotaExhaustedException):
    await _process(_worker(tmp_path), tmp_path, event, monkeypatch)
  lines = (tmp_path / "events.jsonl").read_text(encoding="utf-8").splitlines()
  assert len(lines) == 1 and json.loads(lines[0])["message"] == event["message"]


@pytest.mark.asyncio
async def test_error_event_without_quota_pattern_passes(tmp_path: Path, monkeypatch) -> None:
  event = {"type": ET.ERROR, "message": "tool schema rejected", "content": ""}
  await _process(_worker(tmp_path), tmp_path, event, monkeypatch)
  assert len((tmp_path / "events.jsonl").read_text(encoding="utf-8").splitlines()) == 1


@pytest.mark.asyncio
async def test_non_error_payload_never_raises(tmp_path: Path, monkeypatch) -> None:
  # The loop's heaviest payload shape: a user event whose tool_result content
  # carries the patterns verbatim. The type gate alone answers, both before and
  # after the copies run.
  event = {
      "type": ET.USER,
      "message": {
          "content": [{
              "type": "tool_result",
              "content": "HTTP 429: rate limit, quota exceeded"
          }]
      },
  }
  await _process(_worker(tmp_path), tmp_path, event, monkeypatch)
  assert len((tmp_path / "events.jsonl").read_text(encoding="utf-8").splitlines()) == 1
