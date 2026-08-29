"""Unit tests for the effective-alive invariant in ``runs.resolve_run``.

Death is reported only when it can be PROVEN — pid, pid_start, and started_at
all present AND ``is_run_alive`` says dead. Every other case (any input
missing, raw missing so death is unverifiable, or the probe says alive) is
treated as alive and resolves to a RUNNING/STALLED row, never a
DIED-on-missing-evidence finalize.
"""

from __future__ import annotations

import os
from pathlib import Path

from test_runs import ASSISTANT_LINE, NOW, _resolve, _write_raw

from src.core import runs


def _live_identity() -> tuple[int, str]:
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  return pid, pid_start


# ---------------------------------------------------------------------------
# Liveness row: each missing-input variant is effective-alive
# ---------------------------------------------------------------------------


def test_missing_pid_and_started_at_variants_are_effective_alive(tmp_path: Path) -> None:
  """pid=None and started_at=None variants resolve exactly like pid_start=None:
  RUNNING with the missing field named, never DIED."""
  _write_raw(tmp_path, [ASSISTANT_LINE])
  pid, pid_start = _live_identity()

  no_pid = _resolve(tmp_path, pid=None, pid_start=pid_start, started_at=NOW)
  assert no_pid.outcome is runs.RunOutcome.RUNNING
  assert "pid" in no_pid.reason

  no_started_at = _resolve(tmp_path, pid=pid, pid_start=pid_start, started_at=None)
  assert no_started_at.outcome is runs.RunOutcome.RUNNING
  assert "started_at" in no_started_at.reason

  # Every field missing at once names every field.
  all_missing = _resolve(tmp_path, pid=None, pid_start=None, started_at=None)
  assert all_missing.outcome is runs.RunOutcome.RUNNING
  for field in ("pid", "pid_start", "started_at"):
    assert field in all_missing.reason


def test_missing_field_counts_silence_toward_stalled(tmp_path: Path) -> None:
  """An unverifiable run silent beyond the report threshold lands on the
  existing STALLED row: reported, not killed."""
  _write_raw(tmp_path, [ASSISTANT_LINE], age_seconds=runs.NO_OUTPUT_REPORT_THRESHOLD + 60)
  resolution = _resolve(tmp_path, pid=4242)  # pid_start absent -> death unverifiable
  assert resolution.outcome is runs.RunOutcome.STALLED
  assert "no raw output" in resolution.reason
  assert "pid_start" in resolution.reason


def test_missing_fields_never_reach_died_even_with_a_dead_pid(tmp_path: Path) -> None:
  """A pid that is dead is not proof of THIS run's death while pid_start is
  unrecorded — pid reuse must never finalize an innocent run."""
  _write_raw(tmp_path, [ASSISTANT_LINE])
  resolution = _resolve(tmp_path, pid=999999)  # no /proc entry; pid_start absent
  assert resolution.outcome is runs.RunOutcome.RUNNING


# ---------------------------------------------------------------------------
# Uncovered row: result pre-check, then effective-alive, then DIED
# ---------------------------------------------------------------------------


def test_uncovered_verified_alive_resolves_running(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE])
  pid, pid_start = _live_identity()
  resolution = _resolve(tmp_path, backend_type="opencode", pid=pid, pid_start=pid_start, started_at=NOW)
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.UNCOVERED_ALIVE_REASON


def test_uncovered_unverifiable_death_resolves_running(tmp_path: Path) -> None:
  """Missing any liveness input, an uncovered run is kept alive too — the
  alive check does not depend on which fork resolved the result pre-check."""
  _write_raw(tmp_path, [ASSISTANT_LINE])
  resolution = _resolve(tmp_path, backend_type="opencode", pid=999999)  # pid_start absent
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.UNCOVERED_ALIVE_REASON


def test_uncovered_without_raw_and_unverifiable_resolves_running(tmp_path: Path) -> None:
  resolution = _resolve(tmp_path, backend_type="opencode", pid=4242)  # no raw, pid_start absent
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.UNCOVERED_ALIVE_REASON


# ---------------------------------------------------------------------------
# Raw-missing row: both effective-alive variants, no DIED backdoor
# ---------------------------------------------------------------------------


def test_raw_missing_verified_alive_resolves_running(tmp_path: Path) -> None:
  pid, pid_start = _live_identity()
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start, started_at=NOW)
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.RAW_MISSING_ALIVE_REASON


def test_raw_missing_unverifiable_death_resolves_running(tmp_path: Path) -> None:
  resolution = _resolve(tmp_path, pid=999999)  # pid_start absent -> death unverifiable
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.RAW_MISSING_ALIVE_REASON
