"""Scenario loader tests for the parity harness's asynchronous step forms."""

import contextlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import threading
import time
import types
from collections.abc import Iterator
from typing import Any

import pytest
import yaml

from rust.parity import parity
from rust.parity import scenario_file


def _load(tmp_path: pathlib.Path, steps: list[dict[str, Any]], **fields: Any) -> scenario_file.Scenario:
  document: dict[str, Any] = {"stage": "unit", "name": "loader", "steps": steps}
  document.update(fields)
  path = tmp_path / "scenario.yaml"
  path.write_text(yaml.safe_dump(document, sort_keys=False), encoding="utf-8")
  return scenario_file.load(path)


def _fake_agent() -> dict[str, Any]:
  return {"starts": []}


@pytest.mark.parametrize(
    "step,fields", [
        ({
            "name": "http step",
            "method": "GET",
            "path": "/api/auth/status",
            "defer_files": True
        }, {}),
        (
            {
                "kind": "wait",
                "name": "event wait",
                "timeout_seconds": 1,
                "until":
                    {
                        "event":
                            {
                                "session": "synthetic-session",
                                "type": "run_finished",
                                "fields": {
                                    "outcome": "success"
                                },
                                "count": 2,
                            },
                    }
            }, {}),
        (
            {
                "kind": "wait",
                "name": "file wait",
                "timeout_seconds": 1,
                "until":
                    {
                        "file_field":
                            {
                                "path": "sessions/synthetic/triggers/one.json",
                                "field": "status",
                                "equals": "fired"
                            },
                    }
            }, {}),
        (
            {
                "kind": "wait",
                "name": "gate wait",
                "timeout_seconds": 1,
                "until": {
                    "at_gate": "ready"
                }
            }, {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
            }),
        ({
            "kind": "gate",
            "name": "gate step",
            "open": "ready"
        }, {
            "fake_agent": _fake_agent(),
            "expect_starts": 0,
        }),
        (
            {
                "kind": "ws",
                "name": "wait for provider",
                "path": "/ws/voice/synthetic-session",
                "send": [{
                    "text": "{bad"
                }],
                "until": {
                    "frames": 1
                },
                "timeout_seconds": 1,
                "wait_for_fake_handshake": "gemini",
            }, {
                "fakes": {
                    "gemini": {}
                }
            }),
        (
            {
                "kind": "handover",
                "name": "handover step",
                "agent_after_stop": "alive"
            }, {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["python-to-candidate"],
            }),
        (
            {
                "kind": "log_event",
                "name": "log event step",
                "event": "server_started",
                "fields": {
                    "ok": True
                },
                "timeout_seconds": 5,
            }, {}),
    ])
def test_new_step_kinds_and_keys_load(tmp_path: pathlib.Path, step: dict[str, Any], fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [step], **fields)
  assert scenario.steps[0].name == step["name"]
  if step.get("wait_for_fake_handshake"):
    assert scenario.steps[0].wait_for_fake_handshake == step["wait_for_fake_handshake"]


def test_log_event_step_keeps_its_event_fields_and_timeout(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "log_event",
      "name": "ext usage fetch fails",
      "event": "ext_usage_fetched",
      "fields": {
          "ok": False,
          "attempt": 2
      },
      "timeout_seconds": 5,
  }
  parsed = _load(tmp_path, [step]).steps[0]
  assert parsed.route == "(log_event)"
  assert parsed.log_event == "ext_usage_fetched"
  assert parsed.log_fields == {"ok": "false", "attempt": "2"}
  assert parsed.timeout_seconds == 5.0


def test_tree_path_missing_wait_checks_tree_containment(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "wait",
      "name": "worktree cleanup",
      "timeout_seconds": 1,
      "until": {
          "tree_path_missing": {
              "path": "${capture.worktree}"
          }
      },
  }
  scenario = _load(tmp_path, [step])
  assert scenario.steps[0].until == {"tree_path_missing": {"path": "${capture.worktree}"}}

  tree_root = tmp_path / "tree"
  tree_root.mkdir()
  side = parity.Side(
      "candidate", types.SimpleNamespace(root=tree_root, home=tree_root / "home"), 18600, parity.servers.STRACE_BIN)
  side.captures["worktree"] = str(tree_root / "worktrees" / "synthetic-task")
  until = {"tree_path_missing": {"path": "${capture.worktree}"}}
  assert parity._condition_met(side, until, {})

  worktree = tree_root / "worktrees" / "synthetic-task"
  worktree.mkdir(parents=True)
  assert not parity._condition_met(side, until, {})
  worktree.rmdir()

  outside = tmp_path / "outside"
  outside.mkdir()
  side.captures["worktree"] = str(outside)
  with pytest.raises(RuntimeError, match="wait path escapes"):
    parity._condition_met(side, until, {})

  escaped = {**step, "until": {"tree_path_missing": {"path": "../outside"}}}
  with pytest.raises(scenario_file.ScenarioError, match="must stay under the scenario tree"):
    _load(tmp_path, [escaped])


