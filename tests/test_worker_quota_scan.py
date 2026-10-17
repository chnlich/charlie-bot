"""Worker quota scan: the payload lowercase copies ride ERROR events alone.

Worker._process_event's quota-pattern check reads an event's message and content
payloads; the copies are gated on the event type, so the streamed-turn path pays
the full-payload stringification only where the pattern set can match.
"""

import datetime
import json
import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.runtime import worker


@pytest.mark.asyncio
async def test_error_event_with_quota_pattern_raises_and_persists(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  event = {"type": ET.ERROR, "message": "API Error: quota exceeded for project", "content": ""}
  with pytest.raises(worker.QuotaExhaustedError):
    await conftest.process_worker_event(conftest.make_worker(tmp_path, "quota-scan"), tmp_path, event, monkeypatch)
  lines = (tmp_path / "events.jsonl").read_text(encoding="utf-8").splitlines()
  assert len(lines) == 1 and json.loads(lines[0])["message"] == event["message"]


@pytest.mark.asyncio
async def test_a_rejected_rate_limit_event_without_a_relay_watch_raises_with_its_reset(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A rejection no relay owns ends the run on quota; its resetsAt rides the error so the pool
    cools the member until that moment."""
  resets_at = 1893512400  # 2030-01-01T05:00:00Z
  event = {
      "type": ET.RATE_LIMIT_EVENT,
      "rate_limit_info": {
          "status": "rejected",
          "rateLimitType": "five_hour",
          "resetsAt": resets_at
      },
  }
  with pytest.raises(worker.QuotaExhaustedError) as exc_info:
    await conftest.process_worker_event(conftest.make_worker(tmp_path, "rl-scan"), tmp_path, event, monkeypatch)
  assert exc_info.value.resets_at == datetime.datetime.fromtimestamp(resets_at, datetime.UTC)


@pytest.mark.asyncio
async def test_a_rejected_rate_limit_event_under_a_relay_watch_never_raises(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A single-account rejection relays inside the run: the watch observes it and no quota error
    escapes, so the Claude member stays available (only a full account-pool exhaustion cools it)."""

  class RelayWatch:
    """The lifecycle's relay watch double: it observes every event and never asks to terminate."""

    def __init__(self) -> None:
      self.seen: list[dict] = []

    def observe(self, event_data: dict) -> bool:
      self.seen.append(event_data)
      return False

  event = {
      "type": ET.RATE_LIMIT_EVENT,
      "rate_limit_info": {
          "status": "rejected",
          "rateLimitType": "five_hour",
          "resetsAt": 1893512400
      },
  }
  relay_watch = RelayWatch()
  worker_instance = conftest.make_worker(tmp_path, "rl-watch")
  worker_instance._watch = relay_watch
  log_text = await conftest.process_worker_event(worker_instance, tmp_path, event, monkeypatch)
  assert relay_watch.seen == [event]
  assert json.loads(log_text.splitlines()[0])["type"] == ET.RATE_LIMIT_EVENT
