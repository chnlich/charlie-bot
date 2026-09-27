"""Unit tests for src/core/runs.py (run truth from disk) and finalize_effects judgments."""

from __future__ import annotations

import os
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from src.core import runs

HOST_BOOT = runs.read_host_boot_time()
NOW = datetime.now(UTC)


def _identity(event: dict) -> list[dict]:
  return [event]


def _write_raw(thread_dir: Path, lines: list[str], age_seconds: float = 0.0) -> Path:
  raw = thread_dir / "data" / runs.RAW_LOG_NAME
  raw.parent.mkdir(parents=True, exist_ok=True)
  raw.write_text("\n".join(lines) + "\n", encoding="utf-8")
  if age_seconds:
    ts = time.time() - age_seconds
    os.utime(raw, (ts, ts))
  return raw


RESULT_SUCCESS_LINE = '{"type": "result", "subtype": "success", "is_error": false, "result": "done", "usage": {}}'
ASSISTANT_LINE = '{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}}'

# ---------------------------------------------------------------------------
# Process liveness
# ---------------------------------------------------------------------------


def test_is_run_alive_requires_full_identity() -> None:
  pid = os.getpid()
  pid_start, _state = runs.read_pid_stat(pid)  # type: ignore[misc]
  assert runs.is_run_alive(pid, pid_start, NOW, HOST_BOOT) is True
  # Each missing conjunct kills the judgment.
  assert runs.is_run_alive(None, pid_start, NOW, HOST_BOOT) is False
  assert runs.is_run_alive(pid, None, NOW, HOST_BOOT) is False
  assert runs.is_run_alive(pid, pid_start, None, HOST_BOOT) is False
  # A forged pid_start (pid reuse cannot fake it) is not alive.
  assert runs.is_run_alive(pid, "1", NOW, HOST_BOOT) is False
  # Nothing survives a host reboot: a pre-boot start time is always stale.
  pre_boot = HOST_BOOT - timedelta(minutes=1)
  assert runs.is_run_alive(pid, pid_start, pre_boot, HOST_BOOT) is False
  # Naive started_at is a caller bug, not "dead".
  with pytest.raises(ValueError, match="must be timezone-aware"):
    runs.is_run_alive(pid, pid_start, datetime(2026, 1, 1), HOST_BOOT)


# ---------------------------------------------------------------------------
# Raw -> event projection (pure)
# ---------------------------------------------------------------------------


def test_parse_raw_lines_skips_blank_torn_and_non_json() -> None:
  blob = b'{"a": 1}\n\nnot-json\n{"b": 2}\n{"torn'
  events = runs.parse_raw_lines(blob)
  assert events == [{"a": 1}, {"b": 2}]


def test_result_success_matrix() -> None:
  assert runs.result_success({}) is True  # claude omits subtype/is_error on success
  assert runs.result_success({"subtype": "success", "is_error": False}) is True
  assert runs.result_success({"subtype": "error_max_turns"}) is False
  assert runs.result_success({"is_error": True}) is False


# ---------------------------------------------------------------------------
# End-of-run error hint selection (pure)


# ---------------------------------------------------------------------------
# Completion time and cursor


# ---------------------------------------------------------------------------
# resolve_run: the outcome rows
# ---------------------------------------------------------------------------


def _resolve(thread_dir: Path, **overrides: object) -> runs.RunResolution:
  kwargs = {
      "raw_path": runs.raw_log_path(thread_dir),
      "pid": None,
      "pid_start": None,
      "started_at": NOW,
      "backend_type": None,
      "translate": _identity,
      "host_boot_time": HOST_BOOT,
  }
  kwargs.update(overrides)
  return runs.resolve_run(**kwargs)


def test_resolve_died_when_raw_missing_and_death_verifiable(tmp_path: Path) -> None:
  resolution = _resolve(tmp_path, pid=999999, pid_start="1")
  assert resolution.outcome is runs.RunOutcome.DIED
  assert resolution.reason == runs.LEGACY_RAW_MISSING_REASON


def test_resolve_completed_uses_result_event(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE, RESULT_SUCCESS_LINE])
  resolution = _resolve(tmp_path, pid=None)
  assert resolution.outcome is runs.RunOutcome.COMPLETED
  assert resolution.completed_at == runs.raw_completion_time(runs.raw_log_path(tmp_path))

  _write_raw(tmp_path, ['{"type": "result", "subtype": "error_during_execution", "is_error": true}'])
  resolution = _resolve(tmp_path)
  assert resolution.outcome is runs.RunOutcome.COMPLETED


def test_resolve_stalled_when_alive_and_silent_beyond_threshold(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE], age_seconds=runs.NO_OUTPUT_REPORT_THRESHOLD + 60)
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start)
  assert resolution.outcome is runs.RunOutcome.STALLED
  assert "no raw output" in resolution.reason


def test_resolve_kept_alive_when_death_unverifiable_and_no_result(tmp_path: Path) -> None:
  """pid recorded but pid_start absent: even a dead pid cannot prove THIS
  run's death (pid reuse), so the run is effective-alive and the reason names
  the missing field."""
  _write_raw(tmp_path, [ASSISTANT_LINE])
  resolution = _resolve(tmp_path, pid=999999)
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert "pid_start" in resolution.reason


# ---------------------------------------------------------------------------
# Leftover fd holders


# ---------------------------------------------------------------------------
# finalize_effects judgments
