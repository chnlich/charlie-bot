"""The replay tool pairs a recording with the dictated message that followed it, in either stored form."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import pytest

from src.features.voice import replay_eval

RECORDED_AT = datetime(2026, 10, 8, 12, 0, 0, tzinfo=UTC)


def _ground_truth(tmp_path: Path, event: dict) -> str | None:
  events_path = tmp_path / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True)
  events_path.write_text(json.dumps(event) + "\n", encoding="utf-8")
  return replay_eval.find_ground_truth(tmp_path, RECORDED_AT)


@pytest.mark.parametrize(
    "stored_flag", [
        pytest.param({"input_mode": "voice"}, id="input_mode"),
        pytest.param({"is_voice": True}, id="is_voice"),
    ])
def test_a_dictated_user_event_is_the_ground_truth_in_either_stored_form(tmp_path: Path, stored_flag: dict) -> None:
  event = {"type": "user", "content": "said aloud", "timestamp": "2026-10-08T12:00:30+00:00", **stored_flag}

  assert _ground_truth(tmp_path, event) == "said aloud"


def test_a_typed_user_event_is_not_ground_truth(tmp_path: Path) -> None:
  event = {"type": "user", "content": "typed in", "timestamp": "2026-10-08T12:00:30+00:00"}

  assert _ground_truth(tmp_path, event) is None