def test_ws_fake_handshake_requires_a_declared_fake(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "ws",
      "name": "wait for provider",
      "path": "/ws/voice/synthetic-session",
      "send": [{
          "text": "{bad"
      }],
      "until": {
          "frames": 1
      },
      "timeout_seconds": 1,
      "wait_for_fake_handshake": "gemini",
  }
  with pytest.raises(scenario_file.ScenarioError, match="not declared in fakes"):
    _load(tmp_path, [step])


@pytest.mark.parametrize(
    "fields", [
        {
            "config": {
                "paths": {
                    "workspace_dirs": ["${tree.root}/workspace"]
                }
            }
        },
        {
            "credentials": {
                "synthetic": {
                    "home": "${tree.home}"
                }
            }
        },
        {
            "fakes": {
                "slack": {},
                "discord": {},
                "gemini": {},
                "muse": {}
            }
        },
        {
            "fixture_extras": ["sample_files"]
        },
        {
            "headless_chrome": True
        },
    ])
def test_new_scenario_keys_load(tmp_path: pathlib.Path, fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [{"name": "key scenario", "method": "GET", "path": "/api/auth/status"}], **fields)
  assert scenario.name == "loader"


def test_allowed_baseline_values_rule_loads(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path,
      [{
          "name": "event comparison",
          "method": "GET",
          "path": "/api/auth/status"
      }],
      comparison_rules=[
          {
              "allowed_baseline_values_for_one_event_field":
                  {
                      "event": {
                          "type": "run_finished",
                          "match_fields": {
                              "run_id": "synthetic-run"
                          },
                      },
                      "field": "outcome",
                      "allowed_baseline_values": ["failed", "interrupted"],
                      "required_candidate_value": "interrupted",
                  },
          }
      ],
  )
  assert scenario.comparison_rules[0]["allowed_baseline_values_for_one_event_field"]["field"] == "outcome"


_NUDGE_SELECTOR = {"type": "agent_message", "match_fields": {"from_session_name": "Web reply"}}
_FINISHED_SELECTOR = {"type": "run_finished", "match_fields": {"outcome": "success"}}


def test_either_order_rule_loads(tmp_path: pathlib.Path) -> None:
  rule = {"first": _NUDGE_SELECTOR, "second": _FINISHED_SELECTOR}
  scenario = _load(
      tmp_path, [{
          "name": "adjacent pair",
          "method": "GET",
          "path": "/api/auth/status"
      }],
      comparison_rules=[{
          "either_order_of_adjacent_events": rule
      }])
  loaded = scenario.comparison_rules[0]["either_order_of_adjacent_events"]
  assert loaded["first"] == _NUDGE_SELECTOR
  assert loaded["second"] == _FINISHED_SELECTOR


def test_event_type_stream_rule_loads(tmp_path: pathlib.Path) -> None:
  rule = {"session": "${sessions.atlas}", "event_types": ["improve_iteration_completed", "child_report"]}
  scenario = _load(
      tmp_path, [{
          "name": "event streams",
          "method": "GET",
          "path": "/api/auth/status"
      }],
      comparison_rules=[{
          "compare_event_type_streams": rule
      }])
  assert scenario.comparison_rules[0]["compare_event_type_streams"] == rule


@pytest.mark.parametrize(
    "rule",
    [
        {
            "session": "${sessions.atlas}"
        },
        {
            "session": "${sessions.atlas}",
            "event_types": []
        },
        {
            "session": "${sessions.atlas}",
            "event_types": ["child_report", ""]
        },
        {
            "session": "${sessions.atlas}",
            "event_types": ["child_report", "child_report"]
        },
        {
            "session": "",
            "event_types": ["child_report"]
        },
    ],
)
def test_event_type_stream_rule_rejects_invalid_values(tmp_path: pathlib.Path, rule: dict[str, Any]) -> None:
  with pytest.raises(scenario_file.ScenarioError):
    _load(
        tmp_path, [{
            "name": "event streams",
            "method": "GET",
            "path": "/api/auth/status"
        }],
        comparison_rules=[{
            "compare_event_type_streams": rule
        }])


@pytest.mark.parametrize(
    "rule",
    [
        {
            "first": _NUDGE_SELECTOR
        },
        {
            "first": _NUDGE_SELECTOR,
            "second": {
                "type": "run_finished"
            }
        },
        {
            "first": {
                "type": "agent_message",
                "match_fields": {}
            },
            "second": _FINISHED_SELECTOR
        },
        {
            "first": _NUDGE_SELECTOR,
            "second": {
                "type": "",
                "match_fields": {
                    "outcome": "success"
                }
            }
        },
    ],
)
def test_either_order_rule_rejects_malformed_selectors(tmp_path: pathlib.Path, rule: dict[str, Any]) -> None:
  with pytest.raises(scenario_file.ScenarioError):
    _load(
        tmp_path, [{
            "name": "adjacent pair",
            "method": "GET",
            "path": "/api/auth/status"
        }],
        comparison_rules=[{
            "either_order_of_adjacent_events": rule
        }])


