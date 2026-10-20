"""Parity checks of a fake agent's state after a stop: only "terminated" waits for the process to exit."""

import pathlib
import time
import types

import pytest

from rust.parity import parity

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
