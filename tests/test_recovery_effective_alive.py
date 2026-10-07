"""Unit tests for the effective-alive invariant in ``runs.resolve_run``.

Death is reported only when it can be PROVEN — pid, pid_start, and started_at
all present AND ``is_run_alive`` says dead. Every other case (any input
missing, raw missing so death is unverifiable, or the probe says alive) is
treated as alive and resolves to a RUNNING/STALLED row, never a
DIED-on-missing-evidence finalize.
"""

from __future__ import annotations

import os
import pathlib

import test_runs

from src.runtime import runs


def _live_identity() -> tuple[int, str]:
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  return pid, pid_start


# ---------------------------------------------------------------------------
# Liveness row: each missing-input variant is effective-alive
# ---------------------------------------------------------------------------


def test_missing_field_counts_silence_toward_stalled(tmp_path: pathlib.Path) -> None:
  """An unverifiable run silent beyond the report threshold lands on the
  existing STALLED row: reported, not killed."""
  test_runs._write_raw(tmp_path, [test_runs.ASSISTANT_LINE], age_seconds=runs.NO_OUTPUT_REPORT_THRESHOLD + 60)
  resolution = test_runs._resolve(tmp_path, pid=4242)  # pid_start absent -> death unverifiable
  assert resolution.outcome is runs.RunOutcome.STALLED
  assert "no raw output" in resolution.reason
  assert "pid_start" in resolution.reason


def test_missing_fields_never_reach_died_even_with_a_dead_pid(tmp_path: pathlib.Path) -> None:
  """A pid that is dead is not proof of THIS run's death while pid_start is
  unrecorded — pid reuse must never finalize an innocent run."""
  test_runs._write_raw(tmp_path, [test_runs.ASSISTANT_LINE])
  resolution = test_runs._resolve(tmp_path, pid=999999)  # no /proc entry; pid_start absent
  assert resolution.outcome is runs.RunOutcome.RUNNING


# ---------------------------------------------------------------------------
# Uncovered row: result pre-check, then effective-alive, then DIED
# ---------------------------------------------------------------------------


def test_uncovered_verified_alive_resolves_running(tmp_path: pathlib.Path) -> None:
  test_runs._write_raw(tmp_path, [test_runs.ASSISTANT_LINE])
  pid, pid_start = _live_identity()
  resolution = test_runs._resolve(
      tmp_path, backend_type="opencode", pid=pid, pid_start=pid_start, started_at=test_runs.NOW)
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.reason == runs.UNCOVERED_ALIVE_REASON
