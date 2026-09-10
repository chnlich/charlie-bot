"""Tests for _maybe_override_exit_code_from_result.

A worker killed by SIGTERM after emitting a success result event must not be treated as
failed. The helper inspects events.jsonl and overrides non-zero exit codes accordingly.
"""

from pathlib import Path
from typing import Any

import pytest
from conftest import CLEAN_EXIT_OUTCOME, append_events

from src.core import spawner
from src.core.models import ThreadMetadata


class _FakeThreadManager:

  def __init__(self, events_path: Path) -> None:
    self._events_path = events_path

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    del session_id, thread_id
    return self._events_path


def _thread() -> ThreadMetadata:
  return ThreadMetadata(id="thread-1", session_id="session-id", description="task")


async def _override_from_events(tmp_path: Path, events: list[dict], exit_code: int) -> int:
  """Stage *events* in a fresh events.jsonl and run the exit-code override against it."""
  events_path = tmp_path / "events.jsonl"
  append_events(events_path, events)
  return await spawner._maybe_override_exit_code_from_result(
      exit_code, "session-id", _thread(), _FakeThreadManager(events_path))


def _assistant_event(content: str) -> dict:
  return {"type": "assistant", "message": {"content": content}}


def _result_event(subtype: str, **extra: Any) -> dict:
  return {"type": "result", "subtype": subtype, **extra}


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("events", "expected"),
    [
        ([_assistant_event("working"),
          _result_event("success", is_error=False, result="done")], 0),
        ([_result_event("success", result="done")], 0),
        ([_result_event("error_max_turns", is_error=False)], 143),
        ([_result_event("success", is_error=True)], 143),
        ([_assistant_event("thinking"), {
            "type": "tool_use",
            "name": "Bash"
        }], 143),
        ([_result_event("success", is_error=False),
          _result_event("error_max_turns", is_error=True)], 143),
    ],
    ids=[
        "success-overrides",
        "success-without-is-error-overrides",
        "error-subtype-keeps",
        "is-error-keeps",
        "no-result-keeps",
        "last-result-wins",
    ],
)
async def test_exit_override_decision_by_last_result_event(tmp_path: Path, events: list[dict], expected: int) -> None:
  """The last result event decides: success with is_error False or absent overrides 143 to 0."""
  assert await _override_from_events(tmp_path, events, 143) == expected


@pytest.mark.asyncio
async def test_exit_code_zero_returns_zero_without_reading_events(tmp_path: Path) -> None:
  """When exit_code is already 0, the helper must short-circuit (no override needed, no I/O)."""
  read_calls: dict[str, int] = {"count": 0}

  class _CountingThreadManager:

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      del session_id, thread_id
      read_calls["count"] += 1
      return tmp_path / "missing.jsonl"

  result = await spawner._maybe_override_exit_code_from_result(0, "session-id", _thread(), _CountingThreadManager())

  assert result == 0
  assert read_calls["count"] == 0


@pytest.mark.asyncio
async def test_missing_events_file_returns_original(tmp_path: Path) -> None:
  events_path = tmp_path / "does-not-exist.jsonl"

  result = await spawner._maybe_override_exit_code_from_result(
      143, "session-id", _thread(), _FakeThreadManager(events_path))

  assert result == 143


@pytest.mark.asyncio
async def test_get_events_log_path_raising_does_not_propagate() -> None:

  class _BrokenThreadManager:

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      raise OSError("disk gone")

  result = await spawner._maybe_override_exit_code_from_result(143, "session-id", _thread(), _BrokenThreadManager())

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
    ("outcome", "expected"),
    [
        (CLEAN_EXIT_OUTCOME, False),
        (spawner._WorkerRunOutcome(exit_code=143, quota_exhausted=False, error=""), True),
        (spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=False, error="setup boom"), True),
        (spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=True, error=""), False),
    ],
    ids=["clean-exit", "nonzero-exit", "setup-error", "quota-exhausted"],
)
def test_run_outcome_failed_excludes_clean_exits_and_quota(outcome: spawner._WorkerRunOutcome, expected: bool) -> None:
  assert outcome.failed is expected
