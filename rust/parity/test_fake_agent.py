"""The scripted fake agent's own steps, run as the real script in a synthetic home."""

import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Iterator
from typing import Any

import pytest

FAKE_AGENT = pathlib.Path(__file__).resolve().parent / "fake_agent.py"


def _write_script(home: pathlib.Path, steps: list[dict[str, Any]], name: str = "claude") -> pathlib.Path:
  """One scripted start with *steps*; returns the fake-agent directory the script runs in."""
  agent_dir = home / "fake-agent"
  agent_dir.mkdir(parents=True)
  script = {"starts": [{"name": name, "steps": steps, "exit_code": 0}]}
  (agent_dir / "script.json").write_text(json.dumps(script), encoding="utf-8")
  return agent_dir


@pytest.fixture
def start_fake() -> Iterator[Callable[[pathlib.Path], subprocess.Popen]]:
  """Start the fake as the ``claude`` agent of a home; a test that ends kills every fake it started."""
  processes: list[subprocess.Popen] = []

  def start(home: pathlib.Path, name: str = "claude", *args: str) -> subprocess.Popen:
    environment = {"HOME": str(home), "PATH": os.environ["PATH"]}
    process = subprocess.Popen(
        [sys.executable, str(FAKE_AGENT), name, *args],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True)
    processes.append(process)
    return process

  yield start
  for process in processes:
    try:
      os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
      pass
    if process.poll() is None:
      process.wait()


def _wait_for_file(path: pathlib.Path, process: subprocess.Popen) -> None:
  deadline = time.monotonic() + 10
  while not path.exists():
    assert process.poll() is None, f"the fake exited before {path.name}: {process.stderr.read()}"
    assert time.monotonic() < deadline, f"{path.name} did not appear"
    time.sleep(0.02)


def test_sigterm_steps_set_the_disposition_that_the_next_steps_run_under(
    tmp_path: pathlib.Path, start_fake: Callable[[pathlib.Path], subprocess.Popen]) -> None:
  home = tmp_path / "home"
  agent_dir = _write_script(
      home, [{
          "sigterm": "ignore"
      }, {
          "gate": "first"
      }, {
          "sigterm": "default"
      }, {
          "gate": "second"
      }])
  process = start_fake(home)
  _wait_for_file(agent_dir / "at-first", process)
  process.send_signal(signal.SIGTERM)
  time.sleep(0.3)
  assert process.poll() is None, "a SIGTERM under the ignore disposition ended the fake"

  (agent_dir / "open-first").write_text("open\n", encoding="utf-8")
  _wait_for_file(agent_dir / "at-second", process)
  process.send_signal(signal.SIGTERM)
  assert process.wait(timeout=10) == -signal.SIGTERM


def test_gate_poll_exits_two_when_the_fake_agent_directory_is_gone(
    tmp_path: pathlib.Path, start_fake: Callable[[pathlib.Path], subprocess.Popen]) -> None:
  home = tmp_path / "home"
  agent_dir = _write_script(home, [{"gate": "waiting"}])
  process = start_fake(home)
  _wait_for_file(agent_dir / "at-waiting", process)

  shutil.rmtree(agent_dir)

  assert process.wait(timeout=10) == 2
  assert len(process.stderr.read().splitlines()) == 1


def _read_sse_event(response: Any) -> dict[str, Any]:
  line = response.readline()
  assert line.startswith(b"data: "), line
  event = json.loads(line[len(b"data: "):])
  assert response.readline() == b"\n"
  return event


