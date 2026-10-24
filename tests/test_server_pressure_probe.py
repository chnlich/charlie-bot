"""Checks the pressure probe's write boundary, live-server guard, sizing and restart."""

import json
import pathlib
import threading
import time

import pytest

from tools import server_pressure_probe


def test_server_stops_when_linked_file_is_writable(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "home"
  linked_file = home / "sessions" / "live-session" / "data" / "chat_events.jsonl"
  linked_file.parent.mkdir(parents=True)
  linked_file.write_text("", encoding="utf-8")
  control = tmp_path / "control"
  control.mkdir()
  writable = tmp_path / "synthetic-session"
  writable.mkdir()
  manifest = tmp_path / "linked.json"
  manifest.write_text(json.dumps({"linked_files": [str(linked_file)]}), encoding="utf-8")

  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  monkeypatch.setenv("PATH", server_pressure_probe.SERVER_PATH)
  monkeypatch.setattr(server_pressure_probe, "apply_landlock_write_allowlist", lambda _dirs: {"abi": 1})

  with pytest.raises(RuntimeError, match="linked session file for writing"):
    server_pressure_probe._serve_main(
        [
            "--home",
            str(home),
            "--port",
            "18499",
            "--linked-manifest",
            str(manifest),
            "--control-dir",
            str(control),
            "--start-file",
            str(control / "start"),
            "--stop-file",
            str(control / "stop"),
            "--ready-file",
            str(control / "ready.json"),
            "--writable-dir",
            str(writable),
        ])


def test_copy_rejects_a_parent_id_outside_the_selected_sessions(tmp_path: pathlib.Path) -> None:
  sessions = tmp_path / "sessions"
  selected = sessions / "selected"
  selected.mkdir(parents=True)
  metadata = {
      "name": "selected",
      "profile": "manager",
      "schema_version": 2,
      "task_parent_id": "../../outside",
  }
  (selected / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")

  with pytest.raises(ValueError, match="direct child name"):
    server_pressure_probe.build_session_copy(
        copy_root=tmp_path / "copy",
        source_sessions=sessions,
        selected_ids=["selected"],
        event_byte_limit=server_pressure_probe.GIB,
    )


def test_guard_freezes_slow_probe_and_thaws_after_30_fast_seconds() -> None:

  class FakeClock:

    def __init__(self) -> None:
      self.now = 0.0

    def __call__(self) -> float:
      return self.now

    def advance(self, seconds: float) -> None:
      self.now += seconds

  clock = FakeClock()
  probe_count = 0
  freeze_times: list[float] = []
  thaw_times: list[float] = []

  def probe() -> int:
    nonlocal probe_count
    probe_count += 1
    clock.advance(1.1 if probe_count == 1 else 0.1)
    if probe_count == 1:
      time.sleep(1.05)
    return 200

  guard = server_pressure_probe.PressureGuard(
      probe=probe,
      memory_available=lambda: 32 * server_pressure_probe.GIB,
      freeze=lambda: freeze_times.append(clock.now),
      thaw=lambda: thaw_times.append(clock.now),
      stop_slice=lambda: None,
      clock=clock,
      wall_clock=lambda: f"t={clock.now:.1f}",
  )

  guard.check_once()
  assert freeze_times == [1.1]
  clock.advance(0.9)
  guard.check_once()
  for _ in range(29):
    clock.advance(0.9)
    guard.check_once()
  assert thaw_times == []

  clock.advance(0.9)
  guard.check_once()
  assert thaw_times == [32.1]
  interval = guard.snapshot()["freeze_intervals"][0]
  assert interval["thaw_reason"] == "30_seconds_of_fast_probes"
  assert interval["duration_s"] == 31.0


def test_cold_verdict_applies_the_p95_and_longest_standard() -> None:
  tree = "GET /api/sessions/tree"
  status = "GET /api/sessions/status"
  web_status = "POST /api/internal/web/status"
  reference = {
      status: {
          "p95_ms": 10.0,
          "longest_ms": 20.0
      },
      web_status: {
          "p95_ms": 10.0,
          "longest_ms": 1.0
      },
  }
  concurrent = {
      tree: {
          "p95_ms": 1500.0,
          "longest_ms": 1500.0
      },
      status: {
          "p95_ms": 20.0,
          "longest_ms": 5020.0
      },
      web_status: {
          "p95_ms": 3.0,
          "longest_ms": 5002.0
      },
  }

  verdict = server_pressure_probe._judge_against_reference(concurrent, reference)

  assert tree not in verdict["endpoints"]
  assert verdict["endpoints"][status]["passed"] is True
  assert verdict["endpoints"][web_status]["passed"] is False
  assert verdict["passed"] is False


def test_pressure_target_exceeds_slice_max_inside_the_swap_cap() -> None:
  slice_max = 8 * server_pressure_probe.GIB
  rss_anon = int(1.75 * server_pressure_probe.GIB)
  swap_cap = server_pressure_probe.PARENT_SWAP_MAX
  excess = server_pressure_probe.MEMORY_PRESSURE_SWAP_EXCESS_BYTES
  reserve = server_pressure_probe.REPAIR_PROBE_BYTES

  target = server_pressure_probe.compute_memory_pressure_target(
      server_rss_anon_bytes=rss_anon,
      slice_memory_max_bytes=slice_max,
      slice_swap_max_bytes=swap_cap,
      swap_excess_bytes=excess,
      probe_reserve_bytes=reserve,
      cap_bytes=int(server_pressure_probe.MAX_MEMORY_PRESSURE_GIB * server_pressure_probe.GIB),
  )

  assert target == slice_max - rss_anon + excess - reserve
  assert rss_anon + target > slice_max
  # The repair probe runs beside the worker in the same slice, so the peak
  # overshoot is the full excess and must stay inside the swap cap.
  assert rss_anon + target + reserve - slice_max < swap_cap


def test_pressure_target_fails_fast_outside_its_bounds() -> None:
  gib = server_pressure_probe.GIB
  common = {
      "slice_memory_max_bytes": 8 * gib,
      "slice_swap_max_bytes": server_pressure_probe.PARENT_SWAP_MAX,
      "swap_excess_bytes": server_pressure_probe.MEMORY_PRESSURE_SWAP_EXCESS_BYTES,
      "probe_reserve_bytes": server_pressure_probe.REPAIR_PROBE_BYTES,
      "cap_bytes": int(server_pressure_probe.MAX_MEMORY_PRESSURE_GIB * gib),
  }
  with pytest.raises(RuntimeError, match=r"exceeds the .*-byte cap"):
    server_pressure_probe.compute_memory_pressure_target(server_rss_anon_bytes=int(0.5 * gib), **common)
  with pytest.raises(RuntimeError, match="alone fills the slice MemoryMax"):
    server_pressure_probe.compute_memory_pressure_target(server_rss_anon_bytes=9 * gib, **common)
  with pytest.raises(RuntimeError, match="inside the slice swap cap"):
    server_pressure_probe.compute_memory_pressure_target(
        server_rss_anon_bytes=int(1.75 * gib),
        slice_memory_max_bytes=8 * gib,
        slice_swap_max_bytes=server_pressure_probe.PARENT_SWAP_MAX,
        swap_excess_bytes=server_pressure_probe.PARENT_SWAP_MAX,
        probe_reserve_bytes=common["probe_reserve_bytes"],
        cap_bytes=common["cap_bytes"],
    )
  with pytest.raises(RuntimeError, match="steady-state excess"):
    server_pressure_probe.compute_memory_pressure_target(
        server_rss_anon_bytes=int(1.75 * gib),
        slice_memory_max_bytes=8 * gib,
        slice_swap_max_bytes=server_pressure_probe.PARENT_SWAP_MAX,
        swap_excess_bytes=common["probe_reserve_bytes"],
        probe_reserve_bytes=common["probe_reserve_bytes"],
        cap_bytes=common["cap_bytes"],
    )


class _FakeWorker:
  """Worker handle whose exits and survivorship the test controls directly."""

  def __init__(self, unit: str, allocated: bool, exit_code: int, runs_until_stopped: bool) -> None:
    self.unit = unit
    self.allocated = allocated
    self._exit_code = exit_code
    self._runs_until_stopped = runs_until_stopped
    self.stopped = False
    self.log_closed = False

  def is_running(self) -> bool:
    if self.stopped:
      return False
    return self._runs_until_stopped

  def poll_allocation(self) -> bool:
    return self.allocated

  def exit_code(self) -> int:
    return self._exit_code

  def stop(self) -> int:
    self.stopped = True
    return 0

  def close_log(self) -> None:
    self.log_closed = True


def _wait_for_worker_starts(starts: list, count: int, timeout: float = 5.0) -> None:
  deadline = time.monotonic() + timeout
  while len(starts) < count:
    if time.monotonic() >= deadline:
      raise TimeoutError(f"supervisor started {len(starts)} workers, expected {count}")
    time.sleep(0.005)


def test_supervisor_restarts_a_killed_allocated_worker_until_stop() -> None:
  starts: list = []
  kill_count = [0]

  def start_worker() -> server_pressure_probe.PressureWorker:
    if len(starts) == 0:
      worker = _FakeWorker("worker-killed", allocated=True, exit_code=-9, runs_until_stopped=False)
    else:
      worker = _FakeWorker("worker-survivor", allocated=True, exit_code=0, runs_until_stopped=True)
    starts.append(worker)
    return worker

  def kill_total() -> int:
    kill_count[0] += 1
    return kill_count[0]

  supervisor = server_pressure_probe.PressureSupervisor(
      start_worker=start_worker, kill_total=kill_total, poll_interval_s=0.001)
  thread = threading.Thread(target=supervisor.run, daemon=True)
  thread.start()
  try:
    assert supervisor.first_activity.wait(timeout=5.0)
    _wait_for_worker_starts(starts, 2)
    supervisor.stop()
    thread.join(timeout=5)
    assert not thread.is_alive()
    assert supervisor.active is starts[1]
    assert supervisor.active.is_running()
    assert supervisor.kills == [
        {
            "at": supervisor.kills[0]["at"],
            "worker_exit_code": -9,
            "oom_kill_total": 1,
            "cumulative_kills": 1,
        }
    ]
    assert supervisor.worker_starts == ["worker-killed", "worker-survivor"]
    assert supervisor.worker_exits == [-9]
    assert supervisor.failure is None
  finally:
    supervisor.stop()
    thread.join(timeout=5)


def test_supervisor_starts_the_window_on_the_first_kill_without_allocation() -> None:
  starts: list = []

  def start_worker() -> server_pressure_probe.PressureWorker:
    if len(starts) == 0:
      worker = _FakeWorker("worker-killed", allocated=False, exit_code=-9, runs_until_stopped=False)
    else:
      worker = _FakeWorker("worker-survivor", allocated=False, exit_code=0, runs_until_stopped=True)
    starts.append(worker)
    return worker

  supervisor = server_pressure_probe.PressureSupervisor(
      start_worker=start_worker, kill_total=lambda: 1, poll_interval_s=0.001)
  thread = threading.Thread(target=supervisor.run, daemon=True)
  thread.start()
  try:
    assert supervisor.first_activity.wait(timeout=5.0)
    assert starts[0].allocated is False
    _wait_for_worker_starts(starts, 2)
    supervisor.stop()
    thread.join(timeout=5)
    assert not thread.is_alive()
    assert len(supervisor.kills) == 1
    assert supervisor.kills[0]["worker_exit_code"] == -9
    assert supervisor.worker_starts == ["worker-killed", "worker-survivor"]
    assert supervisor.failure is None
  finally:
    supervisor.stop()
    thread.join(timeout=5)
