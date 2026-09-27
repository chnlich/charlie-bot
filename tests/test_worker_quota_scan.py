"""Worker quota scan: the payload lowercase copies ride ERROR events alone.

Worker._process_event's quota-pattern check reads an event's message and content
payloads; the copies are gated on the event type, so the streamed-turn path pays
the full-payload stringification only where the pattern set can match.
"""

import json
from pathlib import Path

import pytest
from conftest import make_worker, process_worker_event

from src.agents.worker import QuotaExhaustedError
from src.core import event_types as ET


@pytest.mark.asyncio
async def test_error_event_with_quota_pattern_raises_and_persists(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  event = {"type": ET.ERROR, "message": "API Error: quota exceeded for project", "content": ""}
  with pytest.raises(QuotaExhaustedError):
    await process_worker_event(make_worker(tmp_path, "quota-scan"), tmp_path, event, monkeypatch)
  lines = (tmp_path / "events.jsonl").read_text(encoding="utf-8").splitlines()
  assert len(lines) == 1 and json.loads(lines[0])["message"] == event["message"]