def test_opencode_fake_serves_api_and_uses_script_gates_and_spawn_record(
    tmp_path: pathlib.Path, start_fake: Callable[..., subprocess.Popen]) -> None:
  home = tmp_path / "home"
  agent_dir = _write_script(
      home, [
          {
              "emit":
                  {
                      "type": "session.text.delta",
                      "data": {
                          "assistantMessageID": "assistant-1",
                          "ordinal": 0,
                          "delta": "Synthetic answer."
                      }
                  }
          }, {
              "gate": "opencode-turn"
          }
      ],
      name="opencode")
  process = start_fake(home, "opencode", "serve", "--port", "0")
  assert process.stdout is not None
  listening = process.stdout.readline().strip()
  assert listening.startswith("server listening on http://127.0.0.1:"), listening
  base_url = listening.removeprefix("server listening on ")

  with urllib.request.urlopen(f"{base_url}/api/info", timeout=2) as response:
    assert json.loads(response.read()) == {"version": "2.0.32"}
  stream = urllib.request.urlopen(f"{base_url}/api/event", timeout=2)
  assert _read_sse_event(stream) == {"type": "server.connected", "data": {}}
  request = urllib.request.Request(
      f"{base_url}/api/session",
      data=json.dumps({
          "location": {
              "directory": str(home)
          }
      }).encode("utf-8"),
      headers={"Content-Type": "application/json"},
      method="POST")
  with urllib.request.urlopen(request, timeout=2) as response:
    session_id = json.loads(response.read())["data"]["id"]
  request = urllib.request.Request(
      f"{base_url}/api/session/{session_id}/prompt",
      data=json.dumps({
          "text": "Synthetic manager prompt."
      }).encode("utf-8"),
      headers={"Content-Type": "application/json"},
      method="POST")
  with urllib.request.urlopen(request, timeout=2) as response:
    assert response.status == 200
  assert _read_sse_event(stream) == {"type": "session.execution.started", "data": {"sessionID": session_id}}
  assert _read_sse_event(stream) == {
      "type": "session.text.delta",
      "data": {
          "assistantMessageID": "assistant-1",
          "ordinal": 0,
          "delta": "Synthetic answer.",
          "sessionID": session_id
      }
  }
  _wait_for_file(agent_dir / "at-opencode-turn", process)
  spawn = json.loads((agent_dir / "spawns.jsonl").read_text(encoding="utf-8"))
  assert spawn["name"] == "opencode"
  assert spawn["argv"] == ["opencode", "serve", "--port", "0"]
  assert pathlib.Path(spawn["stdout_path"]).is_file()
  assert pathlib.Path(spawn["stdout_path"]).with_name("agent.raw.cursor").read_text(encoding="utf-8") == "0"
  (agent_dir / "open-opencode-turn").write_text("open\n", encoding="utf-8")
  assert _read_sse_event(stream) == {"type": "session.execution.succeeded", "data": {"sessionID": session_id}}
  stream.close()


def test_opencode_fake_lists_and_unloads_loaded_locations(
    tmp_path: pathlib.Path, start_fake: Callable[..., subprocess.Popen]) -> None:
  home = tmp_path / "home"
  _write_script(home, [], name="opencode")
  process = start_fake(home, "opencode", "serve", "--port", "0")
  assert process.stdout is not None
  listening = process.stdout.readline().strip()
  base_url = listening.removeprefix("server listening on ")

  def request(method: str, path: str, body: dict[str, Any] | None = None) -> tuple[int, bytes]:
    data = json.dumps(body).encode("utf-8") if body is not None else None
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        headers={"Content-Type": "application/json"} if data is not None else {},
        method=method)
    try:
      with urllib.request.urlopen(request, timeout=2) as response:
        return response.status, response.read()
    except urllib.error.HTTPError as error:
      return error.code, error.read()

  status, _ = request("GET", "/api/model?" + urllib.parse.urlencode({"location[directory]": "/locations/z"}))
  assert status == 200
  status, _ = request("POST", "/api/session", {"location": {"directory": "/locations/a"}})
  assert status == 200
  status, _ = request("GET", "/api/shell?" + urllib.parse.urlencode({"location[directory]": "/locations/m"}))
  assert status == 200

  status, payload = request("GET", "/api/debug/location")
  assert status == 200
  assert json.loads(payload) == [
      {
          "directory": "/locations/a"
      },
      {
          "directory": "/locations/m"
      },
      {
          "directory": "/locations/z"
      },
  ]
  status, _ = request(
      "DELETE", "/api/debug/location?" + urllib.parse.urlencode({"location[directory]": "/locations/m"}))
  assert status == 204
  status, payload = request("GET", "/api/debug/location")
  assert status == 200
  assert json.loads(payload) == [{"directory": "/locations/a"}, {"directory": "/locations/z"}]


def test_the_nice_record_follows_the_parents_limits_not_the_spawn(tmp_path: pathlib.Path) -> None:
  """The record point is the end of stdin: a parent that raises nice after the spawn and then sends the prompt, as
  src/runtime/agent_process/base.py:1010 and :1016 do, is the value the fake records."""
  home = tmp_path / "home"
  agent_dir = _write_script(home, [{"gate": "never"}])
  process = subprocess.Popen(
      [sys.executable, str(FAKE_AGENT), "claude"],
      env={
          "HOME": str(home),
          "PATH": os.environ["PATH"]
      },
      stdin=subprocess.PIPE,
      stdout=subprocess.PIPE,
      stderr=subprocess.PIPE,
      start_new_session=True)
  try:
    raised = min(19, os.getpriority(os.PRIO_PROCESS, 0) + 5)
    os.setpriority(os.PRIO_PROCESS, process.pid, raised)
    process.stdin.write(b"a prompt\n")
    process.stdin.close()
    record = agent_dir / "spawns.jsonl"
    _wait_for_file(record, process)
    assert json.loads(record.read_text(encoding="utf-8"))["nice"] == raised
  finally:
    os.killpg(process.pid, signal.SIGKILL)
    process.wait()