def test_http_status_and_shape_compare_mode_loads(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path, [
          {
              "name": "value varies",
              "method": "POST",
              "path": "/api/sessions/synthetic/runs",
              "expect_status": 422,
              "compare": "status_and_shape",
          }
      ])
  assert scenario.steps[0].compare == "status_and_shape"


@pytest.mark.parametrize("expect_status", [None, 200, 399])
def test_http_status_and_shape_requires_error_status(tmp_path: pathlib.Path, expect_status: int | None) -> None:
  raw_step = {
      "name": "malformed input",
      "method": "POST",
      "path": "/api/sessions/synthetic/runs",
      "compare": "status_and_shape",
  }
  if expect_status is not None:
    raw_step["expect_status"] = expect_status
  with pytest.raises(scenario_file.ScenarioError, match="requires an expect_status of at least 400"):
    _load(tmp_path, [raw_step])


def _process_step(action: str = "stop", **fields: Any) -> dict[str, Any]:
  return {"kind": "process", "name": "synthetic process", "action": action, "process_id": "worker", **fields}


@pytest.mark.parametrize("action", ["restart", "", None, 1])
def test_process_rejects_action_other_than_start_or_stop(tmp_path: pathlib.Path, action: Any) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="process action must be start or stop"):
    _load(tmp_path, [_process_step(action=action)])


@pytest.mark.parametrize("process_id", ["", None, 7, []])
def test_process_rejects_empty_or_non_string_id(tmp_path: pathlib.Path, process_id: Any) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="process_id must be a non-empty string"):
    _load(tmp_path, [_process_step(process_id=process_id)])


@pytest.mark.parametrize("delay_seconds", [-0.1, float("nan"), float("inf"), True, "soon", None])
def test_process_rejects_invalid_delay(tmp_path: pathlib.Path, delay_seconds: Any) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="delay_seconds must be nonnegative"):
    _load(tmp_path, [_process_step(delay_seconds=delay_seconds)])


def test_process_rejects_delay_on_start(tmp_path: pathlib.Path) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="delay_seconds must be nonnegative and only applies to stop"):
    _load(tmp_path, [_process_step("start", delay_seconds=1, capture={"pid": "pid"})])


@pytest.mark.parametrize("capture", [{}, {"pid": "process_id"}, {"first": "pid", "second": "pid"}])
def test_process_start_requires_exactly_one_pid_capture(tmp_path: pathlib.Path, capture: dict[str, str]) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="start requires one capture mapping to 'pid'"):
    _load(tmp_path, [_process_step("start", capture=capture)])


def test_process_rejects_capture_on_stop(tmp_path: pathlib.Path) -> None:
  with pytest.raises(scenario_file.ScenarioError, match="stop cannot capture process fields"):
    _load(tmp_path, [_process_step("stop", capture={"pid": "pid"})])


def test_process_rejects_unknown_key(tmp_path: pathlib.Path) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape("unknown keys ['unexpected']")):
    _load(tmp_path, [_process_step(unexpected="value")])


def test_scenario_cleanup_stops_leftover_synthetic_process_and_reports_it(tmp_path: pathlib.Path) -> None:
  side = parity.Side(
      "baseline", parity.servers.Tree(label="baseline", root=tmp_path), port=18600, strace=parity.servers.STRACE_BIN)
  process = subprocess.Popen(
      [sys.executable, "-c", "import time; time.sleep(60)"],
      stdin=subprocess.DEVNULL,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL)
  side.processes["leftover"] = process

  try:
    cleanup_lines, notes = parity._cleanup_scenario([side])
    assert process.poll() is not None
    assert cleanup_lines == ["baseline synthetic process 'leftover' needed cleanup at scenario end"]
    assert notes == []
  finally:
    if process.poll() is None:
      process.kill()
      process.wait()


# A fake agent's stand-in: it leads its process group and ignores SIGTERM, so only SIGKILL ends it.
_LEADER_IGNORING_SIGTERM = (
    "import signal, time\n"
    "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
    "print('ready', flush=True)\n"
    "time.sleep(60)\n")


@pytest.fixture
def sigterm_leader() -> Iterator[subprocess.Popen]:
  leader = subprocess.Popen(
      [sys.executable, "-c", _LEADER_IGNORING_SIGTERM],
      stdin=subprocess.DEVNULL,
      stdout=subprocess.PIPE,
      stderr=subprocess.DEVNULL,
      text=True,
      start_new_session=True)
  try:
    assert leader.stdout.readline() == "ready\n"
    yield leader
  finally:
    # The leader is the only member of its group, so killing it ends the group whatever the test did.
    if leader.poll() is None:
      leader.kill()
    leader.wait()


