"""Parity checks of a fake agent's state after a stop: only "terminated" waits for the process to exit."""

import pathlib
import threading
import time
import types

import pytest
import websockets.sync.server
from websockets import exceptions
from websockets.frames import Close

from rust.parity import parity
from rust.parity import scenario_file
from rust.parity import servers

_RECORD = {"scripted": True, "index": 0, "pid": 4242, "pid_start": 1}


def _side(root: pathlib.Path) -> types.SimpleNamespace:
  return types.SimpleNamespace(label="python", tree=types.SimpleNamespace(root=root))


def test_a_terminated_agent_that_dies_after_the_check_passes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  died_at = time.monotonic() + 0.5
  monkeypatch.setattr(parity, "_process_alive", lambda record: time.monotonic() < died_at)
  assert parity._check_agent_after_stop(_side(tmp_path), "terminated", [_RECORD], {0}) == []


def test_an_agent_that_stays_alive_fails_at_the_termination_deadline(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(parity, "_process_alive", lambda record: True)
  monkeypatch.setattr(parity, "AGENT_TERMINATION_TIMEOUT_SECONDS", 0.2)
  started = time.monotonic()
  lines = parity._check_agent_after_stop(_side(tmp_path), "terminated", [_RECORD], {0})
  assert len(lines) == 1
  assert time.monotonic() - started >= 0.2


def _ws_step(**overrides: object) -> scenario_file.Step:
  raw = {
      "kind": "ws",
      "name": "closing",
      "path": "/ws/chat",
      "send": [{
          "json": {
              "text": "hi"
          }
      }],
      "until": {
          "frames": 3
      },
      "timeout_seconds": 5
  }
  raw.update(overrides)
  return scenario_file._step(raw, "test")


def test_a_server_that_closes_mid_step_records_the_close_instead_of_raising(tmp_path: pathlib.Path) -> None:
  """_run_step asks _websocket_request for each side's outcome: a close is that outcome, not an exception."""

  def handler(connection: websockets.sync.server.ServerConnection) -> None:
    connection.recv()
    connection.close(code=1002, reason="closing")

  with websockets.sync.server.serve(handler, "127.0.0.1", 0) as ws_server:
    threading.Thread(target=ws_server.serve_forever, daemon=True).start()
    tree = types.SimpleNamespace(root=tmp_path / "baseline", home=tmp_path / "home")
    side = parity.Side("baseline", tree, 1, servers.STRACE_BIN)
    side.server = types.SimpleNamespace(port=ws_server.socket.getsockname()[1])
    status, frames, collected, lines = parity._websocket_request(side, _ws_step(), {})
  assert status == "closed with code 1002"
  assert frames == []
  assert lines == ["baseline WebSocket closed with code 1002 before until condition {'frames': 3}"]


def test_the_close_outcome_names_a_close_that_carries_no_frame() -> None:
  assert parity._close_outcome(exceptions.ConnectionClosed(None, None)) == "closed without a close frame"
  sent = exceptions.ConnectionClosed(None, Close(1001, "going away"))
  assert parity._close_outcome(sent) == "closed with code 1001"
