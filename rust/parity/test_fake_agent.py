"""The scripted fake agent's own steps, run as the real script in a synthetic home."""

import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterator
from typing import Any

import pytest

FAKE_AGENT = pathlib.Path(__file__).resolve().parent / "fake_agent.py"


def _write_script(home: pathlib.Path, steps: list[dict[str, Any]]) -> pathlib.Path:
  """One scripted start with *steps*; returns the fake-agent directory the script runs in."""
  agent_dir = home / "fake-agent"
  agent_dir.mkdir(parents=True)
  script = {"starts": [{"name": "claude", "steps": steps, "exit_code": 0}]}
  (agent_dir / "script.json").write_text(json.dumps(script), encoding="utf-8")
  return agent_dir


@pytest.fixture
def start_fake() -> Iterator[Callable[[pathlib.Path], subprocess.Popen]]:
  """Start the fake as the ``claude`` agent of a home; a test that ends kills every fake it started."""
  processes: list[subprocess.Popen] = []

  def start(home: pathlib.Path) -> subprocess.Popen:
    environment = {"HOME": str(home), "PATH": os.environ["PATH"]}
    process = subprocess.Popen(
        [sys.executable, str(FAKE_AGENT), "claude"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True)
    processes.append(process)
    return process

  yield start
  for process in processes:
    if process.poll() is None:
      process.kill()
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