def _spawn_record(leader: subprocess.Popen) -> dict[str, Any]:
  _, pid_start = parity.servers.process_identity(leader.pid)
  return {"index": 1, "pid": leader.pid, "pid_start": pid_start, "pgid": os.getpgid(leader.pid)}


def test_fake_group_kill_ends_a_leader_that_ignores_sigterm(sigterm_leader: subprocess.Popen) -> None:
  assert parity._kill_fake_group("baseline", _spawn_record(sigterm_leader)) == []
  assert parity.servers.process_identity(sigterm_leader.pid) is None


def test_fake_group_kill_names_a_group_that_outlives_the_timeout(
    sigterm_leader: subprocess.Popen, monkeypatch: pytest.MonkeyPatch) -> None:
  real_killpg = os.killpg

  def killpg_that_spares_the_group(pgid: int, signum: int) -> None:
    if signum != signal.SIGKILL:
      real_killpg(pgid, signum)

  monkeypatch.setattr(parity.os, "killpg", killpg_that_spares_the_group)
  monkeypatch.setattr(parity, "FAKE_GROUP_EXIT_TIMEOUT_SECONDS", 0.2)

  lines = parity._kill_fake_group("baseline", _spawn_record(sigterm_leader))

  assert len(lines) == 1
  assert f"process group {sigterm_leader.pid} remained" in lines[0]


def test_fake_group_cleanup_does_not_reap_a_recycled_leader_pid(monkeypatch: pytest.MonkeyPatch) -> None:
  leader = subprocess.Popen([sys.executable, "-c", "pass"], start_new_session=True)
  try:
    deadline = time.monotonic() + 10
    identity = parity.servers.process_identity(leader.pid)
    while identity is None or identity[0] != "Z":
      assert time.monotonic() < deadline, "the synthetic child did not become a zombie"
      time.sleep(0.02)
      identity = parity.servers.process_identity(leader.pid)
    record = _spawn_record(leader)
    record["pid_start"] = str(int(record["pid_start"]) + 1)
    monkeypatch.setattr(parity, "FAKE_GROUP_EXIT_TIMEOUT_SECONDS", 0.2)

    lines = parity._kill_fake_group("baseline", record)

    assert len(lines) == 1
    assert "process group" in lines[0] and "remained" in lines[0]
    assert parity.servers.process_identity(leader.pid)[0] == "Z"
  finally:
    with contextlib.suppress(ProcessLookupError):
      os.killpg(leader.pid, signal.SIGKILL)
    leader.wait()


def test_fake_group_kill_passes_a_start_whose_leader_and_group_are_gone() -> None:
  gone = subprocess.Popen([sys.executable, "-c", "pass"], start_new_session=True)
  gone.wait()
  record = {"index": 1, "pid": gone.pid, "pid_start": "0", "pgid": gone.pid}

  assert parity._kill_fake_group("baseline", record) == []


def test_fake_group_kill_signals_only_the_pid_when_the_start_does_not_lead_its_group(
    monkeypatch: pytest.MonkeyPatch) -> None:
  # Neither child calls setsid, so the member shares the test's process group, which must not be signalled.
  member = subprocess.Popen(
      [sys.executable, "-c", "import time; time.sleep(60)"],
      stdin=subprocess.DEVNULL,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL)
  start = subprocess.Popen(
      [sys.executable, "-c", _LEADER_IGNORING_SIGTERM],
      stdin=subprocess.DEVNULL,
      stdout=subprocess.PIPE,
      stderr=subprocess.DEVNULL,
      text=True)
  signalled_groups: list[int] = []
  monkeypatch.setattr(parity.os, "killpg", lambda pgid, signum: signalled_groups.append(pgid))
  try:
    assert start.stdout.readline() == "ready\n"
    record = _spawn_record(start)
    record["pgid"] = os.getpgid(member.pid)
    assert record["pgid"] != record["pid"]

    lines = parity._kill_fake_group("baseline", record)

    assert signalled_groups == []
    assert start.wait(timeout=10) == -signal.SIGKILL
    assert parity.servers.process_identity(start.pid) is None
    assert member.poll() is None
    assert len(lines) == 1
    assert "does not lead its own process group" in lines[0]
  finally:
    for process in (member, start):
      if process.poll() is None:
        process.kill()
      process.wait()


def test_fake_group_kill_passes_an_exited_or_recycled_pid_only_start(monkeypatch: pytest.MonkeyPatch) -> None:
  # Neither child calls setsid, so both share the test's process group. The exited start has no /proc entry; the
  # recycled one is a live process that lacks the recorded pid_start. Neither may get a signal or a line.
  exited = subprocess.Popen([sys.executable, "-c", "pass"])
  exited.wait()
  recycled = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
  signalled: list[str] = []
  monkeypatch.setattr(parity.os, "killpg", lambda pgid, signum: signalled.append(f"killpg {pgid}"))
  monkeypatch.setattr(parity.os, "kill", lambda pid, signum: signalled.append(f"kill {pid}"))
  own_group = os.getpgid(0)
  try:
    exited_record = {"index": 1, "pid": exited.pid, "pid_start": "0", "pgid": own_group}
    recycled_record = {"index": 2, "pid": recycled.pid, "pid_start": "0", "pgid": own_group}

    assert parity._kill_fake_group("baseline", exited_record) == []
    assert parity._kill_fake_group("baseline", recycled_record) == []

    assert signalled == []
  finally:
    monkeypatch.undo()
    if recycled.poll() is None:
      recycled.kill()
    recycled.wait()


