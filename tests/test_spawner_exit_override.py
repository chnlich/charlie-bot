"""Tests for _maybe_override_exit_code_from_result.

A worker killed by SIGTERM after emitting a success result event must not be treated as
failed. The helper inspects events.jsonl and overrides non-zero exit codes accordingly.
"""

import json
from pathlib import Path

import pytest

from src.core import spawner
from src.core.models import ThreadMetadata


def _write_events(path: Path, events: list[dict]) -> None:
  with open(path, "w", encoding="utf-8") as f:
    for ev in events:
      f.write(json.dumps(ev) + "\n")


class _FakeThreadManager:
  def __init__(self, events_path: Path) -> None:
    self._events_path = events_path

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    del session_id, thread_id
    return self._events_path


def _thread() -> ThreadMetadata:
  return ThreadMetadata(id="thread-1", session_id="session-id", description="task")


@pytest.mark.asyncio
async def test_success_result_with_nonzero_exit_overrides_to_zero(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [
      {"type": "assistant", "message": {"content": "working"}},
      {"type": "result", "subtype": "success", "is_error": False, "result": "done"},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 0


@pytest.mark.asyncio
async def test_success_result_with_is_error_none_overrides_to_zero(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [
      {"type": "result", "subtype": "success", "result": "done"},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 0


@pytest.mark.asyncio
async def test_error_max_turns_subtype_does_not_override(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [
      {"type": "result", "subtype": "error_max_turns", "is_error": False},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_is_error_true_does_not_override(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [
      {"type": "result", "subtype": "success", "is_error": True},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_no_result_event_returns_original(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [
      {"type": "assistant", "message": {"content": "thinking"}},
      {"type": "tool_use", "name": "Bash"},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_only_last_result_event_is_considered(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  # Earlier success result followed by a later failure result: should NOT override.
  _write_events(events_path, [
      {"type": "result", "subtype": "success", "is_error": False},
      {"type": "result", "subtype": "error_max_turns", "is_error": True},
  ])

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_exit_code_zero_returns_zero_without_reading_events(tmp_path: Path) -> None:
  """When exit_code is already 0, the helper must short-circuit (no override needed, no I/O)."""
  read_calls: dict[str, int] = {"count": 0}

  class _CountingThreadManager:
    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      del session_id, thread_id
      read_calls["count"] += 1
      return tmp_path / "missing.jsonl"

  result = await spawner._maybe_override_exit_code_from_result(
      0, "session-id", _thread(), _CountingThreadManager())

  assert result == 0
  assert read_calls["count"] == 0


@pytest.mark.asyncio
async def test_missing_events_file_returns_original(tmp_path: Path) -> None:
  events_path = tmp_path / "does-not-exist.jsonl"

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_get_events_log_path_raising_does_not_propagate(tmp_path: Path) -> None:
  class _BrokenThreadManager:
    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      raise OSError("disk gone")

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _BrokenThreadManager())

  assert result == 143


@pytest.mark.asyncio
async def test_malformed_events_file_returns_original(tmp_path: Path) -> None:
  events_path = tmp_path / "events.jsonl"
  # parse_ndjson_file skips malformed lines; with no parseable result, no override.
  events_path.write_text("not json at all\n{also broken\n", encoding="utf-8")

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


def test_helper_is_wired_into_spawn_worker_after_stream_events() -> None:
  """Structural guard: the override helper is called between _stream_worker_events and finalize."""
  import inspect
  source = inspect.getsource(spawner.spawn_worker)
  stream_idx = source.find("_stream_worker_events")
  override_idx = source.find("_maybe_override_exit_code_from_result")
  finalize_idx = source.find("_finalize_worker_safely")

  assert stream_idx != -1, "spawn_worker must call _stream_worker_events"
  assert override_idx != -1, "spawn_worker must call _maybe_override_exit_code_from_result"
  assert finalize_idx != -1, "spawn_worker must call _finalize_worker_safely"
  assert stream_idx < override_idx < finalize_idx, (
      "override must run after _stream_worker_events and before _finalize_worker_safely")
  # The override is gated on a failed outcome (exit_code != 0 and not quota_exhausted) with
  # no recorded error.
  assert "outcome.failed" in source
  assert "not outcome.error" in source


@pytest.mark.parametrize(
    "outcome,expected",
    [
        (spawner._WorkerRunOutcome(exit_code=0, quota_exhausted=False, error=""), False),
        (spawner._WorkerRunOutcome(exit_code=143, quota_exhausted=False, error=""), True),
        (spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error="setup boom"), True),
        (spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=True, error=""), False),
    ],
    ids=["clean-exit", "nonzero-exit", "setup-error", "quota-exhausted"],
)
def test_run_outcome_failed_excludes_clean_exits_and_quota(outcome: spawner._WorkerRunOutcome, expected: bool) -> None:
  assert outcome.failed is expected
