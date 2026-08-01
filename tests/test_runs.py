"""Unit tests for src/core/runs.py (run truth from disk) and finalize_effects judgments."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src.core import event_types as ET
from src.core import finalize_effects, runs

HOST_BOOT = runs.read_host_boot_time()
NOW = datetime.now(timezone.utc)


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


def test_read_host_boot_time_is_aware_and_in_the_past() -> None:
  assert HOST_BOOT.tzinfo is not None
  assert HOST_BOOT < NOW


def test_read_pid_stat_live_and_dead() -> None:
  pair = runs.read_pid_stat(os.getpid())
  assert pair is not None
  assert pair[1] in ("R", "S")
  assert runs.read_pid_stat(999999) is None


def test_is_run_alive_requires_full_identity() -> None:
  pid = os.getpid()
  pid_start, state = runs.read_pid_stat(pid)  # type: ignore[misc]
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
  with pytest.raises(ValueError):
    runs.is_run_alive(pid, pid_start, datetime(2026, 1, 1), HOST_BOOT)


# ---------------------------------------------------------------------------
# Raw -> event projection (pure)
# ---------------------------------------------------------------------------


def test_parse_raw_lines_skips_blank_torn_and_non_json() -> None:
  blob = b'{"a": 1}\n\nnot-json\n{"b": 2}\n{"torn'
  events = runs.parse_raw_lines(blob)
  assert events == [{"a": 1}, {"b": 2}]


def test_project_raw_events_applies_translate_in_order() -> None:
  events = [{"n": 1}, {"n": 2}]
  out = runs.project_raw_events(events, lambda e: [e, {"dup_of": e["n"]}])
  assert out == [{"n": 1}, {"dup_of": 1}, {"n": 2}, {"dup_of": 2}]


def test_summarize_result_takes_last_result() -> None:
  assert runs.summarize_result([{"type": "assistant"}, {"type": ET.RESULT, "result": "a"}]) == {"type": ET.RESULT, "result": "a"}
  assert runs.summarize_result([{"type": ET.RESULT, "result": "first"}, {"type": ET.RESULT, "result": "last"}]) == {
      "type": ET.RESULT, "result": "last"}
  assert runs.summarize_result([{"type": "assistant"}]) is None


def test_result_success_matrix() -> None:
  assert runs.result_success({}) is True  # claude omits subtype/is_error on success
  assert runs.result_success({"subtype": "success", "is_error": False}) is True
  assert runs.result_success({"subtype": "error_max_turns"}) is False
  assert runs.result_success({"is_error": True}) is False


# ---------------------------------------------------------------------------
# Completion time and cursor
# ---------------------------------------------------------------------------


def test_raw_completion_time_missing_and_present(tmp_path: Path) -> None:
  assert runs.raw_completion_time(tmp_path / "nope") is None
  raw = _write_raw(tmp_path, [RESULT_SUCCESS_LINE], age_seconds=60)
  completion = runs.raw_completion_time(raw)
  # Fresh clock: a module-level "now" goes stale when the full suite runs first.
  assert completion is not None
  assert completion.tzinfo is not None
  assert abs((datetime.now(timezone.utc) - completion).total_seconds() - 60) < 5


def test_raw_cursor_roundtrip_and_fallbacks(tmp_path: Path) -> None:
  cursor = tmp_path / "sub" / runs.CURSOR_NAME
  assert runs.read_raw_cursor(cursor) == 0  # missing -> replay
  runs.write_raw_cursor(cursor, 1234)
  assert runs.read_raw_cursor(cursor) == 1234
  cursor.write_text("garbage", encoding="utf-8")
  assert runs.read_raw_cursor(cursor) == 0  # unparseable -> replay


# ---------------------------------------------------------------------------
# resolve_run: the six outcome rows
# ---------------------------------------------------------------------------


def _resolve(thread_dir: Path, **overrides) -> runs.RunResolution:
  kwargs = {
      "thread_dir": thread_dir,
      "pid": None,
      "pid_start": None,
      "started_at": NOW,
      "backend_type": None,
      "translate": _identity,
      "host_boot_time": HOST_BOOT,
  }
  kwargs.update(overrides)
  return runs.resolve_run(**kwargs)


def test_resolve_never_started_when_no_raw_and_no_pid(tmp_path: Path) -> None:
  resolution = _resolve(tmp_path)
  assert resolution.outcome is runs.RunOutcome.NEVER_STARTED
  # NEVER_STARTED ranks above backend coverage: a fresh spawn works for any backend.
  resolution = _resolve(tmp_path, backend_type="opencode")
  assert resolution.outcome is runs.RunOutcome.NEVER_STARTED


def test_resolve_uncovered_backend_type_dies_with_transport_reason(tmp_path: Path) -> None:
  _write_raw(tmp_path, [RESULT_SUCCESS_LINE])
  for backend_type in ("opencode", "antigravity", "tui-cli"):
    resolution = _resolve(tmp_path, backend_type=backend_type)
    assert resolution.outcome is runs.RunOutcome.DIED
    assert resolution.reason == runs.TRANSPORT_NOT_COVERED_REASON


def test_resolve_died_when_raw_missing_but_pid_recorded(tmp_path: Path) -> None:
  resolution = _resolve(tmp_path, pid=4242)
  assert resolution.outcome is runs.RunOutcome.DIED
  assert resolution.reason == runs.LEGACY_RAW_MISSING_REASON


def test_resolve_completed_uses_result_event_for_success(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE, RESULT_SUCCESS_LINE])
  resolution = _resolve(tmp_path, pid=None)
  assert resolution.outcome is runs.RunOutcome.COMPLETED
  assert resolution.success is True
  assert resolution.completed_at == runs.raw_completion_time(runs.raw_log_path(tmp_path))

  _write_raw(tmp_path, ['{"type": "result", "subtype": "error_during_execution", "is_error": true}'])
  resolution = _resolve(tmp_path)
  assert resolution.outcome is runs.RunOutcome.COMPLETED
  assert resolution.success is False


def test_resolve_completed_even_when_process_still_alive(tmp_path: Path) -> None:
  """A trailing result decides the run; the post-result hang belongs to cleanup, not the outcome."""
  _write_raw(tmp_path, [RESULT_SUCCESS_LINE])
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start)
  assert resolution.outcome is runs.RunOutcome.COMPLETED


def test_resolve_running_when_alive_and_silent_below_threshold(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE], age_seconds=60)
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start)
  assert resolution.outcome is runs.RunOutcome.RUNNING


def test_resolve_stalled_when_alive_and_silent_beyond_threshold(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE], age_seconds=runs.NO_OUTPUT_REPORT_THRESHOLD + 60)
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start)
  assert resolution.outcome is runs.RunOutcome.STALLED
  assert "no raw output" in resolution.reason


def test_resolve_died_when_not_alive_and_no_result(tmp_path: Path) -> None:
  _write_raw(tmp_path, [ASSISTANT_LINE])
  resolution = _resolve(tmp_path, pid=999999)
  assert resolution.outcome is runs.RunOutcome.DIED
  assert resolution.reason == runs.DIED_WITHOUT_RESULT_REASON


def test_resolve_drops_torn_final_line_from_the_result_scan(tmp_path: Path) -> None:
  # Producer killed mid-write: the torn tail must not manufacture a result.
  raw = tmp_path / "data" / runs.RAW_LOG_NAME
  raw.parent.mkdir(parents=True)
  raw.write_bytes((ASSISTANT_LINE + '\n{"type": "result", "subty').encode("utf-8"))
  resolution = _resolve(tmp_path, pid=999999)
  assert resolution.outcome is runs.RunOutcome.DIED


# ---------------------------------------------------------------------------
# Leftover fd holders (row 5)
# ---------------------------------------------------------------------------


@pytest.fixture()
def sleep_holding_stdout(tmp_path: Path):
  """A live process whose fd 1 points at a real file; cleaned up after the test."""
  target = tmp_path / "held.log"
  fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_APPEND)
  proc = subprocess.Popen(["sleep", "30"], stdout=fd, stderr=subprocess.DEVNULL)
  os.close(fd)
  try:
    yield proc, target
  finally:
    proc.kill()
    proc.wait()


def test_scan_and_leftover_holders(sleep_holding_stdout) -> None:
  proc, target = sleep_holding_stdout
  holders_scan = runs.scan_stdout_holders()
  st = target.stat()
  assert any(h.pid == proc.pid for h in holders_scan.get((st.st_dev, st.st_ino), []))

  # The run leader itself (run_pid) and this test process are never listed.
  leftovers = runs.leftover_holders_for(target, holders_scan, run_pid=None)
  assert any(h.pid == proc.pid for h in leftovers)
  assert all(h.pid != os.getpid() for h in leftovers)

  leftovers = runs.leftover_holders_for(target, holders_scan, run_pid=proc.pid)
  assert all(h.pid != proc.pid for h in leftovers)

  # A missing raw log has no inode, hence no holders.
  assert runs.leftover_holders_for(target.parent / "gone", holders_scan, run_pid=None) == ()


def test_resolve_attaches_leftover_holders_only_when_not_alive(sleep_holding_stdout, tmp_path: Path) -> None:
  proc, target = sleep_holding_stdout
  raw = runs.raw_log_path(tmp_path)
  raw.parent.mkdir(parents=True)
  # Point the leftover scan at the SAME inode the run's raw log has: hardlink.
  os.link(target, raw)
  _write_raw(tmp_path, [ASSISTANT_LINE])
  holders_scan = { (target.stat().st_dev, target.stat().st_ino): [runs.HolderProcess(pid=proc.pid, cmdline="sleep 30")] }

  # Dead run: the leftover holder is attached for reporting + row-5 cleanup.
  resolution = _resolve(tmp_path, pid=999999, holders_scan=holders_scan)
  assert resolution.outcome is runs.RunOutcome.DIED
  assert [h.pid for h in resolution.leftover_holders] == [proc.pid]

  # Alive run: fd holders are descendants, never liveness, and not reported.
  pid = os.getpid()
  pid_start, _ = runs.read_pid_stat(pid)  # type: ignore[misc]
  resolution = _resolve(tmp_path, pid=pid, pid_start=pid_start, holders_scan=holders_scan)
  assert resolution.outcome is runs.RunOutcome.RUNNING
  assert resolution.leftover_holders == ()


# ---------------------------------------------------------------------------
# finalize_effects judgments
# ---------------------------------------------------------------------------


def _worker_summary(thread_id: str, status: str = "completed") -> dict:
  return {"type": ET.WORKER_SUMMARY, "thread_id": thread_id, "status": status, "content": "s"}


def test_terminal_summary_present_judgment() -> None:
  events = [_worker_summary("t1")]
  assert finalize_effects.terminal_summary_present(events, "t1") is True
  assert finalize_effects.terminal_summary_present(events, "t2") is False
  # A running summary is not the terminal effect.
  assert finalize_effects.terminal_summary_present([_worker_summary("t1", status="running")], "t1") is False
  assert finalize_effects.terminal_summary_present([], "t1") is False


def test_master_woke_after_summary_judgment() -> None:
  summary = _worker_summary("t1")
  # No summary -> no wake measured (the summary persist is the prerequisite).
  assert finalize_effects.master_woke_after_summary([], "t1") is False
  # Master output BEFORE the summary does not count.
  assert finalize_effects.master_woke_after_summary([{"type": ET.ASSISTANT}, summary], "t1") is False
  # Assistant/master_done/assistant_error after the LAST summary count.
  for evt_type in (ET.ASSISTANT, ET.MASTER_DONE, ET.ASSISTANT_ERROR):
    assert finalize_effects.master_woke_after_summary([summary, {"type": evt_type}], "t1") is True
  # Noise (errors, triggers, heartbeats) does not.
  assert finalize_effects.master_woke_after_summary([summary, {"type": "error"}], "t1") is False
  # A re-summary re-arms the judgment: output must follow the LAST summary.
  events = [summary, {"type": ET.ASSISTANT}, _worker_summary("t1")]
  assert finalize_effects.master_woke_after_summary(events, "t1") is False


def test_reviewer_thread_exists_judgment() -> None:
  from src.core.models import ThreadMetadata

  original = ThreadMetadata(id="orig", session_id="s", description="task")
  reviewer = ThreadMetadata(id="rev", session_id="s", description="review", review_of="orig")
  threads = [original, reviewer]
  assert finalize_effects.reviewer_thread_exists(threads, "orig") is True
  assert finalize_effects.reviewer_thread_exists(threads, "orig", exclude_thread_id="rev") is False
  assert finalize_effects.reviewer_thread_exists([original], "orig") is False
  assert finalize_effects.reviewer_thread_exists(threads, "other") is False