def test_scenario_cleanup_kills_a_fake_group_that_ignores_sigterm(
    tmp_path: pathlib.Path, sigterm_leader: subprocess.Popen) -> None:
  side = parity.Side(
      "candidate", parity.servers.Tree(label="candidate", root=tmp_path), port=18600, strace=parity.servers.STRACE_BIN)
  agent_dir = tmp_path / "fake-agent"
  agent_dir.mkdir()
  (agent_dir / "spawns.jsonl").write_text(json.dumps(_spawn_record(sigterm_leader)) + "\n", encoding="utf-8")

  cleanup_lines, notes = parity._cleanup_scenario([side])

  assert parity.servers.process_identity(sigterm_leader.pid) is None
  assert len(cleanup_lines) == 1
  assert "remained alive at scenario end" in cleanup_lines[0]
  assert notes == []


def test_scenario_cleanup_waits_for_a_fake_group_that_has_written_its_exit_marker(tmp_path: pathlib.Path) -> None:
  # A fake agent writes its exit marker and then leaves the process table. The server's shutdown closes its stdin, so
  # that can happen after cleanup has begun. Cleanup must give such a start the grace period, not report or kill it.
  side = parity.Side(
      "candidate", parity.servers.Tree(label="candidate", root=tmp_path), port=18600, strace=parity.servers.STRACE_BIN)
  agent_dir = tmp_path / "fake-agent"
  agent_dir.mkdir()
  leaving = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.5)"], start_new_session=True)
  try:
    (agent_dir / "exit-1").write_text("1", encoding="utf-8")
    (agent_dir / "spawns.jsonl").write_text(json.dumps(_spawn_record(leaving)) + "\n", encoding="utf-8")

    cleanup_lines, notes = parity._cleanup_scenario([side])

    assert cleanup_lines == []
    assert notes == []
  finally:
    leaving.wait(timeout=10)


@pytest.mark.parametrize(
    "step,fields", [
        (
            {
                "name": "raw body file",
                "method": "POST",
                "path": "/api/voice/${sessions.atlas}/confirm",
                "body_file": "audio/not-wave.bin",
            }, {}),
        (
            {
                "name": "multipart body",
                "method": "POST",
                "path": "/api/voice/${sessions.atlas}",
                "multipart": {
                    "audio": {
                        "file": "audio/silence.wav",
                        "content_type": "audio/wav"
                    },
                    "backend": "gemini",
                },
            }, {}),
        (
            {
                "kind": "ws",
                "name": "websocket frame forms",
                "path": "/ws/anything",
                "send": [{
                    "json": {
                        "type": "hello"
                    }
                }, {
                    "text": "plain"
                }, {
                    "binary_file": "audio/silence.pcm"
                }],
                "collect": {
                    "where": {
                        "type": "partial"
                    },
                    "field": "text",
                    "decode": "none"
                },
                "until": {
                    "frames": 1
                },
                "timeout_seconds": 1,
            }, {}),
        (
            {
                "kind": "ws",
                "name": "websocket rejected upgrade",
                "path": "/ws/anything",
                "auth": "none",
                "send": [],
                "until": {
                    "frames": 0
                },
                "timeout_seconds": 1,
                "expect_reject": 403,
            }, {}),
        (
            {
                "kind": "fake",
                "name": "fake inbound message",
                "fake": "slack",
                "action": "socket_event",
                "payload": {
                    "type": "message"
                },
            }, {
                "fakes": {
                    "slack": {}
                }
            }),
    ])
def test_new_http_websocket_and_fake_forms_load(
    tmp_path: pathlib.Path, step: dict[str, Any], fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [step], **fields)
  assert scenario.steps[0].name == step["name"]


