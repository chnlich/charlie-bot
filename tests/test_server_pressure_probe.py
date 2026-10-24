"""Checks the pressure probe's write boundary and live-server guard."""

import json
import pathlib
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