@pytest.mark.parametrize(
    "step", [
        {
            "kind": "wait",
            "name": "bad wait",
            "timeout_seconds": 1
        },
        {
            "kind": "wait",
            "name": "missing timeout",
            "until": {
                "at_gate": "ready"
            }
        },
        {
            "kind": "wait",
            "name": "infinite timeout",
            "timeout_seconds": float("inf"),
            "until": {
                "at_gate": "ready"
            }
        },
        {
            "kind": "wait",
            "name": "bad condition",
            "timeout_seconds": 1,
            "until": {
                "unknown": "x"
            }
        },
        {
            "kind": "wait",
            "name": "bad count",
            "timeout_seconds": 1,
            "until": {
                "event": {
                    "session": "s",
                    "type": "run_finished",
                    "count": 0
                },
            }
        },
        {
            "kind": "gate",
            "name": "missing open"
        },
        {
            "kind": "handover",
            "name": "bad state",
            "agent_after_stop": "running"
        },
        {
            "name": "bad defer",
            "method": "GET",
            "path": "/api/auth/status",
            "defer_files": "yes"
        },
        {
            "kind": "wait",
            "name": "unknown wait key",
            "timeout_seconds": 1,
            "until": {
                "at_gate": "ready"
            },
            "open": "ready"
        },
        {
            "name": "multiple HTTP bodies",
            "method": "POST",
            "path": "/api/auth/status",
            "json": {},
            "body_file": "audio/not-wave.bin",
        },
        {
            "name": "missing HTTP body file",
            "method": "POST",
            "path": "/api/auth/status",
            "body_file": "missing.bin",
        },
        {
            "name": "invalid multipart file fields",
            "method": "POST",
            "path": "/api/auth/status",
            "multipart": {
                "audio": {
                    "file": "audio/not-wave.bin"
                }
            },
        },
        {
            "kind": "ws",
            "name": "bad websocket path",
            "path": "/terminal",
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "missing websocket until",
            "path": "/ws/anything",
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "bad websocket send frame",
            "path": "/ws/anything",
            "send": [{
                "json": {},
                "text": "two forms"
            }],
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "collected compare needs collect",
            "path": "/ws/anything",
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
            "compare": {
                "collected_contains": ["ready"]
            },
        },
        {
            "kind": "ws",
            "name": "reject cannot send frames",
            "path": "/ws/anything",
            "send": [{
                "text": "unexpected"
            }],
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
            "expect_reject": 403,
        },
        {
            "kind": "fake",
            "name": "unsupported fake action",
            "fake": "slack",
            "action": "dispatch",
            "payload": {},
        },
        {
            "kind": "fake",
            "name": "missing fake declaration",
            "fake": "slack",
            "action": "socket_event",
            "payload": {},
        },
        {
            "kind": "log_event",
            "name": "log event without a name",
            "timeout_seconds": 1,
        },
        {
            "kind": "log_event",
            "name": "log event with a list field",
            "event": "server_started",
            "fields": {
                "cache_dir": ["/tmp/synthetic"]
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "log_event",
            "name": "log event without a timeout",
            "event": "server_started",
        },
    ])
def test_invalid_step_names_the_step(tmp_path: pathlib.Path, step: dict[str, Any]) -> None:
  fields: dict[str, Any] = {}
  if step.get("kind") in ("gate", "handover") or (step.get("until") and "at_gate" in step["until"]):
    fields.update(fake_agent=_fake_agent(), expect_starts=0)
  if step.get("kind") == "handover":
    fields["directions"] = ["python-to-candidate"]
  with pytest.raises(scenario_file.ScenarioError, match=step["name"]):
    _load(tmp_path, [step], **fields)


@pytest.mark.parametrize(
    "steps,fields,message", [
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
            }, "step 'handover' is a handover, so directions are required"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["python-to-candidate"],
            }, "directions are refused when no handover step is present"),
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["both"],
            }, "directions must be a subset of"),
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": None,
            }, "directions must be a non-empty list"),
    ])
def test_invalid_directions_fail_loading(
    tmp_path: pathlib.Path, steps: list[dict[str, Any]], fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, steps, **fields)


@pytest.mark.parametrize(
    "steps,fields,message", [
        ([{
            "kind": "gate",
            "name": "gate step",
            "open": "ready"
        }], {}, "step 'gate step' needs a fake_agent script"),
        (
            [{
                "kind": "wait",
                "name": "gate wait",
                "timeout_seconds": 1,
                "until": {
                    "at_gate": "ready"
                }
            }], {}, "step 'gate wait' needs a fake_agent script"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "expect_starts": 0
            }, "fake_agent and expect_starts must be provided together"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent()
            }, "fake_agent and expect_starts must be provided together"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": -1
            }, "expect_starts must be a non-negative integer"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": "claude"
                },
                "expect_starts": 0
            }, "fake_agent must contain a starts list"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": []
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1: requires name, steps and exit_code"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [],
                        "exit_code": 256
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1: exit_code must be an integer from 0 to 255"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [{
                            "emit": "text"
                        }],
                        "exit_code": 0
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1 step 1: emit must be an object"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [{
                            "gate": ""
                        }],
                        "exit_code": 0
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1 step 1: emit must be an object or gate must be a non-empty string"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [{
                            "sigterm": "block"
                        }],
                        "exit_code": 0
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1 step 1: emit must be an object or gate must be a non-empty string; sigterm"),
    ])
def test_invalid_script_fails_loading(
    tmp_path: pathlib.Path, steps: list[dict[str, Any]], fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, steps, **fields)


def test_tree_references_use_route_placeholders(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path, [{
          "name": "tree paths",
          "method": "GET",
          "path": "/absolute_filepath${tree.root}/${tree.home}",
      }])
  assert scenario.steps[0].route == "GET /absolute_filepath{tree}/{home}"


@pytest.mark.parametrize(
    "fields,message", [
        (
            {
                "credentials": {
                    "charliebot": {
                        "access_key": "replaced"
                    }
                }
            }, "scenario.yaml.credentials refuses the charliebot key"),
        (
            {
                "config": {
                    "paths": "${sessions.atlas}"
                }
            }, "scenario.yaml.config only accepts ${tree.root} and ${tree.home} references"),
        ({
            "fakes": {
                "twitter": {}
            }
        }, "scenario.yaml.fakes has unknown fake 'twitter'"),
        ({
            "fakes": {
                "slack": {
                    "tokens": 1
                }
            }
        }, "scenario.yaml.fakes.slack: unknown seed keys ['tokens']"),
        ({
            "fixture_extras": ["missing_extra"]
        }, "scenario.yaml.fixture_extras names missing module 'missing_extra'"),
        ({
            "headless_chrome": "yes"
        }, "scenario.yaml.headless_chrome must be a boolean"),
    ])
def test_invalid_scenario_overlay_keys_name_the_file(
    tmp_path: pathlib.Path, fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, [{"name": "http", "method": "GET", "path": "/api/auth/status"}], **fields)


def test_websocket_send_reference_fails_only_its_step(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path, [
          {
              "kind": "ws",
              "name": "send reference",
              "path": "/ws/anything",
              "send": [{
                  "json": {
                      "session": "${sessions.missing}"
                  }
              }],
              "until": {
                  "frames": 0
              },
              "timeout_seconds": 1,
          }
      ])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  message = scenario_file.unresolved_reference(scenario, scenario.steps[0], manifest, {}, "/tree", "/tree/home")
  assert message == "unresolved reference ${sessions.missing}"


def test_unresolved_capture_fails_only_its_step(tmp_path: pathlib.Path) -> None:
  # The send response lacked input_event_id, so the capture is absent on both sides when the wait step runs.
  until = {
      "event":
          {
              "session": "${sessions.atlas}",
              "type": "run_finished",
              "fields": {
                  "input_event_ids": ["${capture.input_id}"]
              },
          }
  }
  send = {
      "name": "send",
      "method": "POST",
      "path": "/api/chat/atlas/message",
      "capture": {
          "input_id": "input_event_id"
      }
  }
  wait = {"kind": "wait", "name": "wait for the run", "timeout_seconds": 1, "until": until}
  read = {"name": "read", "method": "GET", "path": "/api/auth/status"}
  scenario = _load(tmp_path, [send, wait, read])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  messages = [
      scenario_file.unresolved_reference(scenario, step, manifest, {}, "/tree", "/tree/home") for step in scenario.steps
  ]
  # The wait names the send step: its response lacked input_event_id, so that step never recorded the capture.
  assert messages == [None, "unresolved reference ${capture.input_id}: step 'send' should have captured it", None]
  # A reference that names a dict is a malformed file, not an unresolved one: it still stops the run.
  malformed = _load(tmp_path, [{"name": "read", "method": "GET", "path": "/api/${sessions}"}])
  with pytest.raises(scenario_file.ScenarioError, match=re.escape("names a dict")):
    scenario_file.check_references(malformed, manifest)


def test_unresolved_references_in_multipart_and_collect_fail_their_steps(tmp_path: pathlib.Path) -> None:
  capture = {"name": "capture", "method": "GET", "path": "/api/auth/status", "capture": {"input_id": "id"}}
  multipart = {
      "name": "multipart",
      "method": "POST",
      "path": "/api/upload",
      "multipart": {
          "text": "${capture.input_id}"
      },
  }
  collect = {
      "kind": "ws",
      "name": "collect",
      "path": "/ws/test",
      "collect": {
          "where": {
              "text": "${capture.input_id}"
          },
          "field": "text",
          "decode": "none"
      },
      "until": {
          "frames": 0
      },
      "timeout_seconds": 1,
  }
  scenario = _load(tmp_path, [capture, multipart, collect])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  messages = [
      scenario_file.unresolved_reference(scenario, step, manifest, {}, "/tree", "/tree/home") for step in scenario.steps
  ]
  producer = "unresolved reference ${capture.input_id}: step 'capture' should have captured it"
  assert messages == [None, producer, producer]


def test_http_status_and_shape_compare_loads(tmp_path: pathlib.Path) -> None:
  step = {
      "name": "invalid json",
      "method": "POST",
      "path": "/api/sessions/",
      "json": "{bad",
      "expect_status": 422,
      "compare": "status_and_shape",
  }
  scenario = _load(tmp_path, [step])
  assert scenario.steps[0].http_compare == "status_and_shape"


@pytest.mark.parametrize(
    "step,message", [
        (
            {
                "name": "not an error status",
                "method": "GET",
                "path": "/api/auth/status",
                "expect_status": 200,
                "compare": "status_and_shape",
            },
            "compare requires an expect_status of at least 400",
        ),
        (
            {
                "name": "no expected status",
                "method": "GET",
                "path": "/api/auth/status",
                "compare": "status_and_shape",
            },
            "compare requires an expect_status of at least 400",
        ),
        (
            {
                "name": "unknown compare mode",
                "method": "GET",
                "path": "/api/auth/status",
                "expect_status": 422,
                "compare": "frames",
            },
            "compare must be status_and_shape",
        ),
    ])
def test_http_compare_refused_without_an_error_status(
    tmp_path: pathlib.Path, step: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, [step])


def _handover_script() -> dict[str, Any]:
  return {"starts": [{"name": "claude", "steps": [{"gate": "stop"}], "exit_code": 0}]}


@pytest.mark.parametrize(
    "fields,message", [
        ({
            "open_while_stopped": "stop"
        }, "'timeout_seconds' is required with open_while_stopped"),
        ({
            "timeout_seconds": 5
        }, "'timeout_seconds' applies only with open_while_stopped"),
        ({
            "open_while_stopped": ""
        }, "'open_while_stopped' must be a non-empty string"),
        ({
            "open_while_stopped": 3,
            "timeout_seconds": 5
        }, "'open_while_stopped' must be a non-empty string"),
        ({
            "open_while_stopped": "stop",
            "timeout_seconds": 0
        }, "timeout_seconds must be positive"),
        (
            {
                "open_while_stopped": "missing",
                "timeout_seconds": 5
            }, "opens gate 'missing', which no fake_agent start declares"),
    ])
def test_handover_open_while_stopped_validation(tmp_path: pathlib.Path, fields: dict[str, Any], message: str) -> None:
  step = {"kind": "handover", "name": "hand over", "agent_after_stop": "alive", **fields}
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, [step], fake_agent=_handover_script(), expect_starts=1, directions=["python-to-candidate"])


def test_handover_open_while_stopped_keeps_its_gate_and_timeout(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "handover",
      "name": "hand over",
      "agent_after_stop": "exited",
      "open_while_stopped": "stop",
      "timeout_seconds": 5,
  }
  scenario = _load(tmp_path, [step], fake_agent=_handover_script(), expect_starts=1, directions=["python-to-candidate"])
  assert (scenario.steps[0].open_while_stopped, scenario.steps[0].timeout_seconds) == ("stop", 5.0)


def _write(path: pathlib.Path, text: str) -> None:
  path.write_text(text, encoding="utf-8")


def _tree_side(root: pathlib.Path, label: str) -> parity.Side:
  return parity.Side(label, parity.servers.Tree(label=label, root=root), port=18600, strace=parity.servers.STRACE_BIN)


def test_open_while_stopped_fails_a_start_that_stays_at_its_old_gate(tmp_path: pathlib.Path) -> None:
  # Start 1 reached the gate before the stop, so its marker predates the open and does not count as a new gate.
  agent_dir = tmp_path / "fake-agent"
  agent_dir.mkdir()
  _write(agent_dir / "at-stop", "1")

  lines = parity._open_gate_while_stopped([_tree_side(tmp_path, "baseline")], "stop", [{1}], timeout_seconds=0.2)

  assert len(lines) == 1
  assert "baseline fake-agent start 1" in lines[0]
  assert (agent_dir / "open-stop").is_file()


def test_open_while_stopped_settles_on_an_exit_or_a_new_gate_marker(tmp_path: pathlib.Path) -> None:
  agent_dir = tmp_path / "fake-agent"
  agent_dir.mkdir()
  _write(agent_dir / "at-first", "1")
  # Start 1 moves on to a gate of its own after the open; start 2 exits. Both arrive after the wait begins.
  arrivals = [
      threading.Timer(0.1, _write, (agent_dir / "at-second", "1")),
      threading.Timer(0.1, _write, (agent_dir / "exit-2", "0")),
  ]
  for arrival in arrivals:
    arrival.start()
  try:
    lines = parity._open_gate_while_stopped([_tree_side(tmp_path, "candidate")], "first", [{1, 2}], timeout_seconds=5)
  finally:
    for arrival in arrivals:
      arrival.join()

  assert lines == []
  assert (agent_dir / "open-first").is_file()


def test_absent_headless_chrome_exits_naming_the_flag_only_for_a_scenario_that_needs_it(tmp_path: pathlib.Path) -> None:
  request = [{"name": "http", "method": "GET", "path": "/api/auth/status"}]
  parity._check_headless_chrome([_load(tmp_path, request)], tmp_path / "missing" / "headless_shell")
  needing = _load(tmp_path, request, headless_chrome=True)
  with pytest.raises(SystemExit, match=r"missing headless Chrome .*; provide it with --headless-chrome PATH"):
    parity._check_headless_chrome([needing], tmp_path / "missing" / "headless_shell")
